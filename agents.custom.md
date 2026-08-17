# Custom agent rules

Traps to avoid, earned from debugging this codebase. Not architecture notes, and not general
engineering advice — everything here is specific enough to act on.

Nothing loads this file automatically: `.rules` is what agents read, via the `AGENTS.md` and
`CLAUDE.md` symlinks. Point at it from there if you want it picked up, at the cost of a line of
merge surface with upstream.

## Before adding a view mode, find every handler that forwards focus

A view whose own focus handle is a pass-through — an `on_focus` handler that hands focus to a child editor — cannot host a second mode without disabling that handler for the new mode. `ProjectSearchView` does exactly this: focusing the view forwards to the results editor on the next frame.

Two failures follow, and they look unrelated:

- **A repaint loop.** If the new mode removes the child from the element tree, the forwarder focuses a handle no element owns. The frame's focus path comes out empty, focus-lost fires, the handler runs again next frame. gpui documents this shape in `Window::draw` but guards only its own focus-listener case. The visible symptom is unrelated chrome flickering — toolbar items, status bar, breadcrumbs — because they are downstream of the pane's focus broadcast, while the view actually changed looks fine.
- **Dead keybindings.** If the new mode keeps the child mounted, focus lands on the child instead, so a key context added for the new mode never wins. Arrow keys silently drive the hidden child.

Both are one cause. Guard the forwarder on the mode flag; do not try to out-focus it by calling `focus` again, because it runs a frame later and always wins.

`Pane::focus_in` also restores `last_focus_handle_by_item`, so it is a plausible second suspect — but do not stop there. In this case it was a bystander, and blaming it cost two wrong "fixes".

This only reproduces once the child has held focus: click into it first, then switch modes. That intermittency makes single manual observations worthless as evidence; confirm with a counter, not with one look.

## Diagnose repaint loops by escalating instrumentation, never by reading

Reasoning about which call in a render path *looks* expensive does not find notify loops. It produces confident wrong fixes, because the entity that spins is usually not one the changed code mentions.

Escalate in this order, stopping when the cause is named:

1. **Count call sites.** One counter per suspected entry point (each render fn, each `cx.observe`), printing its count once per second. This mostly earns its keep by showing which suspects are *quiet* — that is what rules things out.
2. **Count notifies by entity.** Temporary counter in `App::notify` (`crates/gpui/src/app.rs`), printing the top entity ids per second. Tells you whether one entity spins or a whole group does.
3. **Name the entities.** Ids alone are useless. Record `std::any::type_name::<T>()` in `EntityMap::insert` (`crates/gpui/src/app/entity_map.rs`) into a side map, and print it alongside each id. A group that shares no logic but ticks in lockstep identifies a *broadcast*, not a rogue entity — find who broadcasts.
4. **Backtrace one entity.** `std::backtrace::Backtrace::force_capture()` in `App::notify`, filtered to one type name and throttled to one print per second. This gives the exact call path and ends the search.

Step 4 is what actually resolves it. Steps 1–3 only narrow the target enough to aim it. Gate all of it behind an env var, and delete it once the cause is confirmed.

Two things that cost real time here:

- **A workaround that hides the symptom is not a diagnosis.** Covering the unmounted view stopped the flicker, which made it look solved; the cause was still there and resurfaced later as arrow keys that did nothing.
- **Verify a check can fail.** A regression test written for these guards passed with the guard deleted, because the code path it aimed at only runs on a drawn frame and a test window is never drawn. Delete the guard, watch the test go red, then put it back.
