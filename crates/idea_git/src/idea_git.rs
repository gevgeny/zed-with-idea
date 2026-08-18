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

use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Context as _;
use editor::{Direction, Editor, EditorSettings};
use file_icons::FileIcons;
use fuzzy::StringMatchCandidate;
use git::{
    repository::RepoPath,
    status::{DiffStat, FileStatus, StageStatus},
};
use git_ui::git_status_icon;
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, FocusHandle, Focusable, IntoElement,
    ParentElement, Pixels, Render, SharedString, Styled, Subscription, Task, TitlebarOptions,
    UniformListScrollHandle, WeakEntity, Window, WindowBounds, WindowOptions, actions, px,
    uniform_list,
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
    Divider, ElevationIndex, Icon, IconButton, IconPosition, Label, ListItem, ListItemSpacing,
    PopoverMenu, PopoverMenuHandle, SplitButton, ToggleState, Tooltip, h_flex, prelude::*, v_flex,
};
use util::ResultExt as _;
use workspace::Workspace;

actions!(
    idea_git,
    [
        /// Opens the git changes window.
        Toggle
    ]
);

/// Bumped on every change, and shown in the status bar so a running build can be identified while
/// iterating. Remove before this is considered finished.
const VERSION: &str = "0.14.w1";

/// What the window opens at the first time. After that the operating system remembers its size
/// and position, which is most of the point of being a window.
const DEFAULT_WINDOW_SIZE: gpui::Size<Pixels> = gpui::Size {
    width: px(1100.),
    height: px(720.),
};

/// The list column will not shrink below this, so the diff cannot squeeze the tree out of view.
const MIN_LIST_WIDTH: Pixels = px(280.);

/// Upper bound on rows kept after filtering. A repository with more changed files than this is
/// past the point where scrolling a list helps.
const MAX_MATCHES: usize = 2_000;

/// How tall the commit message editor is, in lines.
const COMMIT_EDITOR_LINES: usize = 3;

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
        let project = workspace.project().clone();
        let workspace = cx.entity().downgrade();
        toggle_window(workspace, project, cx);
    });
}

/// Opens the window for this project, or brings the existing one forward. One window per project,
/// found by asking the app for its windows and matching on the project entity — the same lookup
/// the settings window uses to keep itself unique.
fn toggle_window(workspace: WeakEntity<Workspace>, project: Entity<Project>, cx: &mut App) {
    let existing = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<IdeaGitWindow>())
        .find(|window| window.read(cx).is_ok_and(|git| git.project == project));

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
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                focus: true,
                show: true,
                is_movable: true,
                kind: gpui::WindowKind::Normal,
                window_background: cx.theme().window_background_appearance(),
                window_min_size: Some(gpui::Size {
                    width: px(720.),
                    height: px(400.),
                }),
                window_bounds: Some(WindowBounds::centered(DEFAULT_WINDOW_SIZE, cx)),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| IdeaGitWindow::new(workspace, project, window, cx)),
        )
        .log_err();
    });
}

pub struct IdeaGitWindow {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    /// `None` when the project has no git repository, which shows as an empty list.
    repository: Option<Entity<Repository>>,
    focus_handle: FocusHandle,
    /// Filters the tree by path. A window has no query to seed and nothing to dismiss, so this is
    /// a plain single-line editor.
    filter_editor: Entity<Editor>,
    preview: Entity<DiffPreview>,
    scroll_handle: UniformListScrollHandle,
    entries: Vec<ChangedFile>,
    /// Indices into `entries` surviving the current query.
    matches: Vec<usize>,
    /// What the list actually shows: `matches` arranged as a tree, minus anything inside a
    /// collapsed directory. Rebuilt whenever the matches or the collapsed set change.
    rows: Vec<Row>,
    /// Directories the user has collapsed, keyed by their full path. Absent means expanded.
    collapsed_dirs: HashSet<String>,
    /// While a query is active the tree renders fully expanded, so a match cannot hide inside a
    /// collapsed directory. `collapsed_dirs` is kept, not cleared, so clearing the query puts the
    /// tree back the way the user left it.
    query_is_active: bool,
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
    cancel_flag: Arc<AtomicBool>,
    _subscriptions: Vec<Subscription>,
}

