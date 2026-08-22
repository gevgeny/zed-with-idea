//! A JetBrains-style git window: the changed files of the active repository, filterable, with a
//! diff of the selected one beside them.
//!
//! Kept in its own crate so it stays mergeable with upstream Zed. It deliberately does not reuse
//! `GitPanel`: the panel's list, selection and staging state are private to `git_ui`, and hosting
//! its renderers would mean hiding the dock to avoid rendering the same entity twice. Everything
//! here goes straight to `project`'s public git API instead.
//!
//! The pieces `git_ui` does expose are reused rather than reimplemented — `git_status_icon` for
//! the per-file status glyph, `git_ui_core::askpass_modal` for credential prompts — so the two
//! stay visually in step.
//!
//! This is a window rather than a modal. A modal is centred, cannot be moved, and dismisses when
//! focus leaves it; a window can sit anywhere, on any monitor, and stay open while you work in
//! the editor beside it. Entities live in the `App` rather than in a window, so the project, the
//! repository and its events reach across exactly as they did before.

use std::collections::{BTreeMap, HashSet};

use anyhow::Context as _;
use editor::{Direction, Editor, EditorSettings};
use file_icons::FileIcons;
use git::{
    repository::RepoPath,
    status::{DiffStat, FileStatus, StageStatus},
};
use git_ui::git_status_icon;
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Focusable, IntoElement,
    ListSizingBehavior, ParentElement, Pixels, Render, SharedString, Styled, Subscription, Task,
    TitlebarOptions, UniformListScrollHandle, WeakEntity, Window, WindowBounds, WindowOptions,
    actions, point, px, uniform_list,
};
use language::Point;
use multi_buffer::MultiBuffer;
use project::{
    Project, ProjectPath,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use settings::Settings as _;
// `git::status::DiffStat` is the number pair; `ui::DiffStat` is the element that shows it.
use ui::{
    ButtonLike, ButtonSize, ButtonStyle, Checkbox, ContextMenu, DiffStat as DiffStatElement,
    Divider, DividerColor, ElevationIndex, Icon, IconButton, IconPosition, Label, ListItem, ListItemSpacing,
    PopoverMenu, PopoverMenuHandle, ScrollAxes, Scrollbars, SplitButton, ToggleState, Tooltip,
    WithScrollbar, h_flex, prelude::*, utils::platform_title_bar_height, v_flex,
};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

actions!(
    plus_git,
    [
        /// Opens the git changes window.
        Toggle
    ]
);

/// What the window opens at the first time. After that the operating system remembers its size
/// and position, which is most of the point of being a window.
const DEFAULT_WINDOW_SIZE: gpui::Size<Pixels> = gpui::Size {
    width: px(1100.),
    height: px(720.),
};

/// The list column will not shrink below this, so the diff cannot squeeze the tree out of view,
/// nor grow past `MAX_LIST_WIDTH` and leave the diff too narrow to read.
const MIN_LIST_WIDTH: Pixels = px(240.);
const MAX_LIST_WIDTH: Pixels = px(700.);

/// What the list column starts at. Dragging the divider replaces it for the life of the window.
const DEFAULT_LIST_WIDTH: Pixels = px(340.);

/// Below this the window stops splitting side by side and stacks the diff under the list instead.
/// Two columns in a narrow window leave neither wide enough to read.
const SIDE_BY_SIDE_MIN_WIDTH: Pixels = px(720.);

/// The stacked split's equivalents: how tall the list is above the diff, and its bounds.
const DEFAULT_LIST_HEIGHT: Pixels = px(280.);
const MIN_LIST_HEIGHT: Pixels = px(120.);
const MAX_LIST_HEIGHT: Pixels = px(900.);

/// How many rows the commit box holds.
const COMMIT_LINES: usize = 5;

/// Carried by a divider drag. Empty because the size is read from the event position, not
/// accumulated — a drag that starts mid-gesture still lands where the pointer is.
struct DividerDrag;

/// The same, for the seam between stacked panes.
struct StackedDividerDrag;

/// Rendered while dragging. Nothing is dragged visually; the divider itself moves.
struct DividerPreview;

impl Render for DividerPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Horizontal step per level of the tree, and where the indent guides sit within a row: the left
/// edge of its icon plus half an `IconSize::Small` (14px), so the line runs down the middle.
const INDENT_PER_DEPTH: f32 = 16.;

const NO_SELECTION_MESSAGE: &str = "Select a file to see its diff";
const LOAD_FAILED_MESSAGE: &str = "Could not load this file's diff";
const BINARY_MESSAGE: &str = "Binary file";

/// Opening a binary file fails with this, from `worktree`. Matched on the message because the
/// error arrives as an opaque `anyhow::Error` with no variant to match on.
fn is_binary_error(error: &anyhow::Error) -> bool {
    error.to_string().contains("Binary files are not supported")
}

pub fn init(cx: &mut App) {
    cx.observe_new(register).detach();
}

fn register(workspace: &mut Workspace, _window: Option<&mut Window>, _: &mut Context<Workspace>) {
    workspace.register_action(move |workspace, _: &Toggle, _window, cx| {
        let Some(multi_workspace) = workspace
            .multi_workspace()
            .and_then(|multi_workspace| multi_workspace.upgrade())
        else {
            return;
        };
        toggle_window(multi_workspace, cx);
    });
}

/// Opens the window for this editor window, or brings the existing one forward. One window per
/// editor window rather than per project: the editor switches between workspaces in place, and the
/// window follows it, so keying on the project would strand a window on the workspace it opened
/// against.
fn toggle_window(multi_workspace: Entity<MultiWorkspace>, cx: &mut App) {
    let existing = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<PlusGitWindow>())
        .find(|window| {
            window
                .read(cx)
                .is_ok_and(|git| git.multi_workspace == multi_workspace)
        });

    if let Some(existing) = existing {
        existing
            .update(cx, |_, window, _| window.activate_window())
            .log_err();
        return;
    }

    // Deferred to get the workspace off the stack, as the settings window does.
    cx.defer(move |cx| {
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Git Changes".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.), px(12.))),
                }),
                focus: true,
                show: true,
                is_movable: true,
                kind: gpui::WindowKind::Normal,
                window_background: cx.theme().window_background_appearance(),
                // Deliberately tiny. Narrower than about 300px the commit row's buttons start to
                // clip, but where that line sits is the user's call, not ours.
                window_min_size: Some(gpui::Size {
                    width: px(150.),
                    height: px(120.),
                }),
                window_bounds: Some(WindowBounds::centered(DEFAULT_WINDOW_SIZE, cx)),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| PlusGitWindow::new(multi_workspace, window, cx));
                // Focusing the window is not the same as focusing something in it; without this
                // the first keypress goes nowhere until the user clicks.
                window.focus(&view.focus_handle(cx), cx);
                view
            },
        )
        .log_err();
    });
}

