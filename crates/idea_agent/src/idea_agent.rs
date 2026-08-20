//! The agent panel in a window of its own.
//!
//! Opened with cmd-alt-0, or ctrl-alt-9 elsewhere: ctrl-alt-0 is already
//! `workspace::ResetActiveDockSize` on those platforms.
//!
//! Kept in its own crate so it stays mergeable with upstream Zed. Unlike the git window, nothing
//! here reimplements the panel: `AgentPanel` is a view, and a view can be rendered by any window,
//! so this hosts the very entity the dock already owns. The two show the same conversation
//! because they are the same panel.
//!
//! Rendering one entity in two windows is safe in a way that rendering it twice in one window is
//! not: gpui keys element state by `(GlobalElementId, TypeId)` per window, and tracks focus per
//! window, so the dock's copy and this one do not collide.

use agent_ui::{
    Agent, AgentPanel, AgentThreadSource, ThreadId,
    thread_metadata_store::ThreadMetadata,
    threads_archive_view::{ThreadsArchiveView, ThreadsArchiveViewEvent},
};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Pixels, Render,
    Styled, Subscription, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions,
    actions, point, px,
};
use project::Project;
use theme::ActiveTheme as _;
use ui::{IconButton, Tooltip, h_flex, prelude::*, v_flex};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace, dock::Dock};

actions!(
    idea_agent,
    [
        /// Opens the agent panel in its own window.
        Toggle
    ]
);

/// What the window opens at the first time. After that the operating system remembers its size
/// and position, which is most of the point of being a window.
const DEFAULT_WINDOW_SIZE: gpui::Size<Pixels> = gpui::Size {
    width: px(520.),
    height: px(820.),
};

/// Horizontal room for the traffic lights, which float over the content because the titlebar is
/// transparent. The tab row is ours, so padding it leaves the hosted views alone — padding the
/// window itself would have indented everything they draw.
const TRAFFIC_LIGHT_INSET: Pixels = px(76.);

/// Where the threads list sits relative to the conversation.
#[derive(PartialEq, Clone, Copy)]
enum ThreadsSide {
    Hidden,
    Left,
    Right,
}

/// Width of the threads pane, and the bounds the divider can drag it between.
const DEFAULT_THREADS_WIDTH: Pixels = px(280.);
const MIN_THREADS_WIDTH: Pixels = px(180.);
const MAX_THREADS_WIDTH: Pixels = px(560.);

/// How wide the divider is to grab, as opposed to the one pixel it draws.
const DIVIDER_HANDLE_WIDTH: f32 = 9.;

/// Carried by the divider drag. Empty because the width is read from the pointer position, not
/// accumulated — a drag that starts mid-gesture still lands where the cursor is.
struct DividerDrag;

/// Rendered while dragging. Nothing is dragged visually; the divider itself moves.
struct DividerPreview;

impl Render for DividerPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(register).detach();
}

fn register(workspace: &mut Workspace, _window: Option<&mut Window>, _: &mut Context<Workspace>) {
    workspace.register_action(move |workspace, _: &Toggle, window, cx| {
        let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
            return;
        };
        let project = workspace.project().clone();
        // Looked up here, while the workspace is already in hand. An action handler runs inside
        // `workspace.update`, so anything that leases it again panics.
        let position = workspace::dock::PanelHandle::position(&panel, window, cx);
        let dock = workspace.dock_at_position(position).downgrade();
        let handle = cx.entity().downgrade();
        toggle_window(project, panel, handle, dock, window, cx);
    });
}

