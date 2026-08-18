//! A JetBrains-style git modal: the changed files of the active repository, filterable, with a
//! diff of the selected one.
//!
//! Kept in its own crate so it stays mergeable with upstream Zed. It deliberately does not reuse
//! `GitPanel`: the panel's list, selection and staging state are private to `git_ui`, and hosting
//! its renderers would mean hiding the dock to avoid rendering the same entity twice. Everything
//! here goes straight to `project`'s public git API instead.
//!
//! The pieces `git_ui` does expose are reused rather than reimplemented — `git_status_icon` for
//! the per-file status glyph — so the two stay visually in step.

use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
    path::PathBuf,
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
    AnyElement, App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, ParentElement, Pixels, Render, SharedString, Styled, Subscription,
    Task, WeakEntity, Window, actions, px,
};
use language::Point;
use multi_buffer::MultiBuffer;
use picker::{
    Picker, PickerDelegate, PreviewBackend, PreviewLayout, PreviewSource, PreviewUpdate,
    SetPreviewHidden, SetPreviewRight,
};
use project::{
    Project, ProjectPath,
    git_store::{Repository, RepositoryEvent},
};
use settings::Settings as _;
// `git::status::DiffStat` is the number pair; `ui::DiffStat` is the element that shows it.
use ui::{
    ButtonLike, ButtonSize, ButtonStyle, Checkbox, ContextMenu, DiffStat as DiffStatElement,
    Divider, ElevationIndex, Icon, IconButton, IconPosition, Label, ListItem, ListItemSpacing,
    PopoverMenu, PopoverMenuHandle, SplitButton, ToggleState, Tooltip, h_flex, prelude::*, v_flex,
};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

actions!(
    idea_git,
    [
        /// Opens the git changes modal.
        Toggle
    ]
);

/// Bumped on every change, and shown in the footer so a running build can be identified while
/// iterating. Remove before this is considered finished.
const VERSION: &str = "0.13.p21";

/// Wide enough for a path plus its diff stat, and shared with the diff preview later.
const MODAL_WIDTH: Rems = Rems(46.);

/// Upper bound on rows kept after filtering. A repository with more changed files than this is
/// past the point where scrolling a list helps.
const MAX_MATCHES: usize = 2_000;

/// How tall the commit message editor is, in lines.
const COMMIT_EDITOR_LINES: usize = 3;

/// Where the indent guides sit: the left edge of a row's icon plus half an `IconSize::Small`
/// (14px), which puts the line down the middle of the icon rather than against its edge.
const INDENT_GUIDE_LEFT_OFFSET: f32 = 26.;

/// Horizontal step per level of the tree, in pixels because the indent guides are drawn from it.
/// The same step the git panel's tree uses.
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
    cx.observe_new(IdeaGit::register).detach();
}

pub struct IdeaGit {
    picker: Entity<Picker<IdeaGitDelegate>>,
    /// Wraps the picker and, later, the diff preview. The picker dismisses itself whenever its
    /// query input blurs, so it needs a way to ask whether focus merely moved elsewhere inside
    /// the modal.
    focus_handle: FocusHandle,
    _repository_subscription: Option<Subscription>,
}

impl IdeaGit {
    fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.register_action(move |workspace, _: &Toggle, window, cx| {
            let project = workspace.project().clone();
            let handle = cx.entity().downgrade();

            workspace.toggle_modal(window, cx, move |window, cx| {
                IdeaGit::new(handle, project, window, cx)
            });
        });
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let repository = project.read(cx).active_repository(cx);
        let focus_handle = cx.focus_handle();
        let project_for_preview = project.clone();