pub struct PlusGitWindow {
    /// The editor window this one belongs to. Also what tells one git window from another when
    /// the action fires again.
    multi_workspace: Entity<MultiWorkspace>,
    /// Whichever workspace that editor window is currently showing, not the one it opened
    /// against — switching worktrees switches the project, and with it the repository.
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    /// `None` when the project has no git repository, which shows as an empty list.
    repository: Option<Entity<Repository>>,
    /// Held by the tree, not the window: the message box sits outside this handle's subtree, so
    /// enter and the arrows reach the editor rather than moving the selection.
    focus_handle: FocusHandle,
    preview: Entity<DiffPreview>,
    scroll_handle: UniformListScrollHandle,
    /// Width of the list column when the panes sit side by side, and its height when they are
    /// stacked. Two values because a width means nothing once the split turns horizontal.
    list_width: Pixels,
    list_height: Pixels,
    entries: Vec<ChangedFile>,
    /// What the list actually shows: `entries` arranged as a tree, minus anything inside a
    /// collapsed directory. Rebuilt whenever the entries or the collapsed set change.
    rows: Vec<Row>,
    /// Directories the user has collapsed, keyed by their full path. Absent means expanded.
    collapsed_dirs: HashSet<String>,
    /// Whether a chain of directories with nothing else in it is folded into one row
    /// (`assets/themes/gruvbox`) or given a row per level.
    compress_directories: bool,
    selected_index: usize,
    /// Whether the user wants the diff pane at all.
    preview_enabled: bool,
    /// Absent until the repository's commit buffer has opened, which happens in the background
    /// when the window opens.
    commit_editor: Option<Entity<Editor>>,
    /// Set while a commit is in flight, so the button cannot be pressed twice.
    committing: bool,
    /// Set while a push is in flight.
    pushing: bool,
    /// The commit options, all off by default and toggled from the commit button's menu.
    amend: bool,
    signoff: bool,
    skip_hooks: bool,
    commit_menu_handle: PopoverMenuHandle<ContextMenu>,
    _multi_workspace_subscription: Subscription,
    /// Rebuilt whenever the project is swapped, since a git store belongs to one.
    _git_subscription: Subscription,
    _editor_window_subscription: Subscription,
}

impl PlusGitWindow {
    fn new(
        multi_workspace: Entity<MultiWorkspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace = multi_workspace.read(cx).workspace().clone();
        let project = workspace.read(cx).project().clone();
        let repository = project.read(cx).active_repository(cx);
        let preview = cx.new(|cx| DiffPreview::new(project.clone(), window, cx));

        // The editor switches workspaces in place — activating a thread from another worktree
        // does exactly that — and every git thing here belongs to a project. Without this the
        // window keeps showing the diff of the worktree it was opened from.
        let _multi_workspace_subscription = cx.observe_in(
            &multi_workspace,
            window,
            |this, _, window, cx| this.follow_workspace(window, cx),
        );
        let _git_subscription = Self::watch_git_store(&project, window, cx);
        // Everything here belongs to the editor window. When that closes there is nothing left to
        // show, and holding its multi-workspace would keep a whole dead window's state alive.
        let _editor_window_subscription =
            cx.observe_release_in(&multi_workspace, window, |_, _, window, _| {
                window.remove_window();
            });

        let mut this = Self {
            multi_workspace,
            workspace: workspace.downgrade(),
            project,
            repository,
            focus_handle: cx.focus_handle(),
            preview,
            scroll_handle: UniformListScrollHandle::new(),
            list_width: DEFAULT_LIST_WIDTH,
            list_height: DEFAULT_LIST_HEIGHT,
            entries: Vec::new(),
            rows: Vec::new(),
            collapsed_dirs: HashSet::new(),
            compress_directories: true,
            selected_index: 0,
            preview_enabled: true,
            commit_editor: None,
            committing: false,
            pushing: false,
            amend: false,
            signoff: false,
            skip_hooks: false,
            commit_menu_handle: PopoverMenuHandle::default(),
            _multi_workspace_subscription,
            _git_subscription,
            _editor_window_subscription,
        };
        this.reload_entries(cx);
        this.open_commit_buffer(window, cx);
        this
    }

