# Project search list view — plan

## Goal

A second result view for project search (`cmd-shift-f`), modeled on JetBrains' Find in Files, switchable from the search toolbar:

1. **Match list** — one row per match: the match line with the query highlighted, right-aligned `file:line`. Scrolls on its own.
2. **Preview** — the selected match's file, scrolled to that match, matches highlighted. Scrolls on its own.

The problem it solves: today's results are a single `MultiBuffer` in one editor, so there is no way to scroll the list of files independently of file contents.

## Design decision

Two options were considered:

- **A** — add a preview pane to the existing `TextFinder` modal (`alt-cmd-f`). Smaller, and matches the JetBrains dialog literally, but stays modal.
- **B** — a second view mode inside the search tab. **Chosen**, because it survives outside a modal.

Original estimate for B assumed reusing `TextFinder`'s `Picker`, which is modal-bound. Building the list directly in the tab from data already there avoids that entirely.

## What the repo already provides

| Piece | Where |
|---|---|
| `SearchMatch` — path, buffer, anchor range, byte column, line number | `crates/search/src/text_finder.rs:492` |
| `Delegate::process_search_result` — `(buffer, ranges)` → `Vec<SearchMatch>` | `crates/search/src/text_finder/delegate.rs:1435` |
| `render_matched_line` — line text, syntax + query highlight, as `StyledText` | `crates/search/src/text_finder/delegate.rs:1315` |
| Right-aligned line-number column sizing | `max_line_number` in `Delegate` |
| `ProjectSearch.match_ranges` and the buffer/range stream | `crates/search/src/project_search.rs` |

`text_finder` is an existing JetBrains-modeled finder — worth reading before writing anything new. Its `delegate` module and `render_matched_line` are private; both need widening to reuse.

## Status

**Done** — toolbar toggle button (`IconName::ListTree`, left of the filter button) and `list_view_enabled` on `ProjectSearchView`. List mode currently renders a placeholder.

The toggle also moves focus to the view's own handle. That is not cosmetic — see the focus rule in `agents.custom.md`. Removing it reintroduces a frame-rate repaint loop that flickers the tab header.

`LIST_VIEW_VERSION` is surfaced in the toggle's tooltip so a running build can be identified while iterating. Remove before merging.

## Remaining steps

Each is one build, independently testable.

- **B** — static `v_flex` of the first N rows, real match text. No virtualization. Requires `matches: Vec<SearchMatch>` on `ProjectSearch`, populated in `consume_search_stream` and cleared alongside `match_ranges`.
- **C** — swap to `uniform_list`, no scroll handle.
- **D** — add `track_scroll` + `UniformListScrollHandle`.
- **E** — selection state, up/down via `menu::SelectNext`/`SelectPrevious`, `scroll_to_item`.
- **F** — preview pane: second `Entity<Editor>` over a singleton buffer from `project.open_buffer`, scrolled to the selected match. Do **not** reuse the results multibuffer — its scroll coupling is the thing being escaped. `picker_preview` is an existing crate worth reading first.

## Open questions

- **Key context.** In list mode `up`/`down` must drive list selection, not an editor cursor. While the list fully replaces the editor there is no conflict; once the preview pane lands, both are in the tree and the contexts must be separated. Decide before step F.
- **Persistence.** Whether the mode is per-workspace-persisted or resets each session. Also whether a setting should choose the default. Both currently unimplemented.
- **Flat vs grouped rows.** Spec is flat, one row per match. `Delegate` also keeps a grouped-with-headers view (`entries`) if that is wanted later.

## Before merging

- Remove `LIST_VIEW_VERSION` and its tooltip text.
- Confirm no debug instrumentation remains in `crates/gpui` (`App::notify`, `EntityMap::insert`).
- Per the repo's `.rules`, proposed rule additions belong in the PR description under **"Suggested .rules additions"**, not committed inline. `agents.custom.md` is currently an untracked standalone file that nothing loads.