        let mut delegate = IdeaGitDelegate {
            modal: cx.entity().downgrade(),
            modal_focus_handle: focus_handle.clone(),
            workspace,
            project,
            repository: repository.clone(),
            entries: Vec::new(),
            matches: Vec::new(),
            rows: Vec::new(),
            collapsed_dirs: HashSet::new(),
            query_is_active: false,
            selected_index: 0,
            preview_layout: PreviewLayout::Hidden,
            preview_enabled: true,
            commit_editor: None,
            committing: false,
            pushing: false,
            amend: false,
            signoff: false,
            skip_hooks: false,
            commit_menu_handle: PopoverMenuHandle::default(),
            cancel_flag: Arc::new(AtomicBool::new(false)),
        };
        delegate.reload_entries(cx);

        let preview = Arc::new(DiffPreviewHandle(
            cx.new(|cx| DiffPreview::new(project_for_preview, window, cx)),
        ));
        // Resizing is left on (the picker's default for a preview picker): it supplies the
        // divider between list and preview, the edge handles, and persistence of both.
        let picker = cx.new(|cx| {
            Picker::uniform_list_with_preview(delegate, preview, window, cx)
                .initial_width(MODAL_WIDTH)
                .show_scrollbar(true)
        });

        // The commit buffer is the repository's own COMMIT_EDITMSG, shared with the git panel, so
        // a message typed in one shows up in the other and survives closing the modal. Opening it
        // is a background job, so the editor appears a frame or two after the modal does.
        if let Some(repository) = repository.clone() {
            let project = picker.read(cx).delegate.project.clone();
            let languages = project.read(cx).languages().clone();
            let buffer_store = project.read(cx).buffer_store().clone();
            let open = repository.update(cx, |repository, cx| {
                repository.open_commit_buffer(Some(languages), buffer_store, cx)
            });
            cx.spawn_in(window, {
                let picker = picker.clone();
                async move |_, cx| {
                    let buffer = open.await?;
                    picker.update_in(cx, |picker, window, cx| {
                        let editor = cx.new(|cx| commit_message_editor(buffer, window, cx));
                        picker.delegate.commit_editor = Some(editor);
                        cx.notify();
                    })?;
                    anyhow::Ok(())
                }
            })
            .detach_and_log_err(cx);
        }

        // Staging from anywhere else — the git panel, a terminal — changes the list under us.
        let _repository_subscription = repository.map(|repository| {
            cx.subscribe_in(
                &repository,
                window,
                |this, _, event: &RepositoryEvent, window, cx| {
                    if !matches!(event, RepositoryEvent::StatusesChanged) {
                        return;
                    }
                    this.picker.update(cx, |picker, cx| {
                        picker.delegate.reload_entries(cx);
                        picker.refresh(window, cx);
                    });
                },
            )
        });

        Self {
            picker,
            focus_handle,
            _repository_subscription,
        }
    }

    /// First escape empties the query, second closes the modal. The picker consumes
    /// `menu::Cancel` itself, so this has to run in the capture phase to get there first.
    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.picker.read(cx).query(cx);
        if query.is_empty() {
            return;
        }
        self.picker.update(cx, |picker, cx| {
            picker.set_query("", window, cx);
        });
        cx.stop_propagation();
    }

    /// Puts the preview beside the list, since the picker itself opens with it hidden. The
    /// picker falls back to placing it below when the window is too narrow to split.
    ///
    /// Deliberately follows only the user's own preference, not whether there is anything to
    /// show: each layout carries its own persisted size, so flipping the layout as the list
    /// empties and refills would swap a dragged size out from under the user.
    ///
    /// Driven from `render` because the layout is only reachable by dispatching an action, and a
    /// dispatch that lands too early is simply retried on the next frame.
    fn sync_preview_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let delegate = &self.picker.read(cx).delegate;
        let wanted = if delegate.preview_enabled {
            PreviewLayout::Right
        } else {
            PreviewLayout::Hidden
        };
        if delegate.preview_layout == wanted {
            return;
        }

        let action: &dyn gpui::Action = if wanted == PreviewLayout::Hidden {
            &SetPreviewHidden
        } else {
            &SetPreviewRight
        };
        let focus_handle = self.picker.focus_handle(cx);
        window.defer(cx, move |window, cx| {
            focus_handle.dispatch_action(action, window, cx);
        });
    }
}

