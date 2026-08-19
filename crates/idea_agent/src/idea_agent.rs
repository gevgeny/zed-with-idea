//! The agent panel in a window of its own.
//!
//! Kept in its own crate so it stays mergeable with upstream Zed. Unlike the git window, nothing
//! here reimplements the panel: `AgentPanel` is a view, and a view can be rendered by any window,
//! so this hosts the very entity the dock already owns. The two show the same conversation
//! because they are the same panel.
//!
//! Rendering one entity in two windows is safe in a way that rendering it twice in one window is
//! not: gpui keys element state by `(GlobalElementId, TypeId)` per window, and tracks focus per
//! window, so the dock's copy and this one do not collide.

use agent_ui::AgentPanel;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Pixels, Render,
    Styled, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, actions, point, px,
};
use project::Project;
use theme::ActiveTheme as _;
use ui::{prelude::*, v_flex};
use util::ResultExt as _;
use workspace::Workspace;

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

/// Vertical room for the traffic lights, which float over the content because the titlebar is
/// transparent. The panel draws its own header at the top, so unlike the git window there is no
/// column of ours to pad from the inside.
const TITLEBAR_INSET: Pixels = px(28.);

pub fn init(cx: &mut App) {
    cx.observe_new(register).detach();
}

fn register(workspace: &mut Workspace, _window: Option<&mut Window>, _: &mut Context<Workspace>) {
    workspace.register_action(move |workspace, _: &Toggle, window, cx| {
        let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
            return;
        };
        let project = workspace.project().clone();
        let handle = cx.entity().downgrade();
        toggle_window(handle, project, panel, window, cx);
    });
}

/// Opens the window for this project, or brings the existing one forward. One window per project,
/// found by asking the app for its windows and matching on the project entity — the same lookup
/// the settings window uses to keep itself unique.
fn toggle_window(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    panel: Entity<AgentPanel>,
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
    let dock = workspace
        .update(cx, |workspace, cx| {
            let position = workspace::dock::PanelHandle::position(&panel, window, cx);
            workspace.dock_at_position(position).downgrade()
        })
        .log_err();
    if let Some(dock) = dock {
        dock.update(cx, |dock, cx| dock.set_open(false, window, cx))
            .log_err();
    }

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
                let view = cx.new(|_| IdeaAgentWindow { project, panel });
                // Focusing the window is not the same as focusing something in it; without this
                // the first keypress goes nowhere until the user clicks.
                window.focus(&view.focus_handle(cx), cx);
                view
            },
        )
        .log_err();
    });
}

/// A host for the panel and nothing else. Everything the window can do, the panel already did.
pub struct IdeaAgentWindow {
    /// Only to tell one window from another when the action fires again.
    project: Entity<Project>,
    panel: Entity<AgentPanel>,
}

impl Render for IdeaAgentWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().colors().panel_background)
            // The titlebar is transparent, so the traffic lights sit over the panel's own header.
            .pt(TITLEBAR_INSET)
            .child(self.panel.clone())
    }
}

impl Focusable for IdeaAgentWindow {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.panel.focus_handle(cx)
    }
}
