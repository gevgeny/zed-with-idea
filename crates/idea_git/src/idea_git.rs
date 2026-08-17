//! A JetBrains-style git modal: the changed files of the active repository, filterable, with a
//! diff of the selected one.
//!
//! Kept in its own crate so it stays mergeable with upstream Zed. It deliberately does not reuse
//! `GitPanel`: the panel's list, selection and staging state are private to `git_ui`, and hosting
//! its renderers would mean hiding the dock to avoid rendering the same entity twice. Everything
//! here goes straight to `project`'s public git API instead.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use editor::{Editor, EditorSettings};
use file_icons::FileIcons;
use fuzzy::StringMatchCandidate;
use git::{
    repository::RepoPath,
    status::{DiffStat, FileStatus},
};
use gpui::{
    AnyElement, App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, IntoElement, ParentElement, Render, SharedString, Styled, Subscription, Task,
    WeakEntity, Window, actions,
};
use language::Point;
use multi_buffer::MultiBuffer;
use picker::{
    Picker, PickerDelegate, PreviewBackend, PreviewLayout, PreviewSource, PreviewUpdate,
    SetPreviewBelow, SetPreviewHidden,
};
use project::{
    Project, ProjectPath,
    git_store::{Repository, RepositoryEvent},
};
use settings::Settings as _;
use ui::{Icon, Label, ListItem, ListItemSpacing, h_flex, prelude::*, v_flex};
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
const VERSION: &str = "0.3.p4";

/// Wide enough for a path plus its diff stat, and shared with the diff preview later.
const MODAL_WIDTH: Rems = Rems(46.);

/// Upper bound on rows kept after filtering. A repository with more changed files than this is
/// past the point where scrolling a list helps.
const MAX_MATCHES: usize = 2_000;