/// Opens the window for this project, or brings the existing one forward. One window per project,
/// found by asking the app for its windows and matching on the project entity — the same lookup
/// the settings window uses to keep itself unique.
fn toggle_window(
    project: Entity<Project>,
    panel: Entity<AgentPanel>,
    workspace: WeakEntity<Workspace>,
    dock: WeakEntity<Dock>,
    window: &mut Window,
    cx: &mut App,
) {
    let existing = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<IdeaAgentWindow>())
        .find(|window| window.read(cx).is_ok_and(|agent| agent.project == project));

    if let Some(existing) = existing {
        existing
            .update(cx, |_, window, _| window.activate_window())
            .log_err();
        return;
    }

    // The panel is about to be shown somewhere else, so take the dock down with it: two copies of
    // one conversation on screen at once is confusing rather than useful, and the dock is the one
    // the user just asked to leave.
    dock.update(cx, |dock, cx| dock.set_open(false, window, cx))
        .log_err();

    // Deferred to get the workspace off the stack, as the settings window does.
    cx.defer(move |cx| {
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("Agent".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.), px(12.))),
                }),
                focus: true,
                show: true,
                is_movable: true,
                kind: gpui::WindowKind::Normal,
                window_background: cx.theme().window_background_appearance(),
                window_min_size: Some(gpui::Size {
                    width: px(300.),
                    height: px(320.),
                }),
                window_bounds: Some(WindowBounds::centered(DEFAULT_WINDOW_SIZE, cx)),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|cx| IdeaAgentWindow::new(project, panel, workspace, window, cx));
                // Focusing the window is not the same as focusing something in it; without this
                // the first keypress goes nowhere until the user clicks.
                window.focus(&view.focus_handle(cx), cx);
                view
            },
        )
        .log_err();
    });
}

/// A host for two of Zed's own views, and nothing else. Everything the window can do, they
/// already did.
pub struct IdeaAgentWindow {
    /// Only to tell one window from another when the action fires again.
    project: Entity<Project>,
    /// The dock's own panel, not a copy: the window and the dock show the same conversation.
    /// Swapped when a thread moves us to another workspace, since panels belong to one.
    panel: Entity<AgentPanel>,
    /// Kept so a thread can be loaded into whichever workspace it belongs to.
    workspace: WeakEntity<Workspace>,
    /// Ours, unlike the panel. The sidebar builds its archive on demand and drops it on close,
    /// so there is no shared instance to borrow — only the code is reused, not the state.
    threads: Entity<ThreadsArchiveView>,
    threads_side: ThreadsSide,
    /// Which thread the list is currently highlighting, so the panel is only pushed at it when
    /// the answer actually changes.
    highlighted_thread: Option<ThreadId>,
    threads_width: Pixels,
    _threads_subscriptions: [Subscription; 2],
}

impl IdeaAgentWindow {
    fn new(
        project: Entity<Project>,
        panel: Entity<AgentPanel>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let threads = cx.new(|cx| {
            ThreadsArchiveView::new(
                workspace.clone(),
                panel.read(cx).connection_store().downgrade(),
                project.read(cx).agent_server_store().downgrade(),
                window,
                cx,
            )
        });

        let _threads_subscription = cx.subscribe_in(
            &threads,
            window,
            |this, _, event: &ThreadsArchiveViewEvent, window, cx| match event {
                // The archive asks to be closed; here that means hiding the pane.
                ThreadsArchiveViewEvent::Close => {
                    this.threads_side = ThreadsSide::Hidden;
                    cx.notify();
                }
                // Without this a clicked thread spins forever: the archive only announces the
                // choice, and the sidebar's own handler routes it into its private state.
                ThreadsArchiveViewEvent::Activate { thread } => {
                    this.activate_thread(thread.clone(), window, cx);
                }
                _ => {}
            },
        );

        // The archive's history arrives in the background, and it notifies itself rather than us
        // when it does. Without this the highlight would have nothing to attach to on the first
        // frame and would never be retried. Safe against looping: once the row is found, the
        // sync leaves early and stops notifying.
        let _redraw_on_history = cx.observe(&threads, |_, _, cx| cx.notify());

        Self {
            project,
            panel,
            workspace,
            threads,
            threads_side: ThreadsSide::Left,
            highlighted_thread: None,
            threads_width: DEFAULT_THREADS_WIDTH,
            _threads_subscriptions: [_threads_subscription, _redraw_on_history],
        }
    }