impl Render for IdeaGit {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_preview_layout(window, cx);

        // The status bar goes here rather than in the picker's footer, which the commit section
        // now occupies: the picker composes results above preview, leaving no slot beneath them.
        let delegate = &self.picker.read(cx).delegate;
        let summary = match (delegate.entries.len(), delegate.rows.len()) {
            (0, _) => "No changes".to_string(),
            (total, _) if !delegate.query_is_active => format!("{total} changed files"),
            (total, shown) => format!("{shown} of {total} changed files"),
        };

        v_flex()
            .key_context("IdeaGit")
            .track_focus(&self.focus_handle)
            .capture_action(cx.listener(Self::cancel))
            .child(self.picker.clone())
            .child(
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
                    ),
            )
    }
}

impl Focusable for IdeaGit {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for IdeaGit {}

impl ModalView for IdeaGit {}

/// The diff of one file, shown whole rather than as a list of hunks.
///
/// `picker_preview`'s backend cannot do this: it shows a plain buffer, and a diff needs a
/// [`BufferDiff`](buffer_diff::BufferDiff) registered on the multibuffer. Implementing
/// [`PreviewBackend`] here keeps that out of the shared crate.
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
            // would sit inside the 36px and leave this header a pixel shorter than the picker's
            // search bar, which is built exactly this way.
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

/// The picker drives its preview through this, on the [`App`] rather than on our own context.
struct DiffPreviewHandle(Entity<DiffPreview>);

impl PreviewBackend for DiffPreviewHandle {
    fn update(&self, update: picker::PreviewUpdate, _window: &mut Window, cx: &mut App) {
        let PreviewSource::Path(abs_path) = update.source else {
            return;
        };
        self.0.update(cx, |preview, cx| {
            let path = preview.project.read(cx).find_project_path(&abs_path, cx);
            match path {
                Some(path) => preview.show(path, cx),
                None => preview.clear(cx),
            }
        });
    }

    fn render(&self, _layout: PreviewLayout, _cx: &mut App) -> AnyElement {
        self.0.clone().into_any_element()
    }

    fn adjust_to_new_size(&self, _window: &mut Window, _cx: &mut App) {}

    fn clear(&self, cx: &mut App) {
        self.0.update(cx, |preview, cx| preview.clear(cx));
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

/// A directory while the tree is being built. Files are indices into `entries`.
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
    /// The same location as `project_path`, in the form the preview takes it.
    abs_path: Option<PathBuf>,
    status: FileStatus,
    diff_stat: Option<DiffStat>,
    /// Repository-relative path, both matched against the query and shown in the row.
    display_path: String,
    file_name: String,
}

pub struct IdeaGitDelegate {
    /// The picker reports dismissal to its delegate; the modal only closes once that is passed
    /// on as a `DismissEvent`.
    modal: WeakEntity<IdeaGit>,
    /// The modal's focus handle, for telling the picker that focus is still inside it.
    modal_focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    /// `None` when the project has no git repository, which shows as an empty list.
    repository: Option<Entity<Repository>>,
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
    /// The picker's current preview layout, reported by `preview_layout_changed`.
    preview_layout: PreviewLayout,
    /// Whether the user wants a preview at all. Separate from the layout, which is also hidden
    /// when there is no file to show.
    preview_enabled: bool,
    /// Absent until the repository's commit buffer has opened, which happens in the background
    /// when the modal opens.
    commit_editor: Option<Entity<Editor>>,
    /// Set while a commit is in flight, so the button cannot be pressed twice.
    committing: bool,
    /// Set while a push is in flight.
    pushing: bool,
    /// The commit options, all off by default and toggled from the commit button's menu.
    amend: bool,
    signoff: bool,
    skip_hooks: bool,
    /// So the modal knows its own popover holds focus rather than treating it as focus leaving.
    commit_menu_handle: PopoverMenuHandle<ContextMenu>,
    cancel_flag: Arc<AtomicBool>,
}

impl IdeaGitDelegate {
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
                let abs_path = project_path
                    .as_ref()
                    .and_then(|path| project.absolute_path(path, cx));