    /// Watches one project's git store.
    ///
    /// A window outlives the repository it was opened against — a branch switch or a change of
    /// active repository would otherwise leave it showing a stale tree forever. A modal never
    /// lived long enough for this to matter.
    fn watch_git_store(
        project: &Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let git_store = project.read(cx).git_store().clone();
        cx.subscribe_in(
            &git_store,
            window,
            |this, _, event: &GitStoreEvent, window, cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_) => {
                    this.follow_active_repository(window, cx)
                }
                GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::StatusesChanged, true) => {
                    this.reload_entries(cx);
                    cx.notify();
                }
                _ => {}
            },
        )
    }

    /// Follows the editor window from one workspace to another.
    ///
    /// Everything shown here — repository, diff, commit message — belongs to a project, so
    /// switching worktrees has to replace all of it rather than leave the previous worktree's
    /// changes on screen.
    fn follow_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.multi_workspace.read(cx).workspace().clone();
        let project = workspace.read(cx).project().clone();
        if project == self.project {
            return;
        }

        self.workspace = workspace.downgrade();
        self.project = project.clone();
        self._git_subscription = Self::watch_git_store(&project, window, cx);
        self.preview = cx.new(|cx| DiffPreview::new(project.clone(), window, cx));
        let repository = project.read(cx).active_repository(cx);
        self.adopt_repository(repository, window, cx);
        cx.notify();
    }

    /// Repoints the window at whichever repository is now active, and reloads from it.
    fn follow_active_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repository = self.project.read(cx).active_repository(cx);
        if repository == self.repository {
            return;
        }
        self.adopt_repository(repository, window, cx);
    }

    /// Shows a repository, discarding everything that described the last one.
    fn adopt_repository(
        &mut self,
        repository: Option<Entity<Repository>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.repository = repository;
        self.collapsed_dirs.clear();
        self.selected_index = 0;
        self.reload_entries(cx);
        self.open_commit_buffer(window, cx);
    }

    /// The commit buffer is the repository's own COMMIT_EDITMSG, shared with the git panel, so a
    /// message typed in one shows up in the other and survives closing the window.
    fn open_commit_buffer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_editor = None;
        let Some(repository) = self.repository.clone() else {
            return;
        };
        let languages = self.project.read(cx).languages().clone();
        let buffer_store = self.project.read(cx).buffer_store().clone();
        let open = repository.update(cx, |repository, cx| {
            repository.open_commit_buffer(Some(languages), buffer_store, cx)
        });

        cx.spawn_in(window, async move |this, cx| {
            let buffer = open.await?;
            this.update_in(cx, |this, window, cx| {
                this.commit_editor = Some(cx.new(|cx| commit_message_editor(buffer, window, cx)));
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected_index = index.min(self.rows.len().saturating_sub(1));
        self.scroll_handle
            .scroll_to_item(self.selected_index, gpui::ScrollStrategy::Top);
        self.update_preview(cx);
        cx.notify();
    }

    /// Feeds the preview whatever the selection now points at. A directory has no diff, so the
    /// pane keeps showing the last file rather than blanking as you move through the tree.
    fn update_preview(&mut self, cx: &mut Context<Self>) {
        // A file that git has never seen is one block of additions, so there is nothing to step
        // between and the diff pane hides its navigation.
        let has_hunks = self
            .selected_entry()
            .is_some_and(|entry| !entry.status.is_created() && !entry.status.is_untracked());
        match self
            .selected_entry()
            .and_then(|entry| entry.project_path.clone())
        {
            Some(path) => self
                .preview
                .update(cx, |preview, cx| preview.show(path, has_hunks, cx)),
            // Only when there is nothing to show at all. A directory row keeps the last file up,
            // rather than blanking the pane as the selection passes over it.
            None if self.rows.is_empty() => {
                self.preview.update(cx, |preview, cx| preview.clear(cx))
            }
            None => {}
        }
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        self.select((self.selected_index + 1) % self.rows.len(), cx);
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.rows.is_empty() {
            return;
        }
        let previous = self
            .selected_index
            .checked_sub(1)
            .unwrap_or(self.rows.len() - 1);
        self.select(previous, cx);
    }

    fn select_first(
        &mut self,
        _: &menu::SelectFirst,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select(0, cx);
    }

    fn select_last(&mut self, _: &menu::SelectLast, _window: &mut Window, cx: &mut Context<Self>) {
        self.select(self.rows.len().saturating_sub(1), cx);
    }

    /// Enter opens a file in the workspace behind, and collapses or expands a directory. The
    /// window stays open either way: unlike a modal, it is not in the way of the editor.
    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.rows.get(self.selected_index),
            Some(Row::Directory { .. })
        ) {
            self.toggle_selected_directory(cx);
        } else {
            self.open_selected(window, cx);
        }
    }

    /// Escape closes the window.
    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_index;
        uniform_list(
            "plus-git-entries",
            self.rows.len(),
            cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                range
                    .map(|ix| {
                        this.render_row(ix, ix == selected, window, cx)
                            .map(IntoElement::into_any_element)
                            .unwrap_or_else(|| div().into_any_element())
                    })
                    .collect()
            }),
        )
        .with_decoration(
            ui::indent_guides(px(INDENT_PER_DEPTH), ui::IndentGuideColors::panel(cx))
                .with_left_offset(
                    ui::LIST_ITEM_INDENT_GUIDE_LEFT_OFFSET + px(INDENT_PER_DEPTH / 2.),
                )
                .with_compute_indents_fn(cx.entity(), |this, range, _window, _cx| {
                    range
                        .filter_map(|index| match this.rows.get(index)? {
                            Row::Directory { depth, .. } | Row::File { depth, .. } => Some(*depth),
                        })
                        .collect()
                }),
        )
        // `Auto`, not the default `Infer`: the list fills the height its column gives it rather
        // than being measured from its own content, which comes out as nothing inside a flex.
        .with_sizing_behavior(ListSizingBehavior::Auto)
        .size_full()
        .py_1()
        .track_scroll(&self.scroll_handle)
    }

    /// The draggable seam between the two panes. A hairline with a resize cursor, swallowing
    /// mouse events so a drag that strays over the list does not select a row.
    fn render_divider(&self, side_by_side: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("plus-git-divider")
            .flex_none()
            .bg(cx.theme().colors().border)
            .block_mouse_except_scroll()
            .map(|this| {
                if side_by_side {
                    this.w_px()
                        .h_full()
                        .cursor_col_resize()
                        .on_drag(DividerDrag, |_, _, _, cx| cx.new(|_| DividerPreview))
                        .on_drag_move::<DividerDrag>(cx.listener(
                            |this, event: &gpui::DragMoveEvent<DividerDrag>, _window, cx| {
                                // Straight from the pointer rather than accumulated from a start
                                // offset, so the pane cannot drift away from the cursor.
                                this.list_width =
                                    event.event.position.x.clamp(MIN_LIST_WIDTH, MAX_LIST_WIDTH);
                                cx.notify();
                            },
                        ))
                } else {
                    this.h_px()
                        .w_full()
                        .cursor_row_resize()
                        .on_drag(StackedDividerDrag, |_, _, _, cx| cx.new(|_| DividerPreview))
                        .on_drag_move::<StackedDividerDrag>(cx.listener(
                            |this, event: &gpui::DragMoveEvent<StackedDividerDrag>, _window, cx| {
                                this.list_height = event
                                    .event
                                    .position
                                    .y
                                    .clamp(MIN_LIST_HEIGHT, MAX_LIST_HEIGHT);
                                cx.notify();
                            },
                        ))
                }
            })
    }

    /// The list column's own header: what the repository totals to, and the two controls that
    /// act on the whole list. Built like the diff pane's header so the two line up.
    fn render_header(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .child(
                h_flex()
                    // The same height the editor window's title bar computes for itself, so the
                    // two line up when the windows sit side by side.
                    .h(platform_title_bar_height(window))
                    .bg(cx.theme().colors().title_bar_background)
                    .px_2p5()
                    .gap_1()
                    .flex_none()
                    .overflow_hidden()
                    .justify_end()
                    .children(self.render_header_controls(window, cx)),
            )
            // `Border`, not the default `BorderVariant`: over the panel background the faded
            // variant barely reads, where the editor window's own header seam is plainly there.
            .child(Divider::horizontal().color(DividerColor::Border))
    }

    fn reload_entries(&mut self, cx: &mut App) {
        let Some(repository) = self.repository.clone() else {
            self.entries.clear();
            return;
        };
        let project = self.project.read(cx);
        let path_style = project.path_style(cx);
        let repository = repository.read(cx);

        self.entries = repository
            .cached_status()
            .map(|entry| {
                let display_path = entry.repo_path.display(path_style).to_string();
                let file_name = entry
                    .repo_path
                    .file_name()
                    .map(str::to_owned)
                    .unwrap_or_else(|| display_path.clone());
                let project_path = repository.repo_path_to_project_path(&entry.repo_path, cx);
                ChangedFile {
                    project_path,
                    repo_path: entry.repo_path,
                    status: entry.status,
                    diff_stat: entry.diff_stat,
                    display_path,
                    file_name,
                }
            })
            .collect();
        self.rebuild_rows();
    }

    /// Arranges the entries into `rows`: one tree holding tracked and untracked files together,
    /// so a directory containing both shows them side by side rather than twice under two
    /// headings.
    fn rebuild_rows(&mut self) {
        // Sorted by path so rows come out alphabetically: `cached_status` order means nothing
        // once the paths are arranged as a tree.
        let mut order = (0..self.entries.len()).collect::<Vec<_>>();
        order.sort_by(|a, b| match (self.entries.get(*a), self.entries.get(*b)) {
            (Some(a), Some(b)) => a.display_path.cmp(&b.display_path),
            _ => std::cmp::Ordering::Equal,
        });

        let mut root = TreeNode::default();
        for &index in &order {
            let Some(entry) = self.entries.get(index) else {
                continue;
            };
            root.insert(&entry.display_path.split('/').collect::<Vec<_>>(), index);
        }

        let compress = self.compress_directories;
        let collapsed_dirs = std::mem::take(&mut self.collapsed_dirs);
        let mut rows = Vec::new();
        root.flatten(
            "",
            0,
            compress,
            &|path: &str| collapsed_dirs.contains(path),
            &mut rows,
        );

        self.collapsed_dirs = collapsed_dirs;
        self.rows = rows;
        self.selected_index = self.selected_index.min(self.rows.len().saturating_sub(1));
    }

    /// Every file under `directory`, which is what a directory's checkbox stages or unstages.
    fn files_under<'a>(&'a self, directory: &str) -> impl Iterator<Item = &'a ChangedFile> {
        let prefix = format!("{directory}/");
        self.entries
            .iter()
            .filter(move |entry| entry.display_path.starts_with(&prefix))
    }

    /// A directory is staged when every file under it is, unstaged when none are, and mixed
    /// otherwise — which is what the checkbox's indeterminate state shows.
    fn directory_stage_status(&self, directory: &str) -> StageStatus {
        let (staged, total) = self
            .files_under(directory)
            .fold((0, 0), |(staged, total), entry| {
                (
                    staged + entry.status.staging().has_staged() as usize,
                    total + 1,
                )
            });
        match staged {
            0 => StageStatus::Unstaged,
            _ if staged == total => StageStatus::Staged,
            _ => StageStatus::PartiallyStaged,
        }
    }

    fn selected_entry(&self) -> Option<&ChangedFile> {
        match self.rows.get(self.selected_index)? {
            Row::File { entry, .. } => self.entries.get(*entry),
            _ => None,
        }
    }

    /// Collapses or expands the selected directory. Does nothing while a query is active, where
    /// the tree is shown fully expanded regardless.
    fn toggle_selected_directory(&mut self, cx: &mut Context<Self>) {
        let Some(Row::Directory { path, .. }) = self.rows.get(self.selected_index) else {
            return;
        };
        let path = path.clone();
        if !self.collapsed_dirs.remove(&path) {
            self.collapsed_dirs.insert(path);
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// Whether any directory is currently expanded, which decides what the collapse button does.
    fn any_directory_expanded(&self) -> bool {
        self.rows.iter().any(|row| {
            matches!(
                row,
                Row::Directory {
                    collapsed: false,
                    ..
                }
            )
        })
    }

    /// Collapses every directory, or expands every directory when none are left expanded, so one
    /// button covers both directions.
    fn toggle_all_directories(&mut self, cx: &mut Context<Self>) {
        self.collapsed_dirs = if self.any_directory_expanded() {
            // Taken from the paths rather than the visible rows: collapsing an outer directory
            // hides the inner ones, so reading rows would only ever reach the top level.
            self.entries
                .iter()
                .flat_map(|entry| {
                    entry
                        .display_path
                        .match_indices('/')
                        .map(|(end, _)| entry.display_path[..end].to_string())
                })
                .collect()
        } else {
            HashSet::new()
        };
        self.rebuild_rows();
        cx.notify();
    }

    /// Lines added and removed across every changed file, as the git panel shows above its list.
    fn total_diff_stat(&self) -> Option<DiffStat> {
        let total = self
            .entries
            .iter()
            .filter_map(|entry| entry.diff_stat)
            .fold(DiffStat::default(), |total, stat| DiffStat {
                added: total.added + stat.added,
                deleted: total.deleted + stat.deleted,
            });
        (total.added + total.deleted > 0).then_some(total)
    }

    /// Stages or unstages `paths`, which is what the checkboxes do: it moves files in and out of
    /// what the next commit will contain, the same as `git add` and `git restore --staged`.
    ///
    /// The list is not updated here. Staging emits `RepositoryEvent::StatusesChanged`, which the
    /// modal is subscribed to, so the checkbox settles when the repository confirms it.
    fn set_staged(&mut self, paths: Vec<RepoPath>, staged: bool, cx: &mut Context<Self>) {
        let Some(repository) = self.repository.clone() else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let task = repository.update(cx, |repository, cx| {
            if staged {
                repository.stage_entries(paths, cx)
            } else {
                repository.unstage_entries(paths, cx)
            }
        });
        cx.spawn(async move |_, _| task.await)
            .detach_and_log_err(cx);
    }

    /// Stages everything, or unstages everything once it all is, so one button covers both.
    /// Deliberately ignores the filter: "all" that meant "all the ones you can see" would be a
    /// trap when the list is narrowed.
    fn toggle_stage_all(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.repository.clone() else {
            return;
        };
        let staging = !self
            .entries
            .iter()
            .all(|entry| entry.status.staging().has_staged() || entry.status.is_untracked());
        let task = repository.update(cx, |repository, cx| {
            if staging {
                repository.stage_all(cx)
            } else {
                repository.unstage_all(cx)
            }
        });
        cx.spawn(async move |_, _| task.await)
            .detach_and_log_err(cx);
    }

    fn toggle_staged_for_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let (paths, staged) = match self.rows.get(row) {
            Some(Row::File { entry, .. }) => {
                let Some(entry) = self.entries.get(*entry) else {
                    return;
                };
                (
                    vec![entry.repo_path.clone()],
                    !entry.status.staging().has_staged(),
                )
            }
            Some(Row::Directory { path, .. }) => {
                let path = path.clone();
                let staged = !self.directory_stage_status(&path).has_staged();
                let paths = self
                    .files_under(&path)
                    .map(|entry| entry.repo_path.clone())
                    .collect();
                (paths, staged)
            }
            None => return,
        };
        self.set_staged(paths, staged, cx);
    }

    /// The message as typed. `None` when the editor has not opened yet or holds only whitespace,
    /// which is also what disables the commit button.
    fn commit_message(&self, cx: &App) -> Option<String> {
        let message = self.commit_editor.as_ref()?.read(cx).text(cx);
        (!message.trim().is_empty()).then_some(message)
    }

    /// Commits what is staged. With nothing staged, git would commit nothing at all, so the
    /// tracked changes are staged first — matching what the git panel does, and what `git commit
    /// -a` would do. Untracked files are never swept in.
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(repository), Some(message)) = (self.repository.clone(), self.commit_message(cx))
        else {
            return;
        };
        if self.committing {
            return;
        }

        let nothing_staged = !self
            .entries
            .iter()
            .any(|entry| entry.status.staging().has_staged());
        let unstaged_tracked = nothing_staged
            .then(|| {
                self.entries
                    .iter()
                    .filter(|entry| !entry.status.is_untracked())
                    .map(|entry| entry.repo_path.clone())
                    .collect::<Vec<_>>()
            })
            .filter(|paths| !paths.is_empty());

        let askpass = self.askpass_delegate("git commit", cx);
        let options = git::repository::CommitOptions {
            amend: self.amend,
            signoff: self.signoff,
            allow_empty: false,
            no_verify: self.skip_hooks,
        };

        self.committing = true;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            if let Some(paths) = unstaged_tracked {
                repository
                    .update(cx, |repository, cx| repository.stage_entries(paths, cx))
                    .await?;
            }
            let committed = repository.update(cx, |repository, cx| {
                repository.commit(message.into(), None, options, askpass, cx)
            });
            let result = committed.await?;

            this.update_in(cx, |this, window, cx| {
                this.committing = false;
                if result.is_ok()
                    && let Some(editor) = this.commit_editor.clone()
                {
                    editor.update(cx, |editor, cx| editor.clear(window, cx));
                }
                cx.notify();
            })?;
            result
        })
        .detach_and_log_err(cx);
    }

    /// Pushes the current branch to the first remote that carries it, setting the upstream when
    /// there is none. The git panel asks which remote when there are several; this takes the
    /// first, which is the same choice in every repository with one remote.
    fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository.clone() else {
            return;
        };
        let Some(branch) = repository.read(cx).branch.clone() else {
            return;
        };
        if self.pushing {
            return;
        }

        let options = match &branch.upstream {
            Some(upstream)
                if !matches!(upstream.tracking, git::repository::UpstreamTracking::Gone) =>
            {
                None
            }
            _ => Some(git::repository::PushOptions::SetUpstream),
        };
        let branch_name = SharedString::from(branch.name().to_string());
        let remotes = repository.update(cx, |repository, _| {
            repository.get_remotes(Some(branch.name().to_string()), true)
        });
        let askpass = self.askpass_delegate("git push", cx);

        self.pushing = true;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let pushed = async {
                let remote = remotes
                    .await??
                    .into_iter()
                    .next()
                    .context("no remote to push to; add one to publish this branch")?;
                repository
                    .update(cx, |repository, cx| {
                        repository.push(
                            branch_name.clone(),
                            branch_name,
                            remote.name,
                            options,
                            askpass,
                            cx,
                        )
                    })
                    .await??;
                anyhow::Ok(())
            }
            .await;

            this.update(cx, |this, cx| {
                this.pushing = false;
                cx.notify();
            })?;
            pushed
        })
        .detach_and_log_err(cx);
    }

    /// Git may need a passphrase or credentials part-way through. The prompt is a workspace
    /// modal, so it appears over the editor window rather than this one; answering it there lets
    /// the commit finish.
    fn askpass_delegate(
        &self,
        operation: &'static str,
        cx: &mut Context<Self>,
    ) -> askpass::AskPassDelegate {
        let workspace = self.workspace.clone();
        // The editor window's, not ours: a workspace modal is rendered by the window that renders
        // the workspace, so prompting through this one leaves a push waiting on a prompt that is
        // never drawn.
        let window = self.multi_workspace.read(cx).window(cx);
        askpass::AskPassDelegate::new(&mut cx.to_async(), move |prompt, tx, cx| {
            let Some(window) = window else {
                return;
            };
            window
                .update(cx, |_, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.toggle_modal(window, cx, |window, cx| {
                                git_ui_core::askpass_modal::AskPassModal::new(
                                    operation.into(),
                                    prompt.into(),
                                    tx,
                                    window,
                                    cx,
                                )
                            });
                        })
                        .ok();
                })
                .ok();
        })
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self
            .selected_entry()
            .and_then(|entry| entry.project_path.clone())
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(editor_window) = self.multi_workspace.read(cx).window(cx) else {
            return;
        };

        cx.spawn_in(window, async move |_, cx| {
            let open = editor_window.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.open_path(path, None, true, window, cx)
                })
            })?;
            open.await?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// The commit button's menu: the options that change what the next commit does. They map
    /// straight onto `CommitOptions`, so nothing here is stored beyond the three flags.
    fn render_commit_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_previous_commit = self
            .repository
            .as_ref()
            .is_some_and(|repository| repository.read(cx).head_commit.is_some());
        let (amend, signoff, skip_hooks) = (self.amend, self.signoff, self.skip_hooks);
        let this = cx.entity();

        PopoverMenu::new("plus-git-commit-menu")
            .trigger(split_button_chevron(
                "plus-git-commit-menu-trigger",
                self.commit_menu_handle.is_deployed(),
            ))
            .with_handle(self.commit_menu_handle.clone())
            .anchor(gpui::Anchor::BottomRight)
            .menu(move |window, cx| {
                let toggle = |_unused: &Entity<PlusGitWindow>, apply: fn(&mut PlusGitWindow)| {
                    let this = this.clone();
                    move |_: &mut Window, cx: &mut App| {
                        this.update(cx, |this, cx| {
                            apply(this);
                            cx.notify();
                        });
                    }
                };
                Some(ContextMenu::build(window, cx, |menu, _, _| {
                    menu.when(has_previous_commit, |menu| {
                        menu.toggleable_entry(
                            "Amend",
                            amend,
                            IconPosition::Start,
                            None,
                            toggle(&this, |this| this.amend = !this.amend),
                        )
                    })
                    .toggleable_entry(
                        "Signoff",
                        signoff,
                        IconPosition::Start,
                        None,
                        toggle(&this, |this| this.signoff = !this.signoff),
                    )
                    .toggleable_entry(
                        "Skip hooks",
                        skip_hooks,
                        IconPosition::Start,
                        None,
                        toggle(&this, |this| this.skip_hooks = !this.skip_hooks),
                    )
                }))
            })
    }

    /// Turns the filter row into the repository's header: which branch, how far ahead or behind,
    /// and the two controls that act on the whole list.
    fn render_header_controls(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let collapse_all = self.any_directory_expanded();
        let compress_directories = self.compress_directories;
        let preview_enabled = self.preview_enabled;
        let this = cx.entity();

        Some(
            h_flex()
                .flex_none()
                .gap_1p5()
                .children(self.total_diff_stat().map(|_| {
                    Label::new("Diff:")
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                }))
                .children(self.total_diff_stat().map(|total| {
                    DiffStatElement::new(
                        "plus-git-total-diff-stat",
                        total.added as usize,
                        total.deleted as usize,
                    )
                }))
                .child(
                    IconButton::new("plus-git-compress-directories", IconName::ListTree)
                        .icon_size(IconSize::Small)
                        .toggle_state(!compress_directories)
                        .tooltip(Tooltip::text(if compress_directories {
                            "Show Every Directory"
                        } else {
                            "Fold Single-Child Directories"
                        }))
                        .on_click({
                            let this = this.clone();
                            move |_, _window, cx| {
                                this.update(cx, |this, cx| {
                                    this.compress_directories = !this.compress_directories;
                                    this.rebuild_rows();
                                    cx.notify();
                                });
                            }
                        }),
                )
                .child(
                    IconButton::new(
                        "plus-git-collapse-all",
                        if collapse_all {
                            IconName::ChevronUpDown
                        } else {
                            IconName::ChevronDownUp
                        },
                    )
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(if collapse_all {
                        "Collapse All"
                    } else {
                        "Expand All"
                    }))
                    .on_click({
                        let this = this.clone();
                        move |_, _window, cx| {
                            this.update(cx, |this, cx| {
                                this.toggle_all_directories(cx);
                            });
                        }
                    }),
                )
                .child(
                    IconButton::new("plus-git-preview-toggle", IconName::Eye)
                        .icon_size(IconSize::Small)
                        .toggle_state(preview_enabled)
                        .tooltip(Tooltip::text("Toggle Preview"))
                        .on_click(move |_, _window, cx| {
                            this.update(cx, |this, cx| {
                                this.preview_enabled = !preview_enabled;
                                // `sync_preview_layout` applies it on the next render.
                                cx.notify();
                            });
                        }),
                )
                .into_any_element(),
        )
    }

    /// The commit section: the message editor and the actions that operate on the whole
    /// repository. It sits at the bottom of the list column, under the tree and beside the
    /// preview, and is rendered whether or not anything matches the filter.
    fn render_commit_section(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let staged = self
            .entries
            .iter()
            .filter(|entry| entry.status.staging().has_staged())
            .count();
        let all_staged = staged > 0 && staged == self.entries.len();
        let can_commit = !self.committing && self.commit_message(cx).is_some();
        let can_push = !self.pushing
            && self
                .repository
                .as_ref()
                .is_some_and(|repository| repository.read(cx).branch.is_some());
        let commit_label = if self.amend { "Amend" } else { "Commit" };
        let commit_menu = self.render_commit_menu(cx).into_any_element();
        let this = cx.entity();

        Some(
            v_flex()
                .w_full()
                .flex_none()
                .gap_1p5()
                .pb_2()
                .children(self.commit_editor.clone().map(|editor| {
                    div()
                        .key_context("CommitEditor")
                        .w_full()
                        .px_2()
                        .pt_2()
                        .bg(cx.theme().colors().editor_background)
                        .child(editor)
                }))
                .child(
                    h_flex()
                        .w_full()
                        .flex_none()
                        .gap_1()
                        .px_2()
                        .child(div().flex_1())
                        .child(
                            action_button(
                                ButtonLike::new("plus-git-stage-all"),
                                if all_staged {
                                    "Unstage all"
                                } else {
                                    "Stage all"
                                },
                                !self.entries.is_empty(),
                                ButtonStyle::Outlined,
                            )
                            .on_click({
                                let this = this.clone();
                                move |_, _window, cx| {
                                    this.update(cx, |this, cx| {
                                        this.toggle_stage_all(cx);
                                    });
                                }
                            }),
                        )
                        .child(
                            SplitButton::new(
                                action_button(
                                    ButtonLike::new_rounded_left("plus-git-commit"),
                                    commit_label,
                                    can_commit,
                                    ButtonStyle::Transparent,
                                )
                                .on_click({
                                    let this = this.clone();
                                    move |_, window, cx| {
                                        this.update(cx, |this, cx| {
                                            this.commit(window, cx);
                                        });
                                    }
                                }),
                                commit_menu,
                            )
                            .style(ui::SplitButtonStyle::Outlined),
                        )
                        .child(
                            action_button(
                                ButtonLike::new("plus-git-push"),
                                "Push",
                                can_push,
                                ButtonStyle::Outlined,
                            )
                            .on_click(move |_, window, cx| {
                                this.update(cx, |this, cx| {
                                    this.push(window, cx);
                                });
                            }),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_row(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<ListItem> {
        let row = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse);

        // Staging is the checkbox's own job, so it stops the click before the row treats it as a
        // selection or an open.
        let stage_checkbox = |state: StageStatus, cx: &mut Context<Self>| {
            let this = cx.entity();
            Checkbox::new(("stage", ix), stage_toggle_state(state))
                .fill()
                .elevation(ElevationIndex::Surface)
                .on_click(move |_, window, cx| {
                    this.update(cx, |this, cx| {
                        this.toggle_staged_for_row(ix, cx);
                        cx.notify();
                    });
                    cx.stop_propagation();
                    window.refresh();
                })
        };

        match self.rows.get(ix)? {
            Row::Directory {
                path,
                label,
                depth,
                collapsed,
            } => {
                let checkbox = stage_checkbox(self.directory_stage_status(path), cx);
                let this = cx.entity();
                Some(
                    row.toggle_state(selected)
                        .on_click(move |_, window, cx| {
                            this.update(cx, |this, cx| {
                                window.focus(&this.focus_handle, cx);
                                this.select(ix, cx);
                                this.toggle_selected_directory(cx);
                            });
                            cx.stop_propagation();
                        })
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap_1p5()
                                .justify_between()
                                .child(
                                    h_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_2()
                                        .child(indent(*depth))
                                        .child(folder_icon(*collapsed, path, cx))
                                        .child(
                                            Label::new(label.clone())
                                                .color(Color::Muted)
                                                .truncate(),
                                        ),
                                )
                                .child(checkbox),
                        ),
                )
            }

            Row::File { entry, depth } => {
                let entry = self.entries.get(*entry)?;
                let stat = entry.diff_stat.filter(|stat| stat.added + stat.deleted > 0);
                let checkbox = stage_checkbox(entry.status.staging(), cx);

                // A single click selects, a double click opens.
                let this = cx.entity();
                Some(
                    row.toggle_state(selected)
                        .on_click(move |event: &ClickEvent, window, cx| {
                            let opening = event.click_count() >= 2;
                            this.update(cx, |this, cx| {
                                // Clicking the preview leaves focus in its editor, where the
                                // arrow keys move a cursor. Returning to the list takes it back.
                                window.focus(&this.focus_handle, cx);

                                this.select(ix, cx);
                                if opening {
                                    this.open_selected(window, cx);
                                }
                                cx.notify();
                            });
                            cx.stop_propagation();
                        })
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap_2()
                                .justify_between()
                                .child(
                                    h_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_2()
                                        .child(indent(*depth))
                                        // The git status, not the file type: which files changed
                                        // and how is the whole point of this list.
                                        .child(git_status_icon(entry.status))
                                        .child(
                                            Label::new(entry.file_name.clone())
                                                .single_line()
                                                .truncate(),
                                        ),
                                )
                                .child(
                                    h_flex()
                                        .flex_none()
                                        .gap_1p5()
                                        .children(stat.map(|stat| {
                                            DiffStatElement::new(
                                                ix,
                                                stat.added as usize,
                                                stat.deleted as usize,
                                            )
                                        }))
                                        .child(checkbox),
                                ),
                        ),
                )
            }
        }
    }
}

impl Render for PlusGitWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Side by side while there is room for both, stacked when there is not — the same call a
        // media query makes, decided from the viewport each frame. Purely about width: whether
        // there is a diff pane at all is a separate question, and folding the two together left
        // the list pinned to a stacked height with nothing beneath it.
        let side_by_side = window.viewport_size().width >= SIDE_BY_SIDE_MIN_WIDTH;
        let split = self.preview_enabled;

        let header = self.render_header(window, cx).into_any_element();
        let list = self.render_list(cx).into_any_element();
        let divider = self.render_divider(side_by_side, cx).into_any_element();
        // Stacked with a diff pane, the window's height is already split between tree and diff;
        // a message box and its buttons on top of that leaves too little of either. Toggling the
        // preview off brings them back, which is also how the tree gets the whole window.
        let commit_section = (side_by_side || !split)
            .then(|| self.render_commit_section(window, cx))
            .flatten()
            .map(IntoElement::into_any_element);

        let tree = div()
            .id("plus-git-tree")
            .track_focus(&self.focus_handle)
            .flex_1()
            .min_h_0()
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .child(list)
            .custom_scrollbars(
                Scrollbars::new(ScrollAxes::Vertical).tracked_scroll_handle(&self.scroll_handle),
                window,
                cx,
            );

        let list_column = v_flex()
            // The settings window's sidebar colour: this column is a list beside content, not
            // content itself.
            .bg(cx.theme().colors().panel_background)
            // Sized against the pane it shares the window with, and only then. On its own it
            // fills the window.
            .map(|this| match (split, side_by_side) {
                (false, _) => this.size_full().flex_1().min_h_0(),
                // Capped against the window as well as its own bounds: the window can be dragged
                // smaller than the list's stored size.
                (true, true) => {
                    let width = self.list_width.min(window.viewport_size().width * 0.7);
                    this.h_full().w(width).flex_none()
                }
                (true, false) => {
                    let height = self.list_height.min(window.viewport_size().height * 0.7);
                    this.w_full().h(height).flex_none()
                }
            })
            .child(header)
            .child(tree)
            .children(commit_section);

        let preview = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(self.preview.clone());

        v_flex()
            .key_context("PlusGit")
            .relative()
            .size_full()
            .bg(cx.theme().colors().background)
            .on_action(cx.listener(Self::cancel))
            // `CommitEditor` binds cmd-enter to this, which is where the message box sends it.
            .on_action(cx.listener(|this, _: &git::Commit, window, cx| {
                this.commit(window, cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .when(side_by_side, |this| this.flex_row())
                    .when(!side_by_side, |this| this.flex_col())
                    .child(list_column)
                    .when(split, |this| this.child(divider).child(preview)),
            )
    }
}

impl Focusable for PlusGitWindow {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The diff of one file, shown whole rather than as a list of hunks.
///
/// One file, whole, rather than the project-wide multibuffer of hunks Zed's diff views use: the
/// point of the pane is reading a file that happens to have changed.
struct DiffPreview {
    project: Entity<Project>,
    editor: Entity<Editor>,
    multibuffer: Entity<MultiBuffer>,
    /// What the editor currently shows, so re-selecting the same row does no work.
    current_path: Option<ProjectPath>,
    /// Shown instead of the editor: either nothing is selected, or its diff failed to load.
    message: Option<SharedString>,
    /// The file name in the header, kept beside `current_path` so the header can render before
    /// the diff finishes loading.
    title: Option<SharedString>,
    /// The directory holding it, shown after the name the way the search modal does.
    subtitle: Option<SharedString>,
    /// Whether stepping between changes means anything here. A newly added file is a single
    /// block of additions, so the buttons would move nowhere.
    has_hunks_to_step_through: bool,
    /// Only one load at a time; selecting a new row cancels the previous one.
    pending_update: Task<()>,
}

impl DiffPreview {
    fn new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let multibuffer = cx.new(|_| MultiBuffer::without_headers(language::Capability::ReadWrite));
        let editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx);
            let settings = EditorSettings::get_global(cx).clone();

            // Read-only, but not inert: the point of the pane is reading a diff, so scrolling and
            // selecting text have to work. Editing belongs in the real editor.
            editor.set_read_only(true);
            editor.set_input_enabled(false);
            editor.disable_inline_diagnostics();
            editor.disable_diagnostics(cx);
            editor.disable_expand_excerpt_buttons(cx);
            editor.disable_mouse_wheel_zoom();
            editor.set_show_gutter(settings.gutter.line_numbers, cx);
            editor.set_show_line_numbers(settings.gutter.line_numbers, cx);
            editor.set_show_git_diff_gutter(true, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_bookmarks(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_show_cursor_when_unfocused(true, cx);
            editor.set_should_serialize(false, cx);
            editor
        });

        Self {
            project,
            editor,
            multibuffer,
            current_path: None,
            title: None,
            subtitle: None,
            has_hunks_to_step_through: false,
            message: Some(NO_SELECTION_MESSAGE.into()),
            pending_update: Task::ready(()),
        }
    }

    fn show(&mut self, path: ProjectPath, has_hunks: bool, cx: &mut Context<Self>) {
        if self.current_path.as_ref() == Some(&path) {
            return;
        }
        self.current_path = Some(path.clone());
        self.has_hunks_to_step_through = has_hunks;
        self.title = path
            .path
            .file_name()
            .map(|name| SharedString::from(name.to_string()));
        // Worktree-relative, so "./" rather than a leading slash. A file at the root has no
        // parent and shows as "./" alone.
        let path_style = self.project.read(cx).path_style(cx);
        self.subtitle = Some(SharedString::from(match path.path.parent() {
            Some(parent) if !parent.is_empty() => format!("./{}", parent.display(path_style)),
            _ => "./".to_string(),
        }));
        let project = self.project.clone();

        self.pending_update = cx.spawn(async move |this, cx| {
            let loaded = load_diff(project, path, cx).await;
            this.update(cx, |this, cx| match loaded {
                Ok((buffer, diff)) => {
                    this.message = None;
                    this.multibuffer.update(cx, |multibuffer, cx| {
                        // `set_excerpts_for_buffer` is keyed by path, so it adds the new file
                        // beside the previous one rather than replacing it. This pane shows one
                        // file at a time, so the old one has to go first.
                        multibuffer.clear(cx);
                        // The whole file as one excerpt, rather than one excerpt per hunk: this
                        // pane is for reading a file that happens to have changed, not for
                        // reviewing every change in the repository at once.
                        let full_file = Point::zero()..buffer.read(cx).max_point();
                        multibuffer.set_excerpts_for_buffer(buffer, [full_file], 0, cx);
                        multibuffer.add_diff(diff, cx);
                    });
                    cx.notify();
                }
                Err(error) => {
                    // A binary file is not a failure to explain away: git has nothing to diff, so
                    // say that rather than implying something went wrong.
                    this.message = Some(if is_binary_error(&error) {
                        BINARY_MESSAGE.into()
                    } else {
                        LOAD_FAILED_MESSAGE.into()
                    });
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.pending_update = Task::ready(());
        self.current_path = None;
        self.title = None;
        self.subtitle = None;
        self.has_hunks_to_step_through = false;
        self.message = Some(NO_SELECTION_MESSAGE.into());
        self.multibuffer
            .update(cx, |multibuffer, cx| multibuffer.clear(cx));
        cx.notify();
    }

    /// Moves the cursor to the next or previous changed block, wrapping at the ends. The editor
    /// scrolls to follow it, which is the whole point: the preview shows the entire file, so the
    /// changes in a long one can be far apart.
    fn go_to_hunk(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.snapshot(window, cx);
            let position = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            editor.go_to_hunk_before_or_after_position(
                &snapshot, position, direction, true, window, cx,
            );
        });
    }
}

async fn load_diff(
    project: Entity<Project>,
    path: ProjectPath,
    cx: &mut gpui::AsyncApp,
) -> anyhow::Result<(Entity<language::Buffer>, Entity<buffer_diff::BufferDiff>)> {
    let buffer = project
        .update(cx, |project, cx| project.open_buffer(path, cx))
        .await?;
    let diff = project
        .update(cx, |project, cx| {
            project.open_uncommitted_diff(buffer.clone(), cx)
        })
        .await?;
    Ok((buffer, diff))
}

impl Render for DiffPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let steppable = self.message.is_none() && self.has_hunks_to_step_through;
        let body = match self.message.clone() {
            Some(message) => v_flex()
                .size_full()
                .justify_center()
                .items_center()
                .child(
                    Label::new(message)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            None => self.editor.clone().into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            // A divider after the row rather than a bottom border on it: a border is drawn
            // inside the height, which would leave this header a pixel shorter than the tree's
            // and put the two lines at different heights.
            .child(
                h_flex()
                    .h(platform_title_bar_height(window))
                    .bg(cx.theme().colors().title_bar_background)
                    .px_2p5()
                    .gap_1()
                    .flex_none()
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .child(
                                Label::new(self.title.clone().unwrap_or_else(|| "Diff".into()))
                                    .single_line(),
                            )
                            .children(self.subtitle.clone().map(|directory| {
                                Label::new(directory)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .truncate()
                            })),
                    )
                    .when(steppable, |this| {
                        this.child(
                            IconButton::new("plus-git-previous-hunk", IconName::ArrowUp)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Previous Change"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.go_to_hunk(Direction::Prev, window, cx);
                                })),
                        )
                        .child(
                            IconButton::new("plus-git-next-hunk", IconName::ArrowDown)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Next Change"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.go_to_hunk(Direction::Next, window, cx);
                                })),
                        )
                    }),
            )
            // `Border`, not the default `BorderVariant`: over the panel background the faded
            // variant barely reads, where the editor window's own header seam is plainly there.
            .child(Divider::horizontal().color(DividerColor::Border))
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// A line in the list. Tracked and untracked files share one tree rather than being split into
/// sections, so a directory holding both shows them together.
enum Row {
    Directory {
        /// Full path from the repository root, which is what `collapsed_dirs` is keyed by.
        path: String,
        /// What the row shows. Differs from the last component of `path` when a chain of
        /// single-child directories has been folded into one row.
        label: String,
        depth: usize,
        collapsed: bool,
    },
    File {
        /// Index into `entries`.
        entry: usize,
        depth: usize,
    },
}

