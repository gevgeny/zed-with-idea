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

**Done — A through E (except opening a match).** Toolbar toggle, flat `matches` list on `ProjectSearch`, virtualized `uniform_list`, scrollbar, selection with `up`/`down` and click.

The `on_focus` handler in `ProjectSearchView::new` forwards focus to the results editor one frame after the view is focused. It is guarded on `list_view_enabled`. **Removing that guard breaks list view twice over**: arrow keys silently drive the results editor, and if the editor is also unmounted, a frame-rate repaint loop flickers the tab header. See `agents.custom.md`.

`LIST_VIEW_VERSION` is surfaced in the toggle's tooltip so a running build can be identified while iterating. Remove before merging.

## Remaining steps

- **E (rest)** — `enter` opens the selected match. Undecided: open in place of the search tab, or in a new tab (matching today's `alt-enter` / `OpenExcerpts`).
- **F** — preview pane: second `Entity<Editor>` over a singleton buffer from `project.open_buffer`, scrolled to the selected match. Do **not** reuse the results multibuffer — its scroll coupling is the thing being escaped. `picker_preview` is an existing crate worth reading first.

## Performance

Not yet measured in release. Scrolling ~2k matches feels slow in a debug build. Suspects, in order: the debug build itself; `render_matched_line` taking a buffer snapshot and running tree-sitter per visible row per frame (`text_finder` does the same, so it is at least idiomatic). Measure with `cargo run --release` before optimizing anything.

## Open questions

- **Key context.** In list mode `up`/`down` must drive list selection, not an editor cursor. While the list fully replaces the editor there is no conflict; once the preview pane lands, both are in the tree and the contexts must be separated. Decide before step F.
- **Persistence.** Whether the mode is per-workspace-persisted or resets each session. Also whether a setting should choose the default. Both currently unimplemented.
- **Flat vs grouped rows.** Spec is flat, one row per match. `Delegate` also keeps a grouped-with-headers view (`entries`) if that is wanted later.

## Before merging

- Remove `LIST_VIEW_VERSION` and its tooltip text.
- Confirm no debug instrumentation remains in `crates/gpui` (`App::notify`, `EntityMap::insert`).
- Per the repo's `.rules`, proposed rule additions belong in the PR description under **"Suggested .rules additions"**, not committed inline. `agents.custom.md` is currently an untracked standalone file that nothing loads.