                ChangedFile {
                    project_path,
                    abs_path,
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

    /// Stages or unstages `paths`, which is what the checkboxes do: it moves files in and out of
    /// what the next commit will contain, the same as `git add` and `git restore --staged`.
    ///
    /// The list is not updated here. Staging emits `RepositoryEvent::StatusesChanged`, which the
    /// modal is subscribed to, so the checkbox settles when the repository confirms it.
    fn set_staged(&mut self, paths: Vec<RepoPath>, staged: bool, cx: &mut Context<Picker<Self>>) {
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
    fn toggle_all_directories(&mut self, cx: &mut Context<Picker<Self>>) {
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

    fn has_staged_changes(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.status.staging().has_staged())
    }

    /// Stages everything, or unstages everything once it all is, so one button covers both.
    /// Deliberately ignores the filter: "all" that meant "all the ones you can see" would be a
    /// trap when the list is narrowed.
    fn toggle_stage_all(&mut self, cx: &mut Context<Picker<Self>>) {
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

    /// The message as typed. `None` when the editor has not opened yet or holds only whitespace,
    /// which is also what disables the commit button.
    fn commit_message(&self, cx: &App) -> Option<String> {
        let message = self.commit_editor.as_ref()?.read(cx).text(cx);
        (!message.trim().is_empty()).then_some(message)
    }

    /// Commits what is staged. With nothing staged, git would commit nothing at all, so the
    /// tracked changes are staged first — matching what the git panel does, and what `git commit
    /// -a` would do. Untracked files are never swept in.
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
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

        cx.spawn_in(window, async move |picker, cx| {
            if let Some(paths) = unstaged_tracked {
                repository
                    .update(cx, |repository, cx| repository.stage_entries(paths, cx))
                    .await?;
            }
            let committed = repository.update(cx, |repository, cx| {
                repository.commit(message.into(), None, options, askpass, cx)
            });
            let result = committed.await?;

            picker.update_in(cx, |picker, window, cx| {
                picker.delegate.committing = false;
                if result.is_ok()
                    && let Some(editor) = picker.delegate.commit_editor.clone()
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
    fn push(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
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

        cx.spawn_in(window, async move |picker, cx| {
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

            picker.update(cx, |picker, cx| {
                picker.delegate.pushing = false;
                cx.notify();
            })?;
            pushed
        })
        .detach_and_log_err(cx);
    }

    /// The commit button's menu: the options that change what the next commit does. They map
    /// straight onto `CommitOptions`, so nothing here is stored beyond the three flags.
    fn render_commit_menu(&self, cx: &mut Context<Picker<Self>>) -> impl IntoElement {
        let has_previous_commit = self
            .repository
            .as_ref()
            .is_some_and(|repository| repository.read(cx).head_commit.is_some());
        let (amend, signoff, skip_hooks) = (self.amend, self.signoff, self.skip_hooks);
        let picker = cx.entity();

        PopoverMenu::new("idea-git-commit-menu")
            .trigger(split_button_chevron(
                "idea-git-commit-menu-trigger",
                self.commit_menu_handle.is_deployed(),
            ))
            .with_handle(self.commit_menu_handle.clone())
            .anchor(gpui::Anchor::BottomRight)
            .menu(move |window, cx| {
                let toggle = |picker: &Entity<Picker<IdeaGitDelegate>>,
                              apply: fn(&mut IdeaGitDelegate)| {
                    let picker = picker.clone();
                    move |_: &mut Window, cx: &mut App| {
                        picker.update(cx, |picker, cx| {
                            apply(&mut picker.delegate);
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
                            toggle(&picker, |delegate| delegate.amend = !delegate.amend),
                        )
                    })
                    .toggleable_entry(
                        "Signoff",
                        signoff,
                        IconPosition::Start,
                        None,
                        toggle(&picker, |delegate| delegate.signoff = !delegate.signoff),
                    )
                    .toggleable_entry(
                        "Skip hooks",
                        skip_hooks,
                        IconPosition::Start,
                        None,
                        toggle(&picker, |delegate| {
                            delegate.skip_hooks = !delegate.skip_hooks
                        }),
                    )
                }))
            })
    }

    /// Git may need a passphrase or credentials part-way through. The prompt is a workspace
    /// modal, so it takes this one's place; answering it lets the commit finish.
    fn askpass_delegate(
        &self,
        operation: &'static str,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
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

    fn toggle_staged_for_row(&mut self, row: usize, cx: &mut Context<Picker<Self>>) {
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

    fn selected_entry(&self) -> Option<&ChangedFile> {
        match self.rows.get(self.selected_index)? {
            Row::File { entry, .. } => self.entries.get(*entry),
            _ => None,
        }
    }

    /// Collapses or expands the selected directory. Does nothing while a query is active, where
    /// the tree is shown fully expanded regardless.
    fn toggle_selected_directory(&mut self, cx: &mut Context<Picker<Self>>) {
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

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(path) = self
            .selected_entry()
            .and_then(|entry| entry.project_path.clone())
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let modal = self.modal.clone();

        cx.spawn_in(window, async move |_, cx| {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_path(path, None, true, window, cx)
                })?
                .await?;
            modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn cancel_running_filter(&mut self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
        self.cancel_flag = Arc::new(AtomicBool::new(false));
    }
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

impl PickerDelegate for IdeaGitDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "IdeaGit"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Filter changed files…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(if self.repository.is_none() {
            "No git repository".into()
        } else if self.entries.is_empty() {
            "No changes".into()
        } else {
            "No matching files".into()
        })
    }

    fn match_count(&self) -> usize {
        self.rows.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Hovering must not move the selection: the diff preview will follow it, and passing the
    /// pointer over the list would load a buffer per row.
    fn select_on_hover(&self) -> bool {
        false
    }

    /// The picker dismisses itself when its query input blurs. Clicking the preview does exactly
    /// that, so report focus that merely moved elsewhere in the modal.
    fn has_another_open_menu(&self, window: &Window, cx: &App) -> bool {
        self.modal_focus_handle.contains_focused(window, cx)
            || self.commit_menu_handle.is_deployed()
    }

    fn set_selected_index(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = index;
        cx.notify();
    }

    fn indent_size(&self, _cx: &App) -> Option<Pixels> {
        Some(px(INDENT_PER_DEPTH))
    }

    /// Further right than a plain `ListItem` row, so the guides land under the middle of the
    /// folder icon rather than to its left: our rows put an indent spacer before that icon.
    fn indent_guide_left_offset(&self, _cx: &App) -> Pixels {
        px(INDENT_GUIDE_LEFT_OFFSET)
    }

    fn visible_depths(&self, range: Range<usize>) -> Vec<usize> {
        range
            .filter_map(|index| self.rows.get(index).map(Row::depth))
            .collect()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
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

        cx.spawn_in(window, async move |picker, cx| {
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

            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches
                        .into_iter()
                        .map(|entry| entry.candidate_id)
                        .collect();
                    picker.delegate.selected_index = 0;
                    picker.delegate.rebuild_rows();
                    cx.notify();
                })
                .log_err();
        })
    }

    /// The preview loads the buffer and its diff itself, so a path is all it needs. There is no
    /// match to scroll to: the whole file is shown, from the top.
    fn try_get_preview_data_for_match(&self, _cx: &App) -> Option<PreviewUpdate> {
        let abs_path = self.selected_entry()?.abs_path.clone()?;
        Some(PreviewUpdate {
            source: PreviewSource::Path(abs_path),
            match_location: None,
        })
    }

    /// The picker reports its layout here, at construction and on every change, which is the only
    /// way to know it: `Picker::preview_layout` is private, and reading the picker from a delegate
    /// method would panic anyway, since the picker is mid-update while it renders.
    fn preview_layout_changed(&mut self, layout: PreviewLayout) {
        self.preview_layout = layout;
    }

    /// Enter opens a file, and collapses or expands a directory.
    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        match self.rows.get(self.selected_index) {
            Some(Row::Directory { .. }) => self.toggle_selected_directory(cx),
            _ => self.open_selected(window, cx),
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.cancel_running_filter();
        self.modal
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let row = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse);

        // Staging is the checkbox's own job, so it stops the click before the row treats it as a
        // selection or an open.
        let stage_checkbox = |state: StageStatus, cx: &mut Context<Picker<Self>>| {
            let picker = cx.entity();
            Checkbox::new(("stage", ix), stage_toggle_state(state)).on_click(
                move |_, window, cx| {
                    picker.update(cx, |picker, cx| {
                        picker.delegate.toggle_staged_for_row(ix, cx);
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
                let picker = cx.entity();
                Some(
                    row.toggle_state(selected)
                        .on_click(move |_, window, cx| {
                            picker.update(cx, |picker, cx| {
                                let query_input = picker.focus_handle(cx);
                                window.focus(&query_input, cx);
                                picker.set_selected_index(ix, None, false, window, cx);
                                picker.delegate.toggle_selected_directory(cx);
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

                // The picker opens a match on a single click. Rows take the click first so a
                // single click only selects, and a double click opens.
                let picker = cx.entity();
                Some(
                    row.toggle_state(selected)
                        .on_click(move |event: &ClickEvent, window, cx| {
                            let opening = event.click_count() >= 2;
                            picker.update(cx, |picker, cx| {
                                // Clicking the preview will leave focus in its editor, where the
                                // arrow keys move a cursor. Returning to the list takes it back.
                                let query_input = picker.focus_handle(cx);
                                window.focus(&query_input, cx);

                                picker.set_selected_index(ix, None, false, window, cx);
                                if opening {
                                    picker.delegate.open_selected(window, cx);
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

    /// Turns the filter row into the repository's header: which branch, how far ahead or behind,
    /// and the two controls that act on the whole list.
    fn searchbar_trailer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let collapse_all = self.any_directory_expanded();
        let preview_enabled = self.preview_enabled;
        let picker = cx.entity();

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
                        let picker = picker.clone();
                        move |_, _window, cx| {
                            picker.update(cx, |picker, cx| {
                                picker.delegate.toggle_all_directories(cx);
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
                            picker.update(cx, |picker, cx| {
                                picker.delegate.preview_enabled = !preview_enabled;
                                // `sync_preview_layout` applies it on the next render.
                                cx.notify();
                            });
                        }),
                )
                .into_any_element(),
        )
    }

    /// The commit section: the message editor and the actions that operate on the whole
    /// repository. The picker renders this at the bottom of the results column — under the tree,
    /// beside the preview — and renders it even with no matches, so narrowing the filter to
    /// nothing cannot unmount the editor being typed into.
    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
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
        let picker = cx.entity();

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
                                let picker = picker.clone();
                                move |_, _window, cx| {
                                    picker.update(cx, |picker, cx| {
                                        picker.delegate.toggle_stage_all(cx);
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
                                let picker = picker.clone();
                                move |_, window, cx| {
                                    picker.update(cx, |picker, cx| {
                                        picker.delegate.commit(window, cx);
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
                                picker.update(cx, |picker, cx| {
                                    picker.delegate.push(window, cx);
                                });
                            }),
                        ),
                )
                .into_any_element(),
        )
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