#[derive(Default)]
struct TreeNode {
    children: BTreeMap<String, TreeNode>,
    files: Vec<usize>,
}

impl TreeNode {
    fn insert(&mut self, components: &[&str], entry: usize) {
        match components {
            [] => {}
            [_file] => self.files.push(entry),
            [directory, rest @ ..] => self
                .children
                .entry((*directory).to_string())
                .or_default()
                .insert(rest, entry),
        }
    }

    /// Appends this node's rows, directories before files, skipping the contents of anything
    /// collapsed. With `compress`, a chain of directories holding a single child and no files of
    /// its own becomes one row, so `assets/themes/gruvbox` is one line rather than three;
    /// without it, every directory gets a level of its own.
    fn flatten(
        &self,
        prefix: &str,
        depth: usize,
        compress: bool,
        collapsed: &dyn Fn(&str) -> bool,
        rows: &mut Vec<Row>,
    ) {
        for (name, child) in &self.children {
            let mut path = join_path(prefix, name);
            let mut label = name.clone();
            let mut child = child;
            while compress && child.files.is_empty() && child.children.len() == 1 {
                let (name, only_child) = child
                    .children
                    .iter()
                    .next()
                    .expect("a map of length one has a first entry");
                path = join_path(&path, name);
                label = format!("{label}/{name}");
                child = only_child;
            }

            let is_collapsed = collapsed(&path);
            rows.push(Row::Directory {
                path: path.clone(),
                label,
                depth,
                collapsed: is_collapsed,
            });
            if !is_collapsed {
                child.flatten(&path, depth + 1, compress, collapsed, rows);
            }
        }

        for &entry in &self.files {
            rows.push(Row::File { entry, depth });
        }
    }
}

fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// One row: a changed file, with everything needed to render it and to open it later.
struct ChangedFile {
    repo_path: RepoPath,
    /// `None` for a file outside every worktree, which cannot be opened or previewed.
    project_path: Option<ProjectPath>,
    status: FileStatus,
    diff_stat: Option<DiffStat>,
    /// Repository-relative path, both matched against the query and shown in the row.
    display_path: String,
    file_name: String,
}

/// A blank spacer standing in for a row's tree depth. A file directly under a directory row lines
/// up past that row's chevron, so the depth is offset by one.
/// The folder icon for a directory row, from the user's icon theme when it has one for that
/// directory name, and open or closed to show whether the row is expanded.
fn folder_icon(collapsed: bool, path: &str, cx: &App) -> Icon {
    let fallback = if collapsed {
        IconName::Folder
    } else {
        IconName::FolderOpen
    };
    FileIcons::get_folder_icon(!collapsed, std::path::Path::new(path), cx)
        .map(Icon::from_path)
        .unwrap_or_else(|| Icon::new(fallback))
        .color(Color::Muted)
        .size(IconSize::Small)
}

fn indent(depth: usize) -> gpui::Div {
    div().flex_none().w(px(INDENT_PER_DEPTH * depth as f32))
}

/// The commit message editor: a plain multi-line editor over the repository's commit buffer,
/// sized to its content. Ported from `git_ui`, where the equivalent is crate-private.
/// An editor of exactly `lines` rows. `EditorMode::Full` would grow to fill a container, but
/// brings the whole code-editor apparatus with it — gutter, line numbers, code actions, the
/// buffer font — so the box stays a fixed number of rows.
///
/// One row is `SingleLine` rather than a one-row `AutoHeight`, which would still accept newlines
/// and scroll them out of sight. `SingleLine` refuses them at the editor.
fn commit_editor_mode(lines: usize) -> editor::EditorMode {
    if lines <= 1 {
        editor::EditorMode::SingleLine
    } else {
        editor::EditorMode::AutoHeight {
            min_lines: lines,
            max_lines: Some(lines),
        }
    }
}