impl IdeaGitWindow {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let repository = project.read(cx).active_repository(cx);
        let filter_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter changed files…", window, cx);
            editor
        });
        let preview = cx.new(|cx| DiffPreview::new(project.clone(), window, cx));

        let mut subscriptions = vec![cx.subscribe_in(
            &filter_editor,
            window,
            |this, _, event: &editor::EditorEvent, window, cx| {
                if matches!(event, editor::EditorEvent::BufferEdited) {
                    this.refresh_matches(window, cx);
                }
            },
        )];

        // A window outlives the repository it was opened against — a branch switch or a change of
        // active repository would otherwise leave it showing a stale tree forever. A modal never
        // lived long enough for this to matter.
        let git_store = project.read(cx).git_store().clone();
        subscriptions.push(cx.subscribe_in(
            &git_store,
            window,
            |this, _, event: &GitStoreEvent, window, cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_) => {
                    this.follow_active_repository(window, cx)
                }
                GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::StatusesChanged, true) => {
                    this.reload_entries(cx);
                    this.refresh_matches(window, cx);
                }
                _ => {}
            },
        ));

        let mut this = Self {
            workspace,
            project,
            repository,
            focus_handle: cx.focus_handle(),
            filter_editor,
            preview,
            scroll_handle: UniformListScrollHandle::new(),
            entries: Vec::new(),
            matches: Vec::new(),
            rows: Vec::new(),
            collapsed_dirs: HashSet::new(),
            query_is_active: false,
            selected_index: 0,
            preview_enabled: true,
            commit_editor: None,
            committing: false,
            pushing: false,
            amend: false,
            signoff: false,
            skip_hooks: false,
            commit_menu_handle: PopoverMenuHandle::default(),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            _subscriptions: subscriptions,
        };
        this.reload_entries(cx);
        this.refresh_matches(window, cx);
        this.open_commit_buffer(window, cx);
        this
    }

    /// Repoints the window at whichever repository is now active, and reloads from it.
    fn follow_active_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repository = self.project.read(cx).active_repository(cx);
        if repository == self.repository {
            return;
        }
        self.repository = repository;
        self.collapsed_dirs.clear();
        self.selected_index = 0;
        self.reload_entries(cx);
        self.refresh_matches(window, cx);
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

    fn refresh_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.filter_editor.read(cx).text(cx);
        self.update_matches(query, window, cx).detach();
    }

    fn selected_row_is_directory(&self) -> bool {
        matches!(
            self.rows.get(self.selected_index),
            Some(Row::Directory { .. })
        )
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
        match self
            .selected_entry()
            .and_then(|entry| entry.project_path.clone())
        {
            Some(path) => self
                .preview
                .update(cx, |preview, cx| preview.show(path, cx)),
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
        let next = (self.selected_index + 1) % self.rows.len();
        self.select(next, cx);
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
        if self.selected_row_is_directory() {
            self.toggle_selected_directory(cx);
        } else {
            self.open_selected(window, cx);
        }
    }

    /// Escape empties the filter, and closes the window once it is already empty.
    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_editor.read(cx).text(cx).is_empty() {
            window.remove_window();
            return;
        }
        self.filter_editor
            .update(cx, |editor, cx| editor.clear(window, cx));
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_index;
        uniform_list(
            "idea-git-entries",
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
                        .filter_map(|index| this.rows.get(index).map(Row::depth))
                        .collect()
                }),
        )
        .flex_grow_1()
        .py_1()
        .track_scroll(&self.scroll_handle)
    }

    fn render_filter_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .child(
                h_flex()
                    .h_9()
                    .px_2p5()
                    .gap_1()
                    .flex_none()
                    .overflow_hidden()
                    .child(div().flex_1().child(self.filter_editor.clone()))
                    .children(self.render_header_controls(window, cx)),
            )
            .child(Divider::horizontal())
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let summary = match (self.entries.len(), self.rows.len()) {
            (0, _) => "No changes".to_string(),
            (total, _) if !self.query_is_active => format!("{total} changed files"),
            (total, shown) => format!("{shown} of {total} changed files"),
        };

        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .justify_between()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new(summary)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(VERSION)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }
}

