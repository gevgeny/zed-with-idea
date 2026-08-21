//! The agent panel in a window of its own.
//!
//! Opened with cmd-alt-0, or ctrl-alt-9 elsewhere: ctrl-alt-0 is already
//! `workspace::ResetActiveDockSize` on those platforms.
//!
//! Kept in its own crate so it stays mergeable with upstream Zed. Nothing here reimplements
//! either half of what it shows: `AgentPanel` and `Sidebar` are views, and a view can be rendered
//! by any window, so this hosts the panel the dock already owns beside a sidebar of its own.
//!
//! Rendering one entity in two windows is safe in a way that rendering it twice in one window is
//! not: gpui keys element state by `(GlobalElementId, TypeId)` per window, and tracks focus per
//! window, so the dock's copy and this one do not collide.

use agent_ui::AgentPanel;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Pixels, Render,
    Styled, Subscription, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions,
    actions, point, px,
};
use sidebar::Sidebar;
use theme::ActiveTheme as _;
use ui::{IconButton, Tooltip, h_flex, prelude::*, v_flex};
use util::ResultExt as _;
use workspace::{MultiWorkspace, SidebarHandle as _, Workspace, dock::Dock};

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
    width: px(820.),
    height: px(820.),
};

/// Horizontal room for the traffic lights, which float over the content because the titlebar is
/// transparent. The tab row is ours, so padding it leaves the hosted views alone — padding the
/// window itself would have indented everything they draw.
const TRAFFIC_LIGHT_INSET: Pixels = px(76.);

/// Bumped on every change, and shown in the header so a running build can be identified while
/// iterating. Remove before this is considered finished.
const VERSION: &str = "0.2.p4";

/// Where the threads list sits relative to the conversation.
#[derive(PartialEq, Clone, Copy)]
enum ThreadsSide {
    Hidden,
    Left,
    Right,
}

/// The bounds the divider can drag the sidebar between. Its width is the sidebar's own — it
/// renders itself at whatever `set_width` was last given.
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
        let Some(multi_workspace) = workspace
            .multi_workspace()
            .and_then(|multi_workspace| multi_workspace.upgrade())
        else {
            return;
        };
        // Looked up here, while the workspace is already in hand. An action handler runs inside
        // `workspace.update`, so anything that leases it again panics.
        let position = workspace::dock::PanelHandle::position(&panel, window, cx);
        let dock = workspace.dock_at_position(position).downgrade();
        toggle_window(multi_workspace, panel, dock, window, cx);
    });
}

/// Opens the window for this editor window, or brings the existing one forward. One agent window
/// per editor window, found by asking the app for its windows and matching on the multi-workspace
/// — the same lookup the settings window uses to keep itself unique.
fn toggle_window(
    multi_workspace: Entity<MultiWorkspace>,
    panel: Entity<AgentPanel>,
    dock: WeakEntity<Dock>,
    window: &mut Window,
    cx: &mut App,
) {
    let existing = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<IdeaAgentWindow>())
        .find(|window| {
            window
                .read(cx)
                .is_ok_and(|agent| agent.multi_workspace == multi_workspace)
        });

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
                let view = cx.new(|cx| IdeaAgentWindow::new(multi_workspace, panel, window, cx));
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
    /// The editor window this one belongs to. Also what tells one agent window from another when
    /// the action fires again.
    multi_workspace: Entity<MultiWorkspace>,
    /// The panel of whichever workspace the editor window is showing, not a copy of it: this
    /// window and the dock show the same conversation. Swapped when the sidebar moves the editor
    /// to another workspace, since panels belong to one.
    panel: Entity<AgentPanel>,
    /// Ours, unlike the panel — the editor window's own sidebar is registered as *the* sidebar of
    /// its multi-workspace, and its `ListState` measures itself against that window's width. Only
    /// the instance is separate; both read the same stores, so live status and ordering match.
    threads: Entity<Sidebar>,
    threads_side: ThreadsSide,
    /// Redraws us when the panel swaps its conversation — it loads threads in the background — and
    /// when the sidebar moves the editor to another workspace, which is when the panel we render
    /// stops being the right one. Rebuilt whenever the panel itself is swapped.
    _panel_subscription: Subscription,
    _multi_workspace_subscription: Subscription,
}

impl IdeaAgentWindow {
    fn new(
        multi_workspace: Entity<MultiWorkspace>,
        panel: Entity<AgentPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let threads = cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx));

        // Deliberately not `register_sidebar`: that would make this instance *the* sidebar of the
        // editor window, replacing the one already there.
        let _multi_workspace_subscription =
            cx.observe(&multi_workspace, |this, _, cx| this.follow_workspace(cx));
        let _panel_subscription = cx.observe(&panel, |_, _, cx| cx.notify());

        Self {
            multi_workspace,
            panel,
            threads,
            threads_side: ThreadsSide::Left,
            _panel_subscription,
            _multi_workspace_subscription,
        }
    }

    /// Follows the editor window from one workspace to another.
    ///
    /// Activating a thread that belongs elsewhere switches the whole editor window, and the panel
    /// belongs to a workspace — so the one this window renders has to be swapped for the new
    /// workspace's, or it would keep showing the conversation we just navigated away from.
    fn follow_workspace(&mut self, cx: &mut Context<Self>) {
        let workspace = self.multi_workspace.read(cx).workspace().clone();
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            return;
        };
        if panel == self.panel {
            return;
        }
        self._panel_subscription = cx.observe(&panel, |_, _, cx| cx.notify());
        self.panel = panel;
        cx.notify();
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
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                Label::new(VERSION)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
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
    ///
    /// It drives the sidebar's own width rather than a width of ours, since the sidebar draws
    /// itself at whatever it was last set to.
    fn render_divider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let from_left = self.threads_side == ThreadsSide::Left;
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
                        move |this, event: &gpui::DragMoveEvent<DividerDrag>, window, cx| {
                            let x = event.event.position.x;
                            // Dragging a pane on the right grows it as the pointer moves left.
                            let width = if from_left {
                                x
                            } else {
                                window.viewport_size().width - x
                            };
                            this.threads.set_width(
                                Some(width.clamp(MIN_THREADS_WIDTH, MAX_THREADS_WIDTH)),
                                cx,
                            );
                            cx.notify();
                        },
                    )),
            )
    }
}

impl Render for IdeaAgentWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let threads = self.threads.clone();
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

impl Focusable for IdeaAgentWindow {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }
}
