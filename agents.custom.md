# Custom agent rules

Rules earned from real debugging sessions. Traps to avoid, not architecture notes.

## Keep a focusable child view mounted; do not swap it out of the element tree

Adding a view mode that renders something *else* where a focusable child view used to be leaves that child alive but unrendered, and its focus handle outside the tree.

`Pane::focus_in` restores `last_focus_handle_by_item` for its active item, and `Window::focus` accepts any handle without checking that it is rendered. So focus is set to a handle that no element owns, the frame's focus path comes out empty, focus-lost fires, the pane restores the same handle again — a frame-rate loop. gpui documents this shape at `crates/gpui/src/window.rs` in `draw`, but only guards the focus-listener case.

**Moving focus elsewhere before the swap does not fix it.** The pane's stored handle is keyed by item and outlives the focus change; it gets restored on the next focus event regardless. There is currently no public API to ask gpui whether a handle is still in the tree, so a view cannot defend itself.

Render the alternate mode *over* the existing child (absolute + `occlude`) instead of replacing it. The cost is one hidden child's layout per repaint.

Symptoms point away from the cause: unrelated chrome (toolbar items, status bar items, breadcrumbs) flickers, because it is downstream of the pane's focus broadcast, while the view actually changed looks fine. It also only reproduces once the removed view has held focus — click into it first, then switch modes, or the loop never starts. That intermittency makes single manual observations worthless as evidence; confirm with a counter, not with one look.

## Diagnose repaint loops by escalating instrumentation, never by reading

Reasoning about which call in a render path *looks* expensive does not find notify loops. It produces confident wrong fixes, because the entity that spins is usually not one the changed code mentions.

Escalate in this order, stopping when the cause is named:

1. **Count call sites.** One counter per suspected entry point (each render fn, each `cx.observe`), printing its count once per second. This mostly earns its keep by showing which suspects are *quiet* — that is what rules things out.
2. **Count notifies by entity.** Temporary counter in `App::notify` (`crates/gpui/src/app.rs`), printing the top entity ids per second. Tells you whether one entity spins or a whole group does.
3. **Name the entities.** Ids alone are useless. Record `std::any::type_name::<T>()` in `EntityMap::insert` (`crates/gpui/src/app/entity_map.rs`) into a side map, and print it alongside each id. A group that shares no logic but ticks in lockstep identifies a *broadcast*, not a rogue entity — find who broadcasts.
4. **Backtrace one entity.** `std::backtrace::Backtrace::force_capture()` in `App::notify`, filtered to one type name and throttled to one print per second. This gives the exact call path and ends the search.

Step 4 is what actually resolves it. Steps 1–3 only narrow the target enough to aim it. Gate all of it behind an env var, and delete it once the cause is confirmed.

## Bisect a broken feature by removal, not by patching

When a new feature misbehaves and the cause is not obvious, strip it back to the smallest version that still shows the bug, then re-add one piece at a time. Each step is independently testable and each answer is unambiguous.

This is faster than patching a suspected cause: a patch that does not fix it teaches nothing about what did, while a removal that does not fix it eliminates everything removed.

Corollary: when a workaround makes the symptom disappear, that is not a diagnosis. Note that it works, then keep going — a suppressed cause resurfaces somewhere with no obvious connection to the change that exposed it.