    /// Opens a thread, moving this window — and the editor — to the worktree it belongs to.
    ///
    /// A thread records the folders it was held against, and loading one into the wrong
    /// workspace makes the panel re-record it there, silently rewriting where the conversation
    /// lives. So the workspace is switched first and the thread loaded into *its* panel, which
    /// is what Zed's own sidebar does.
    fn activate_thread(
        &mut self,
        thread: ThreadMetadata,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let thread_id = thread.thread_id;
        // Whatever happens next, the archive has to stop spinning: it refuses a second click on
        // a thread it still believes is restoring.
        self.threads.update(cx, |threads, cx| {
            threads.clear_restoring(&thread_id, cx);
        });

        let workspace = if self.holds(&thread, cx) {
            self.workspace.upgrade()
        } else {
            // Only worktrees already open in some window. Opening one that is not runs through
            // the sidebar's private machinery, so those are left to it.
            self.find_open_workspace(&thread, cx)
        };
        let Some(workspace) = workspace else {
            return;
        };

        // The panel belongs to the workspace, so following one means following the other. Same
        // for the project, or the next `cmd-alt-0` would not find this window and would open a
        // second one beside it.
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };
        self.project = workspace.read(cx).project().clone();
        self.workspace = workspace.downgrade();
        self.panel = panel.clone();

        panel.update(cx, |panel, cx| {
            panel.load_agent_thread(
                Agent::from(thread.agent_id.clone()),
                thread_id,
                Some(thread.folder_paths().clone()),
                thread.title.clone(),
                true,
                AgentThreadSource::Sidebar,
                window,
                cx,
            );
        });
        cx.notify();
    }

    /// Points the list at whatever conversation the panel is showing.
    ///
    /// The archive highlights only what the keyboard moved to, so without this the open thread
    /// is never marked — on opening the window, or when the thread is switched from the panel
    /// side. Driven from `render` because the archive loads its history in the background, so
    /// there is nothing to select until it has.
    fn sync_highlight(&mut self, cx: &mut Context<Self>) {
        let active = self.panel.read(cx).active_thread_id(cx);
        if active == self.highlighted_thread {
            return;
        }
        let Some(thread_id) = active else {
            self.highlighted_thread = None;
            return;
        };
        // Only remembered once the row actually exists. The archive loads its history in the
        // background, so on opening the window there is nothing to select yet, and recording the
        // attempt would mean never trying again.
        let selected = self
            .threads
            .update(cx, |threads, cx| threads.select_thread(&thread_id, cx));
        if selected {
            self.highlighted_thread = active;
        }
    }

    /// Whether a thread was recorded against exactly the folders this window's project holds.
    fn holds(&self, thread: &ThreadMetadata, cx: &App) -> bool {
        let folders = thread.folder_paths();
        folders.is_empty()
            || self
                .project
                .read(cx)
                .worktree_paths(cx)
                .folder_path_list()
                .paths()
                == folders.paths()
    }

    /// The already-open workspace whose folders are the ones this thread was recorded against,
    /// brought to the front of its window on the way — which is what moves the editor with us.
    fn find_open_workspace(
        &self,
        thread: &ThreadMetadata,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Workspace>> {
        let wanted = thread.folder_paths().paths().to_vec();
        let found = cx.windows().into_iter().find_map(|handle| {
            let handle = handle.downcast::<MultiWorkspace>()?;
            let multi_workspace = handle.read(cx).ok()?;
            let workspace = multi_workspace
                .workspaces()
                .find(|workspace| {
                    workspace
                        .read(cx)
                        .root_paths(cx)
                        .iter()
                        .map(|path| path.to_path_buf())
                        .collect::<Vec<_>>()
                        == wanted
                })?
                .clone();
            Some((handle, workspace))
        });

        let (handle, workspace) = found?;
        handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.activate(workspace.clone(), None, window, cx);
            })
            .log_err();
        Some(workspace)
    }

    /// The window's own strip: room for the traffic lights, and the one control that is ours.
    ///
    /// Three `IconButton`s rather than a `ToggleButtonGroup`, which always draws its labels — the
    /// icons already say left, right and closed.
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let side = self.threads_side;
        let button = |id: &'static str, tooltip: &'static str, icon, which| {
            IconButton::new(id, icon)
                .icon_size(IconSize::Small)
                .toggle_state(side == which)
                .tooltip(Tooltip::text(tooltip))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.threads_side = which;
                    cx.notify();
                }))
        };

        h_flex()
            .flex_none()
            .w_full()
            .h_9()
            .pl(TRAFFIC_LIGHT_INSET)
            .pr_2()
            .justify_end()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_px()
                    .p_px()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(button(
                        "idea-agent-threads-left",
                        "Threads Left",
                        IconName::ThreadsSidebarLeftOpen,
                        ThreadsSide::Left,
                    ))
                    .child(button(
                        "idea-agent-threads-hidden",
                        "Hide Threads",
                        IconName::ThreadsSidebarLeftClosed,
                        ThreadsSide::Hidden,
                    ))
                    .child(button(
                        "idea-agent-threads-right",
                        "Threads Right",
                        IconName::ThreadsSidebarRightOpen,
                        ThreadsSide::Right,
                    )),
            )
    }

    /// The draggable seam between the two panes: a one-pixel line with a wider invisible handle
    /// centred on it, so it looks like every other divider but is possible to grab. Same shape as
    /// the split editor's own handle.
    fn render_divider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .w_px()
            .h_full()
            .flex_none()
            .bg(cx.theme().colors().border)
            .child(
                div()
                    .id("idea-agent-divider")
                    .absolute()
                    .left(px(-DIVIDER_HANDLE_WIDTH / 2.))
                    .w(px(DIVIDER_HANDLE_WIDTH))
                    .h_full()
                    .cursor_col_resize()
                    .block_mouse_except_scroll()
                    .on_drag(DividerDrag, |_, _, _, cx| cx.new(|_| DividerPreview))
                    .on_drag_move::<DividerDrag>(cx.listener(
                        |this, event: &gpui::DragMoveEvent<DividerDrag>, _window, cx| {
                            this.threads_width = event
                                .event
                                .position
                                .x
                                .clamp(MIN_THREADS_WIDTH, MAX_THREADS_WIDTH);
                            cx.notify();
                        },
                    )),
            )
    }
}