const NO_SELECTION_MESSAGE: &str = "Select a file to see its diff";
const LOAD_FAILED_MESSAGE: &str = "Could not load this file's diff";

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
            selected_index: 0,
            preview_layout: PreviewLayout::Hidden,
            preview_enabled: true,
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

    /// Puts the preview below the list, since the picker itself opens with it hidden.
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
            PreviewLayout::Below
        } else {
            PreviewLayout::Hidden
        };
        if delegate.preview_layout == wanted {
            return;
        }

        let action: &dyn gpui::Action = if wanted == PreviewLayout::Hidden {
            &SetPreviewHidden
        } else {
            &SetPreviewBelow
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

        v_flex()
            .key_context("IdeaGit")
            .track_focus(&self.focus_handle)
            .capture_action(cx.listener(Self::cancel))
            .child(self.picker.clone())
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
            message: Some(NO_SELECTION_MESSAGE.into()),
            pending_update: Task::ready(()),
        }
    }

    fn show(&mut self, path: ProjectPath, cx: &mut Context<Self>) {
        if self.current_path.as_ref() == Some(&path) {
            return;
        }
        self.current_path = Some(path.clone());
        let project = self.project.clone();

        self.pending_update = cx.spawn(async move |this, cx| {
            let loaded = load_diff(project, path, cx).await;
            this.update(cx, |this, cx| match loaded {
                Ok((buffer, diff)) => {
                    this.message = None;
                    this.multibuffer.update(cx, |multibuffer, cx| {
                        // The whole file as one excerpt, rather than one excerpt per hunk: this
                        // pane is for reading a file that happens to have changed, not for
                        // reviewing every change in the repository at once.
                        let full_file = Point::zero()..buffer.read(cx).max_point();
                        multibuffer.set_excerpts_for_buffer(buffer, [full_file], 0, cx);
                        multibuffer.add_diff(diff, cx);
                    });
                    cx.notify();
                }
                Err(_) => {
                    this.message = Some(LOAD_FAILED_MESSAGE.into());
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.pending_update = Task::ready(());
        self.current_path = None;
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

impl Render for DiffPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let container = v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background);

        match self.message.clone() {
            Some(message) => container.justify_center().items_center().child(
                Label::new(message)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
            None => container.child(self.editor.clone()),
        }
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
    /// Directory part of `display_path`, absent for a file at the repository root.
    parent_dir: Option<String>,
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
    /// Indices into `entries` surviving the current query, in match order.
    matches: Vec<usize>,
    selected_index: usize,
    /// The picker's current preview layout, reported by `preview_layout_changed`.
    preview_layout: PreviewLayout,
    /// Whether the user wants a preview at all. Separate from the layout, which is also hidden
    /// when there is no file to show.
    preview_enabled: bool,
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
                let parent_dir = entry
                    .repo_path
                    .parent()
                    .filter(|parent| !parent.is_empty())
                    .map(|parent| parent.display(path_style).to_string());

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
                    parent_dir,
                }
            })
            .collect();
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(path) = self
            .matches
            .get(self.selected_index)
            .and_then(|&index| self.entries.get(index))
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

/// A single letter and colour for a status, matching what the git panel shows.
fn status_badge(status: FileStatus) -> (&'static str, Color) {
    if status.is_conflicted() {
        ("C", Color::VersionControlConflict)
    } else if status.is_untracked() {
        ("U", Color::VersionControlAdded)
    } else if status.is_created() {
        ("A", Color::VersionControlAdded)
    } else if status.is_deleted() {
        // Colouring these red would put a column of red down a list that is mostly not deletions.
        ("D", Color::Disabled)
    } else if status.is_modified() {
        ("M", Color::VersionControlModified)
    } else {
        ("•", Color::Muted)
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
        self.matches.len()
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

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.cancel_running_filter();

        if query.is_empty() {
            self.matches = (0..self.entries.len().min(MAX_MATCHES)).collect();
            self.selected_index = 0;
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
                    cx.notify();
                })
                .log_err();
        })
    }

    /// The preview loads the buffer and its diff itself, so a path is all it needs. There is no
    /// match to scroll to: the whole file is shown, from the top.
    fn try_get_preview_data_for_match(&self, _cx: &App) -> Option<PreviewUpdate> {
        let abs_path = self
            .matches
            .get(self.selected_index)
            .and_then(|&index| self.entries.get(index))?
            .abs_path
            .clone()?;
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

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.open_selected(window, cx);
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
        let entry = self
            .matches
            .get(ix)
            .and_then(|&index| self.entries.get(index))?;

        let (badge, badge_color) = status_badge(entry.status);
        let icon = FileIcons::get_icon(entry.repo_path.as_std_path(), cx).map(|path| {
            Icon::from_path(path)
                .color(Color::Muted)
                .size(IconSize::Small)
        });
        let stat = entry.diff_stat.filter(|stat| stat.added + stat.deleted > 0);

        // The picker opens a match on a single click. Rows take the click first so a single click
        // only selects, and a double click opens.
        let picker = cx.entity();
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .on_click(move |event: &ClickEvent, window, cx| {
                    let opening = event.click_count() >= 2;
                    picker.update(cx, |picker, cx| {
                        // Clicking the preview will leave focus in its editor, where the arrow
                        // keys move a cursor. Returning to the list has to take focus back.
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
                                .gap_1p5()
                                .children(icon)
                                .child(Label::new(entry.file_name.clone()).single_line())
                                .children(entry.parent_dir.clone().map(|parent| {
                                    Label::new(parent)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate()
                                })),
                        )
                        .child(
                            h_flex()
                                .flex_none()
                                .gap_1p5()
                                .children(stat.filter(|stat| stat.added > 0).map(|stat| {
                                    Label::new(format!("+{}", stat.added))
                                        .size(LabelSize::Small)
                                        .color(Color::VersionControlAdded)
                                }))
                                .children(stat.filter(|stat| stat.deleted > 0).map(|stat| {
                                    Label::new(format!("−{}", stat.deleted))
                                        .size(LabelSize::Small)
                                        .color(Color::VersionControlDeleted)
                                }))
                                .child(
                                    Label::new(badge)
                                        .size(LabelSize::Small)
                                        .color(badge_color)
                                        .single_line(),
                                ),
                        ),
                ),
        )
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let summary = match self.entries.len() {
            0 => "No changes".to_string(),
            1 => "1 changed file".to_string(),
            count if count == self.matches.len() => format!("{count} changed files"),
            count => format!("{} of {count} changed files", self.matches.len()),
        };

        Some(
            h_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_2()
                .justify_between()
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
                .into_any_element(),
        )
    }
}