impl Render for IdeaGitWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter_bar = self.render_filter_bar(window, cx).into_any_element();
        let list = self.render_list(cx).into_any_element();
        let commit_section = self
            .render_commit_section(window, cx)
            .map(IntoElement::into_any_element);
        let status_bar = self.render_status_bar(cx).into_any_element();

        v_flex()
            .key_context("IdeaGit")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().background)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .h_full()
                            .when(self.preview_enabled, |this| {
                                this.w(MIN_LIST_WIDTH).flex_none()
                            })
                            .when(!self.preview_enabled, |this| this.flex_1())
                            .child(filter_bar)
                            .child(div().flex_1().min_h_0().child(list))
                            .children(commit_section),
                    )
                    .when(self.preview_enabled, |this| {
                        this.child(Divider::vertical()).child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .child(self.preview.clone()),
                        )
                    }),
            )
            .child(status_bar)
    }
}

impl Focusable for IdeaGitWindow {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter_editor.focus_handle(cx)
    }
}

impl IdeaGitWindow {
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
    }

    /// Arranges `matches` into `rows`: one tree holding tracked and untracked files together, so
    /// a directory containing both shows them side by side rather than twice under two headings.
    fn rebuild_rows(&mut self) {
        // Sorted by path so rows come out alphabetically: `matches` arrives either in
        // `cached_status` order or ranked by fuzzy relevance, neither of which means anything
        // once the paths are arranged as a tree.
        let mut matches = self.matches.clone();
        matches.sort_by(|a, b| match (self.entries.get(*a), self.entries.get(*b)) {
            (Some(a), Some(b)) => a.display_path.cmp(&b.display_path),
            _ => std::cmp::Ordering::Equal,
        });

        let mut root = TreeNode::default();
        for &index in &matches {
            let Some(entry) = self.entries.get(index) else {
                continue;
            };
            root.insert(&entry.display_path.split('/').collect::<Vec<_>>(), index);
        }

        let collapsed_dirs = std::mem::take(&mut self.collapsed_dirs);
        let query_is_active = self.query_is_active;
        let mut rows = Vec::new();
        root.flatten(
            "",
            0,
            &|path: &str| !query_is_active && collapsed_dirs.contains(path),
            &mut rows,
        );

        self.collapsed_dirs = collapsed_dirs;
        self.rows = rows;
        self.selected_index = self.selected_index.min(self.rows.len().saturating_sub(1));
    }

    /// Every file under `directory`, which is what a directory's checkbox stages or unstages.
    fn files_under(&self, directory: &str) -> Vec<&ChangedFile> {
        let prefix = format!("{directory}/");
        self.entries
            .iter()
            .filter(|entry| entry.display_path.starts_with(&prefix))
            .collect()
    }

    /// A directory is staged when every file under it is, unstaged when none are, and mixed
    /// otherwise — which is what the checkbox's indeterminate state shows.
    fn directory_stage_status(&self, directory: &str) -> StageStatus {
        let mut staged = 0;
        let mut total = 0;
        for entry in self.files_under(directory) {
            total += 1;
            match entry.status.staging() {
                StageStatus::Staged => staged += 1,
                StageStatus::PartiallyStaged => return StageStatus::PartiallyStaged,
                StageStatus::Unstaged => {}
            }
        }
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
        if self.query_is_active {
            return;
        }
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
        if self.any_directory_expanded() {
            // Only the directories currently shown are known here, and collapsing an outer one
            // hides the inner ones before they can be collected, so this walks until it settles.
            loop {
                let expanded = self
                    .rows
                    .iter()
                    .filter_map(|row| match row {
                        Row::Directory {
                            path,
                            collapsed: false,
                            ..
                        } => Some(path.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if expanded.is_empty() {
                    break;
                }
                self.collapsed_dirs.extend(expanded);
                self.rebuild_rows();
            }
        } else {
            self.collapsed_dirs.clear();
            self.rebuild_rows();
        }
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

    fn has_staged_changes(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.status.staging().has_staged())
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
                    .into_iter()
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

        let unstaged_tracked = (!self.has_staged_changes())
            .then(|| {
                self.entries
                    .iter()
                    .filter(|entry| !entry.status.is_untracked())
                    .map(|entry| entry.repo_path.clone())
                    .collect::<Vec<_>>()
            })
            .filter(|paths| !paths.is_empty());

        let askpass = self.askpass_delegate("git commit", window, cx);
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
        let askpass = self.askpass_delegate("git push", window, cx);

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
    /// modal, so it takes this one's place; answering it lets the commit finish.
    fn askpass_delegate(
        &self,
        operation: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> askpass::AskPassDelegate {
        let workspace = self.workspace.clone();
        let window = window.window_handle();
        askpass::AskPassDelegate::new(&mut cx.to_async(), move |prompt, tx, cx| {
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

        cx.spawn_in(window, async move |_, cx| {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_path(path, None, true, window, cx)
                })?
                .await?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn cancel_running_filter(&mut self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
        self.cancel_flag = Arc::new(AtomicBool::new(false));
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        self.cancel_running_filter();
        self.query_is_active = !query.is_empty();

        if query.is_empty() {
            self.matches = (0..self.entries.len().min(MAX_MATCHES)).collect();
            self.selected_index = 0;
            self.rebuild_rows();
            cx.notify();
            return Task::ready(());
        }

        let candidates = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| StringMatchCandidate::new(index, &entry.display_path))
            .collect::<Vec<_>>();
        let cancel_flag = Arc::clone(&self.cancel_flag);
        let executor = cx.background_executor().clone();

        cx.spawn_in(window, async move |this, cx| {
            let matches = fuzzy::match_strings(
                &candidates,
                &query,
                false,
                true,
                MAX_MATCHES,
                &cancel_flag,
                executor,
            )
            .await;
            if cancel_flag.load(Ordering::SeqCst) {
                return;
            }

            this.update(cx, |this, cx| {
                this.matches = matches
                    .into_iter()
                    .map(|entry| entry.candidate_id)
                    .collect();
                this.selected_index = 0;
                this.rebuild_rows();
                cx.notify();
            })
            .log_err();
        })
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

        PopoverMenu::new("idea-git-commit-menu")
            .trigger(split_button_chevron(
                "idea-git-commit-menu-trigger",
                self.commit_menu_handle.is_deployed(),
            ))
            .with_handle(self.commit_menu_handle.clone())
            .anchor(gpui::Anchor::BottomRight)
            .menu(move |window, cx| {
                let toggle = |_unused: &Entity<IdeaGitWindow>, apply: fn(&mut IdeaGitWindow)| {
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
        let preview_enabled = self.preview_enabled;
        let this = cx.entity();

        Some(
            h_flex()
                .flex_none()
                .gap_1p5()
                .children(self.total_diff_stat().map(|total| {
                    DiffStatElement::new(
                        "idea-git-total-diff-stat",
                        total.added as usize,
                        total.deleted as usize,
                    )
                }))
                .child(
                    IconButton::new(
                        "idea-git-collapse-all",
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
                    IconButton::new("idea-git-preview-toggle", IconName::Eye)
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
                .gap_1p5()
                .p_2()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .children(self.commit_editor.clone().map(|editor| {
                    div()
                        .w_full()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(cx.theme().colors().editor_background)
                        .child(editor)
                }))
                .child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .child(
                            Label::new(format!("{staged} of {} staged", self.entries.len()))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(div().flex_1())
                        .child(
                            action_button(
                                ButtonLike::new("idea-git-stage-all"),
                                if all_staged {
                                    "Unstage all"
                                } else {
                                    "Stage all"
                                },
                                !self.entries.is_empty(),
                                ElevationIndex::Surface,
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
                        .child(SplitButton::new(
                            action_button(
                                ButtonLike::new_rounded_left("idea-git-commit"),
                                commit_label,
                                can_commit,
                                ElevationIndex::ModalSurface,
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
                        ))
                        .child(
                            action_button(
                                ButtonLike::new("idea-git-push"),
                                "Push",
                                can_push,
                                ElevationIndex::Surface,
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
            Checkbox::new(("stage", ix), stage_toggle_state(state)).on_click(
                move |_, window, cx| {
                    this.update(cx, |this, cx| {
                        this.toggle_staged_for_row(ix, cx);
                        cx.notify();
                    });
                    cx.stop_propagation();
                    window.refresh();
                },
            )
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
                                let filter_input = this.filter_editor.focus_handle(cx);
                                window.focus(&filter_input, cx);
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
                                let filter_input = this.filter_editor.focus_handle(cx);
                                window.focus(&filter_input, cx);

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
            message: Some(NO_SELECTION_MESSAGE.into()),
            pending_update: Task::ready(()),
        }
    }

    fn show(&mut self, path: ProjectPath, cx: &mut Context<Self>) {
        if self.current_path.as_ref() == Some(&path) {
            return;
        }
        self.current_path = Some(path.clone());
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
        self.message = Some(NO_SELECTION_MESSAGE.into());
        self.multibuffer
            .update(cx, |multibuffer, cx| multibuffer.clear(cx));
        cx.notify();
    }
}

impl DiffPreview {
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            // `h_9` then a divider, rather than a bottom border on the row itself: the border
            // would sit inside the 36px and leave this header a pixel shorter than the filter
            // bar, which is built exactly this way.
            .child(
                h_flex()
                    .h_9()
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
                    .child(
                        IconButton::new("idea-git-previous-hunk", IconName::ArrowUp)
                            .icon_size(IconSize::Small)
                            .disabled(self.message.is_some())
                            .tooltip(Tooltip::text("Previous Change"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.go_to_hunk(Direction::Prev, window, cx);
                            })),
                    )
                    .child(
                        IconButton::new("idea-git-next-hunk", IconName::ArrowDown)
                            .icon_size(IconSize::Small)
                            .disabled(self.message.is_some())
                            .tooltip(Tooltip::text("Next Change"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.go_to_hunk(Direction::Next, window, cx);
                            })),
                    ),
            )
            .child(Divider::horizontal())
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

impl Row {
    fn depth(&self) -> usize {
        match self {
            Row::Directory { depth, .. } | Row::File { depth, .. } => *depth,
        }
    }
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
    /// collapsed. A chain of directories with a single child and no files of its own is folded
    /// into one row, so `assets/themes/gruvbox` is one line rather than three.
    fn flatten(
        &self,
        prefix: &str,
        depth: usize,
        collapsed: &dyn Fn(&str) -> bool,
        rows: &mut Vec<Row>,
    ) {
        for (name, child) in &self.children {
            let mut path = join_path(prefix, name);
            let mut label = name.clone();
            let mut child = child;
            while child.files.is_empty() && child.children.len() == 1 {
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
                child.flatten(&path, depth + 1, collapsed, rows);
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
fn commit_message_editor(
    commit_buffer: Entity<language::Buffer>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Editor {
    let buffer = cx.new(|cx| MultiBuffer::singleton(commit_buffer, cx));
    let mut editor = Editor::new(
        editor::EditorMode::AutoHeight {
            min_lines: COMMIT_EDITOR_LINES,
            max_lines: Some(COMMIT_EDITOR_LINES),
        },
        buffer,
        None,
        window,
        cx,
    );
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
/// The elevation differs by design. A standalone button paints its own background, and
/// `ElevationIndex::Surface` is what reads against the modal; the commit button sits inside a
/// `SplitButton`, which paints that same surface across both halves itself, so its own layer is
/// the modal's and stays invisible underneath.
fn action_button(
    base: ButtonLike,
    label: &str,
    enabled: bool,
    elevation: ElevationIndex,
) -> ButtonLike {
    base.layer(elevation)
        .size(ButtonSize::Compact)
        .disabled(!enabled)
        .child(Label::new(label.to_string()).size(LabelSize::Small))
}

/// The chevron half of a split button. Ported from `git_ui`, where it is crate-private.
fn split_button_chevron(id: &'static str, menu_open: bool) -> ButtonLike {
    let size = ui::rems_from_px(20.);
    ButtonLike::new_rounded_right(id)
        .layer(ElevationIndex::ModalSurface)
        .selected_style(ButtonStyle::Tinted(ui::TintColor::Accent))
        .width(size)
        .height(size.into())
        .child(
            Icon::new(if menu_open {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            })
            .size(IconSize::XSmall),
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
        root.flatten("", 0, &|path| collapsed.contains(path), &mut rows);

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