impl Render for IdeaAgentWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_highlight(cx);

        // Capped against the window as well as its own bounds: the window can be dragged
        // narrower than the pane's stored width.
        let width = self.threads_width.min(window.viewport_size().width * 0.7);

        let threads = div()
            .w(width)
            .h_full()
            .flex_none()
            // ponytail: the archive's header sizes itself as a titlebar and pulls itself up a
            // pixel under client decorations, so it sits a pixel above the agent panel's toolbar.
            // Nudging the whole pane back down lines the two headers up; it also moves the list
            // by a pixel, which is the trade. Drop this if upstream stops doing it.
            .mt(px(1.))
            // A row's title fades out under a *solid* patch of what `ThreadItem` assumes the
            // sidebar's background is. The archive paints no background of its own, so unless
            // this pane matches that assumption every fade shows as a rectangle.
            .bg(sidebar_background(cx))
            .child(self.threads.clone());
        let panel = div().flex_1().min_w_0().h_full().child(self.panel.clone());

        v_flex()
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(h_flex().flex_1().min_h_0().map(|this| {
                match self.threads_side {
                    ThreadsSide::Hidden => this.child(panel),
                    ThreadsSide::Left => this
                        .child(threads)
                        .child(self.render_divider(cx))
                        .child(panel),
                    ThreadsSide::Right => this
                        .child(panel)
                        .child(self.render_divider(cx))
                        .child(threads),
                }
            }))
    }
}

/// What `ui::ThreadItem` blends its title fade into when nobody sets `base_bg`: the sidebar's
/// background, which the archive is normally drawn on. Copied rather than exposed, since it is a
/// local in `thread_item.rs`.
fn sidebar_background(cx: &App) -> gpui::Hsla {
    let colors = cx.theme().colors();
    colors
        .title_bar_background
        .blend(colors.panel_background.opacity(0.25))
}

impl Focusable for IdeaAgentWindow {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }
}