fn commit_message_editor(
    commit_buffer: Entity<language::Buffer>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Editor {
    let buffer = cx.new(|cx| MultiBuffer::singleton(commit_buffer, cx));
    let mut editor = Editor::new(commit_editor_mode(COMMIT_LINES), buffer, None, window, cx);
    editor.set_use_autoclose(false);
    editor.set_show_gutter(false, cx);
    editor.set_show_wrap_guides(false, cx);
    editor.set_show_indent_guides(false, cx);
    editor.set_placeholder_text("Commit message", window, cx);
    editor
}

/// A button in the commit row. All three are built the same way so they cannot drift apart:
/// `Button` and `ButtonLike` do not agree on their defaults, and the commit button has to be a
/// `ButtonLike` regardless, since only that half-rounds for a split button.
///
/// `Outlined` is what the rest of Zed's windows use for a button of this weight — bordered rather
/// than filled. The commit button also sits inside a `SplitButton`, which paints its own surface
/// across both halves, so it takes `Transparent` to avoid painting twice.
fn action_button(base: ButtonLike, label: &str, enabled: bool, style: ButtonStyle) -> ButtonLike {
    base.style(style)
        .size(ButtonSize::Large)
        .disabled(!enabled)
        .child(Label::new(label.to_string()))
}

/// The chevron half of a split button. Ported from `git_ui`, where it is crate-private.
fn split_button_chevron(id: &'static str, menu_open: bool) -> ButtonLike {
    // Square, and the same height as `ButtonSize::Large`, so the two halves match.
    let size = ui::rems_from_px(32.);
    ButtonLike::new_rounded_right(id)
        .style(ButtonStyle::Transparent)
        .selected_style(ButtonStyle::Tinted(ui::TintColor::Accent))
        .width(size)
        .height(size.into())
        .child(
            Icon::new(if menu_open {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            })
            .size(IconSize::Small),
        )
}

/// A staged file is checked, an unstaged one is not, and a file staged in part is neither.
fn stage_toggle_state(status: StageStatus) -> ToggleState {
    match status {
        StageStatus::Staged => ToggleState::Selected,
        StageStatus::Unstaged => ToggleState::Unselected,
        StageStatus::PartiallyStaged => ToggleState::Indeterminate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders what the list would show, one string per row, indented by depth. Directories end
    /// in `/`; files show their full path so a misplaced row is obvious.
    fn outline(paths: &[&str], collapsed: &[&str]) -> Vec<String> {
        outline_with(paths, collapsed, true)
    }

    fn outline_with(paths: &[&str], collapsed: &[&str], compress: bool) -> Vec<String> {
        let mut root = TreeNode::default();
        let mut indices = (0..paths.len()).collect::<Vec<_>>();
        indices.sort_by_key(|index| paths[*index]);
        for index in indices {
            root.insert(&paths[index].split('/').collect::<Vec<_>>(), index);
        }

        let collapsed = collapsed
            .iter()
            .map(|path| path.to_string())
            .collect::<HashSet<_>>();
        let mut rows = Vec::new();
        root.flatten("", 0, compress, &|path| collapsed.contains(path), &mut rows);

        rows.iter()
            .map(|row| {
                let indent = |depth: &usize| "  ".repeat(*depth);
                match row {
                    Row::Directory { label, depth, .. } => format!("{}{label}/", indent(depth)),
                    Row::File { entry, depth } => format!("{}{}", indent(depth), paths[*entry]),
                }
            })
            .collect()
    }

    #[test]
    fn folds_a_chain_of_single_child_directories() {
        assert_eq!(
            outline(
                &[
                    "assets/keymaps/default-linux.json",
                    "assets/themes/gruvbox/LICENSE"
                ],
                &[]
            ),
            vec![
                "assets/",
                "  keymaps/",
                "    assets/keymaps/default-linux.json",
                "  themes/gruvbox/",
                "    assets/themes/gruvbox/LICENSE",
            ]
        );
    }

    #[test]
    fn a_collapsed_directory_hides_its_contents_but_not_its_siblings() {
        assert_eq!(
            outline(
                &["src/main.rs", "src/nested/deep.rs", "README.md"],
                &["src"]
            ),
            vec!["src/", "README.md"]
        );
    }

    #[test]
    fn directories_come_before_files_at_every_level() {
        assert_eq!(
            outline(&["b.rs", "a/inner.rs", "a.rs"], &[]),
            vec!["a/", "  a/inner.rs", "a.rs", "b.rs"]
        );
    }

    /// Without compression every directory gets a row, and the files under it move one level
    /// deeper for each.
    #[test]
    fn every_directory_gets_a_level_when_compression_is_off() {
        let paths = ["assets/themes/gruvbox/LICENSE"];
        assert_eq!(
            outline_with(&paths, &[], false),
            vec![
                "assets/",
                "  themes/",
                "    gruvbox/",
                "      assets/themes/gruvbox/LICENSE",
            ]
        );
        assert_eq!(
            outline_with(&paths, &[], true),
            vec!["assets/themes/gruvbox/", "  assets/themes/gruvbox/LICENSE",]
        );
    }

    /// Uncompressed, each level is collapsible on its own — the whole point of the mode.
    #[test]
    fn an_intermediate_directory_collapses_when_compression_is_off() {
        assert_eq!(
            outline_with(
                &["assets/themes/gruvbox/LICENSE"],
                &["assets/themes"],
                false
            ),
            vec!["assets/", "  themes/"]
        );
    }

    /// A folded chain is keyed by its full path, so collapsing it takes the whole chain with it.
    #[test]
    fn a_folded_chain_collapses_under_its_full_path() {
        let paths = ["assets/themes/gruvbox/LICENSE"];
        assert_eq!(
            outline(&paths, &["assets/themes/gruvbox"]),
            vec!["assets/themes/gruvbox/"]
        );
        // `assets` is not a row of its own once the chain is folded, so collapsing it is not
        // something the user can express, and asking for it changes nothing.
        assert_eq!(
            outline(&paths, &["assets"]),
            vec!["assets/themes/gruvbox/", "  assets/themes/gruvbox/LICENSE"]
        );
    }
}
