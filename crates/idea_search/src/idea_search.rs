//! A JetBrains-style "find in files" modal: a list of matches with a preview of the selected
//! one.
//!
//! Kept in its own crate so it stays mergeable with upstream Zed. The only contact with the rest
//! of the tree is registering the action; everything else builds on public APIs.

use std::{
    ops::Range,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use collections::HashSet;
use futures::StreamExt;
use editor::Editor;
use gpui::{
    AnyElement, App, ClickEvent, Context, DismissEvent, Entity, EventEmitter, Focusable, FontWeight,
    HighlightStyle, IntoElement, ParentElement, Render, Styled, StyledText, Task, TextStyle,
    WeakEntity, Window, actions, relative,
};
use language::{Buffer, HighlightId, LanguageAwareStyling, OffsetRangeExt as _};
use picker::{
    MatchLocation, Picker, PickerDelegate, PreviewSource, PreviewUpdate, SetPreviewBelow,
    SetPreviewHidden, TogglePreview,
};
use project::{Project, ProjectPath, search::SearchQuery};
use search::{SearchOption, SearchOptions};
use settings::Settings as _;
use theme::ActiveTheme as _;
use theme_settings::ThemeSettings;
use ui::{
    CommonAnimationExt, Divider, Label, ListItem, ListItemSpacing, Tooltip, h_flex, prelude::*,
    v_flex,
};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

actions!(
    idea_search,
    [
        /// Opens the project-wide search modal.
        Toggle
    ]
);

/// Bumped on every change, and shown at the trailing edge of the query input so a running build
/// can be identified while iterating. Remove before this is considered finished.
const VERSION: &str = "0.2.p12";

/// Wider than a plain picker: rows carry a line of source plus its location, and the preview
/// pane will share this width. The picker is told the same value, or it opens at its own default
/// and leaves a gap inside the modal.
const MODAL_WIDTH: Rems = Rems(48.);

/// Waited out before a search starts, so typing does not queue a scan per keystroke.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// Results are drained in batches so the list fills in while the scan is still running.
const RESULT_BATCH_SIZE: usize = 256;

/// Upper bound on collected matches, so a query like "e" cannot exhaust memory.
const MAX_MATCHES: usize = 10_000;

/// Longest run of a matched line kept for a row. Rows are one line tall and truncate anyway, and
/// a minified file is a single enormous line.
const MAX_ROW_TEXT_LEN: usize = 300;

/// How much of a long line to keep before the match when windowing it.
const CONTEXT_BEFORE_MATCH: usize = 40;

/// Options offered as toggles, in the order they appear next to the query input.
const TOGGLEABLE_OPTIONS: [SearchOption; 4] = [
    SearchOption::CaseSensitive,
    SearchOption::WholeWord,
    SearchOption::Regex,
    SearchOption::IncludeIgnored,
];

pub fn init(cx: &mut App) {
    cx.observe_new(IdeaSearch::register).detach();
}

pub struct IdeaSearch {
    picker: Entity<Picker<IdeaSearchDelegate>>,
}

impl IdeaSearch {
    fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.register_action(move |workspace, _: &Toggle, window, cx| {
            let project = workspace.project().clone();
            let handle = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, move |window, cx| {
                IdeaSearch::new(handle, project, window, cx)
            });
        });
    }

    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project_for_preview = project.clone();
        let delegate = IdeaSearchDelegate {
            modal: cx.entity().downgrade(),
            workspace,
            project,
            matches: Vec::new(),
            unique_files: HashSet::default(),
            selected_index: 0,
            search_options: SearchOptions::NONE,
            is_searching: false,
            preview_visible: true,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        };
        let preview = picker_preview::editor_preview(project_for_preview, window, cx);
        let picker = cx.new(|cx| {
            Picker::uniform_list_with_preview(delegate, preview, window, cx)
                .initial_width(MODAL_WIDTH)
                .show_scrollbar(true)
        });

        // The picker restores whichever layout was last used, including to the right or hidden.
        // Only the below-the-list layout is offered here, so pin it; this also makes the
        // toggle's initial state known, since the picker does not expose its layout.
        //
        // On the next frame rather than immediately: dispatching to a focus handle looks it up
        // in the last rendered frame's dispatch tree, and nothing has been rendered yet here, so
        // an immediate dispatch is silently dropped.
        let picker_focus_handle = picker.focus_handle(cx);
        cx.on_next_frame(window, move |_, window, cx| {
            picker_focus_handle.dispatch_action(&SetPreviewBelow, window, cx);
        });

        Self { picker }
    }
}

impl IdeaSearch {
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

    /// The picker owns preview visibility but does not expose it, so mirror every action that
    /// changes it. These run in the capture phase and do not stop propagation: the picker still
    /// handles them.
    fn preview_toggled(&mut self, _: &TogglePreview, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_preview_visible(!self.preview_visible(cx), cx);
    }

    fn preview_shown(&mut self, _: &SetPreviewBelow, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_preview_visible(true, cx);
    }

    fn preview_hidden(&mut self, _: &SetPreviewHidden, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_preview_visible(false, cx);
    }

    fn preview_visible(&self, cx: &App) -> bool {
        self.picker.read(cx).delegate.preview_visible
    }

    fn set_preview_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.picker.update(cx, |picker, cx| {
            picker.delegate.preview_visible = visible;
            cx.notify();
        });
    }
}

impl Render for IdeaSearch {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("IdeaSearch")
            .w(MODAL_WIDTH)
            .capture_action(cx.listener(Self::cancel))
            .capture_action(cx.listener(Self::preview_toggled))
            .capture_action(cx.listener(Self::preview_shown))
            .capture_action(cx.listener(Self::preview_hidden))
            .child(self.picker.clone())
    }
}

impl Focusable for IdeaSearch {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for IdeaSearch {}
impl ModalView for IdeaSearch {}

/// One row: a single match, with enough context to render it and to open it later.
struct SearchMatch {
    path: ProjectPath,
    buffer: Entity<Buffer>,
    line_number: u32,
    /// The matched line, trimmed of indentation and windowed around the match, so a minified
    /// file does not put a megabyte in a row.
    line_text: String,
    /// Byte range of the match within `line_text`.
    highlight: Range<usize>,
    /// Where `line_text` came from in the buffer, so syntax highlighting can be gathered for
    /// visible rows only rather than for every match found.
    buffer_range: Range<usize>,
    /// The match itself, for the preview to highlight and scroll to.
    anchor_range: Range<text::Anchor>,
    match_range: Range<usize>,
}

pub struct IdeaSearchDelegate {
    /// The picker reports dismissal to its delegate; the modal only closes once that is passed
    /// on as a `DismissEvent`.
    modal: WeakEntity<IdeaSearch>,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    matches: Vec<SearchMatch>,
    /// Distinct files among `matches`, for the footer summary.
    unique_files: HashSet<ProjectPath>,
    selected_index: usize,
    search_options: SearchOptions,
    /// Whether a scan is still running, so the summary can distinguish "no results yet" from
    /// "no results".
    is_searching: bool,
    /// Mirrors the picker's preview visibility, which it does not expose. Kept in step by
    /// forcing the layout when the modal opens and by watching `TogglePreview`.
    preview_visible: bool,
    /// Set when a search is abandoned, so a scan already running stops instead of filling the
    /// list with results for a query the user has moved on from.
    cancel_flag: Arc<AtomicBool>,
}

impl IdeaSearchDelegate {
    /// Opens the selected match in a tab, puts the cursor on its line, and closes the modal.
    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(search_match) = self.matches.get(self.selected_index) else {
            return;
        };
        let path = search_match.path.clone();
        let row = search_match.line_number.saturating_sub(1);
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let modal = self.modal.clone();

        cx.spawn_in(window, async move |_, cx| {
            let item = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_path(path, None, true, window, cx)
                })?
                .await?;

            if let Some(editor) = item.downcast::<Editor>() {
                editor.update_in(cx, |editor, window, cx| {
                    editor.change_selections(Default::default(), window, cx, |selections| {
                        let position = text::Point::new(row, 0);
                        selections.select_ranges([position..position])
                    });
                })?;
            }

            modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn cancel_running_search(&mut self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
        self.cancel_flag = Arc::new(AtomicBool::new(false));
    }
}

/// Turns the anchors a search reports into rows, reading each line's text once so rendering does
/// not have to touch the buffer.
fn collect_matches(
    buffer: &Entity<Buffer>,
    ranges: &[Range<text::Anchor>],
    cx: &App,
) -> Vec<SearchMatch> {
    let read = buffer.read(cx);
    let Some(file) = read.file() else {
        return Vec::new();
    };
    let path = ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    };
    let snapshot = read.snapshot();

    ranges
        .iter()
        .map(|range| {
            let point_range = range.to_point(&snapshot);
            let row = point_range.start.row;
            let line_start = language::Point::new(row, 0);
            let line_end = language::Point::new(row, snapshot.line_len(row));
            let full_line = snapshot
                .text_for_range(line_start..line_end)
                .collect::<String>();

            // Columns are byte offsets into the untrimmed line. A match can span rows, in which
            // case it runs to the end of this one.
            let match_start = point_range.start.column as usize;
            let match_end = if point_range.end.row == row {
                point_range.end.column as usize
            } else {
                full_line.len()
            };

            let (window, highlight) = render_window(&full_line, match_start..match_end);
            let line_offset = snapshot.point_to_offset(line_start);
            let buffer_range = line_offset + window.start..line_offset + window.end;

            SearchMatch {
                path: path.clone(),
                buffer: buffer.clone(),
                line_number: row + 1,
                line_text: full_line[window].to_string(),
                highlight,
                buffer_range,
                anchor_range: range.clone(),
                match_range: snapshot.point_to_offset(point_range.start)
                    ..snapshot.point_to_offset(point_range.end),
            }
        })
        .collect()
}

/// Trims indentation and, for very long lines, keeps only a window around the match. Returns the
/// slice of `line` to draw, and the match's byte range within that slice.
fn render_window(line: &str, match_range: Range<usize>) -> (Range<usize>, Range<usize>) {
    let trimmed_start = line.len() - line.trim_start().len();
    let start = floor_boundary(line, match_range.start.max(trimmed_start));
    let end = floor_boundary(line, match_range.end.clamp(start, line.len()));

    // Keep some context before the match when the line is too long to show whole.
    let window_start = if end.saturating_sub(trimmed_start) > MAX_ROW_TEXT_LEN {
        floor_boundary(line, start.saturating_sub(CONTEXT_BEFORE_MATCH))
    } else {
        trimmed_start
    };
    let window_end = ceil_boundary(line, (window_start + MAX_ROW_TEXT_LEN).min(line.len()));
    let window_len = window_end - window_start;

    let highlight = start.saturating_sub(window_start).min(window_len)
        ..end.saturating_sub(window_start).min(window_len);
    (window_start..window_end, highlight)
}

/// Tree-sitter highlight spans for `range`, as byte ranges relative to the start of that range.
/// Ids rather than styles, so a theme change restyles existing rows.
fn collect_syntax(
    snapshot: &language::BufferSnapshot,
    range: Range<usize>,
) -> Vec<(Range<usize>, HighlightId)> {
    let mut highlights = Vec::new();
    let mut offset = 0;
    for chunk in snapshot.chunks(
        range,
        LanguageAwareStyling {
            tree_sitter: true,
            diagnostics: false,
        },
    ) {
        let chunk_end = offset + chunk.text.len();
        if let Some(id) = chunk.syntax_highlight_id {
            highlights.push((offset..chunk_end, id));
        }
        offset = chunk_end;
    }
    highlights
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `HighlightedLabel` requires of us: in-bounds, on char boundaries, start before end.
    fn assert_usable(text: &str, highlight: &Range<usize>) {
        assert!(highlight.start <= highlight.end, "{highlight:?} in {text:?}");
        assert!(highlight.end <= text.len(), "{highlight:?} in {text:?}");
        assert!(text.is_char_boundary(highlight.start), "{highlight:?}");
        assert!(text.is_char_boundary(highlight.end), "{highlight:?}");
    }

    /// The text a row would draw, plus the match range within it.
    fn windowed(line: &str, match_range: Range<usize>) -> (String, Range<usize>) {
        let (window, highlight) = render_window(line, match_range);
        assert!(line.is_char_boundary(window.start));
        assert!(line.is_char_boundary(window.end));
        (line[window].to_string(), highlight)
    }

    #[test]
    fn render_window_trims_indentation() {
        let (text, highlight) = windowed("    let value = 1;", 8..13);
        assert_eq!(text, "let value = 1;");
        assert_eq!(&text[highlight.clone()], "value");
        assert_usable(&text, &highlight);
    }

    #[test]
    fn render_window_keeps_multibyte_boundaries() {
        let line = "let emoji = \"🎉🎉🎉\";";
        let start = line.find('🎉').expect("emoji in line");
        let (text, highlight) = windowed(line, start..start + "🎉".len());
        assert_eq!(&text[highlight.clone()], "🎉");
        assert_usable(&text, &highlight);
    }

    #[test]
    fn render_window_shortens_a_long_line() {
        let mut line = "x".repeat(5000);
        line.push_str("needle");
        let start = line.len() - "needle".len();
        let (text, highlight) = windowed(&line, start..line.len());

        assert!(text.len() <= MAX_ROW_TEXT_LEN, "kept {} bytes", text.len());
        assert_usable(&text, &highlight);
        assert_eq!(&text[highlight.clone()], "needle");
    }

    #[test]
    fn render_window_survives_a_match_inside_the_indentation() {
        let (text, highlight) = windowed("\t\tvalue", 0..1);
        assert_usable(&text, &highlight);
    }
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

impl PickerDelegate for IdeaSearchDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "IdeaSearch"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search all files…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
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
        self.cancel_running_search();
        self.matches.clear();
        self.unique_files.clear();
        self.selected_index = 0;
        self.is_searching = false;
        cx.notify();

        if query.is_empty() {
            return Task::ready(());
        }
        self.is_searching = true;

        let whole_word = self.search_options.contains(SearchOptions::WHOLE_WORD);
        let case_sensitive = self.search_options.contains(SearchOptions::CASE_SENSITIVE);
        let include_ignored = self.search_options.contains(SearchOptions::INCLUDE_IGNORED);
        let search_query = if self.search_options.contains(SearchOptions::REGEX) {
            SearchQuery::regex(
                query,
                whole_word,
                case_sensitive,
                include_ignored,
                false,
                Default::default(),
                Default::default(),
                false,
                None,
            )
        } else {
            SearchQuery::text(
                query,
                whole_word,
                case_sensitive,
                include_ignored,
                Default::default(),
                Default::default(),
                false,
                None,
            )
        };
        // An incomplete regex is expected while typing, so a failed query is not an error worth
        // reporting; it just has no results yet.
        let Some(search_query) = search_query.ok() else {
            return Task::ready(());
        };

        let cancel_flag = Arc::clone(&self.cancel_flag);
        let project = self.project.clone();

        cx.spawn_in(window, async move |picker, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            if cancel_flag.load(Ordering::SeqCst) {
                return;
            }

            let results = project.update(cx, |project, cx| project.search(search_query, cx));

            let mut results = pin!(results.rx.clone().ready_chunks(RESULT_BATCH_SIZE));
            while let Some(batch) = results.next().await {
                if cancel_flag.load(Ordering::SeqCst) {
                    return;
                }

                let new_matches = cx.update(|_, cx| {
                    batch
                        .into_iter()
                        .filter_map(|result| match result {
                            project::search::SearchResult::Buffer { buffer, ranges } => {
                                Some(collect_matches(&buffer, &ranges, cx))
                            }
                            _ => None,
                        })
                        .flatten()
                        .collect::<Vec<_>>()
                });
                let Some(new_matches) = new_matches.log_err() else {
                    return;
                };

                let still_running = picker
                    .update(cx, |picker, cx| {
                        let delegate = &mut picker.delegate;
                        let remaining = MAX_MATCHES.saturating_sub(delegate.matches.len());
                        for search_match in new_matches.into_iter().take(remaining) {
                            delegate.unique_files.insert(search_match.path.clone());
                            delegate.matches.push(search_match);
                        }
                        cx.notify();
                        delegate.matches.len() < MAX_MATCHES
                    })
                    .unwrap_or(false);
                if !still_running {
                    break;
                }
            }

            // A cancelled search has already been replaced by a newer one, which owns the flag.
            if !cancel_flag.load(Ordering::SeqCst) {
                picker
                    .update(cx, |picker, cx| {
                        picker.delegate.is_searching = false;
                        cx.notify();
                    })
                    .ok();
            }
        })
    }

    /// Only reports whether the layout is horizontal, so it cannot distinguish hidden from
    /// below. Still worth tracking: a switch to the side layout means the preview is visible.
    fn preview_layout_changed(&mut self, layout_is_horizontal: bool) {
        if layout_is_horizontal {
            self.preview_visible = true;
        }
    }

    /// Picker pulls this whenever the selection changes and drives the preview pane with it.
    fn try_get_preview_data_for_match(&self, _cx: &App) -> Option<PreviewUpdate> {
        let search_match = self.matches.get(self.selected_index)?;
        Some(PreviewUpdate {
            source: PreviewSource::Buffer(search_match.buffer.clone()),
            match_location: Some(MatchLocation {
                anchor_range: search_match.anchor_range.clone(),
                range: search_match.match_range.clone(),
            }),
        })
    }

    fn searchbar_trailer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let active = self.search_options;
        let picker = cx.entity();

        let toggles = TOGGLEABLE_OPTIONS.into_iter().map(|option| {
            let options = option.as_options();
            let picker = picker.clone();

            IconButton::new(("idea-search-option", option as usize), option.icon())
                .icon_size(IconSize::Small)
                .toggle_state(active.contains(options))
                .tooltip(Tooltip::text(option.label()))
                .on_click(move |_, window, cx| {
                    picker.update(cx, |picker, cx| {
                        picker.delegate.search_options.toggle(options);
                        // Re-runs the query through `update_matches` with the new options.
                        picker.refresh(window, cx);
                    });
                })
        });

        // The preview toggle lives here rather than in the footer, where the picker puts it by
        // default: `render_footer` below replaces that whole strip. Only the below-the-list
        // layout is offered, so this is on/off rather than a layout choice.
        let preview_visible = self.preview_visible;
        let preview_toggle = IconButton::new("idea-search-preview-toggle", IconName::Eye)
            .icon_size(IconSize::Small)
            .toggle_state(preview_visible)
            .tooltip(Tooltip::text("Toggle Preview"))
            .on_click(move |_, window, cx| {
                let action: &dyn gpui::Action = if preview_visible {
                    &SetPreviewHidden
                } else {
                    &SetPreviewBelow
                };
                window.dispatch_action(action.boxed_clone(), cx);
            });

        Some(
            h_flex()
                .gap_px()
                .children(toggles)
                .child(Divider::vertical().mx_1())
                .child(preview_toggle)
                .into_any_element(),
        )
    }

    /// Always `Some`, even when empty: returning `None` lets the picker fall back to its default
    /// footer, which carries a second set of preview controls.
    fn render_footer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let path_style = self.project.read(cx).path_style(cx);
        let location = self.matches.get(self.selected_index).map(|search_match| {
            let file_name = search_match
                .path
                .path
                .file_name()
                .map(|name| name.to_string())
                .unwrap_or_default();
            // Worktree-relative, so "./" rather than a leading slash. A file at the root has no
            // parent, and shows as "./" alone.
            let directory = match search_match.path.path.parent() {
                Some(parent) if !parent.is_empty() => {
                    format!("./{}", parent.display(path_style))
                }
                _ => "./".to_string(),
            };

            h_flex()
                .min_w_0()
                .gap_1p5()
                .child(Label::new(file_name).size(LabelSize::Small))
                .child(
                    Label::new(directory)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
        });

        let summary = (!self.matches.is_empty()).then(|| {
            // A capped scan stopped early, so both totals are lower bounds.
            let capped = if self.matches.len() >= MAX_MATCHES {
                "+"
            } else {
                ""
            };
            Label::new(format!(
                "{}{capped} matches in {}{capped} files",
                self.matches.len(),
                self.unique_files.len(),
            ))
            .size(LabelSize::Small)
            .color(Color::Muted)
        });

        let spinner = self.is_searching.then(|| {
            Icon::new(IconName::LoadCircle)
                .color(Color::Accent)
                .size(IconSize::Small)
                .with_rotate_animation(2)
        });

        Some(
            h_flex()
                .w_full()
                .px_2()
                .py_1()
                .gap_2()
                .justify_between()
                .child(div().flex_1().min_w_0().truncate().children(location))
                .child(
                    h_flex()
                        .flex_none()
                        .gap_1p5()
                        .children(spinner)
                        .children(summary)
                        .child(
                            Label::new(VERSION)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .into_any_element(),
        )
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.open_selected(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        // Escape closes the modal while a scan may still be running; stop it rather than let it
        // finish into a list nobody is looking at.
        self.cancel_running_search();
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
        let search_match = self.matches.get(ix)?;
        let file_name = search_match
            .path
            .path
            .file_name()
            .map(|name| name.to_string())
            .unwrap_or_default();

        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.buffer_font.family.clone(),
            font_features: settings.buffer_font.features.clone(),
            font_fallbacks: settings.buffer_font.fallbacks.clone(),
            // No font_size here: a `TextRun` carries font, color and weight, but takes its size
            // from the element's text style. The row container sets it.
            font_weight: settings.buffer_font.weight,
            line_height: relative(1.),
            ..Default::default()
        };

        // Gathered per visible row rather than for every match found: a broad query can produce
        // thousands of matches, and highlighting all of them up front is work whose result is
        // mostly never shown.
        let snapshot = search_match.buffer.read(cx).snapshot();
        let syntax_theme = cx.theme().syntax();
        let syntax = collect_syntax(&snapshot, search_match.buffer_range.clone())
            .into_iter()
            .filter_map(|(range, id)| Some((range, syntax_theme.get(id).copied()?)));
        let match_highlight = (
            search_match.highlight.clone(),
            HighlightStyle {
                background_color: Some(cx.theme().colors().search_match_background),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );
        let highlights = gpui::combine_highlights(syntax, [match_highlight]);

        // The picker opens a match on a single click. Rows here take the click first so a single
        // click only previews the match, and a double click opens it.
        let picker = cx.entity();
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .on_click(move |event: &ClickEvent, window, cx| {
                    let opening = event.click_count() >= 2;
                    picker.update(cx, |picker, cx| {
                        picker.delegate.selected_index = ix;
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
                        .text_sm()
                        .child(div().flex_1().min_w_0().truncate().child(
                            StyledText::new(search_match.line_text.clone())
                                .with_default_highlights(&text_style, highlights),
                        ))
                        .child(
                            Label::new(format!("{file_name}:{}", search_match.line_number))
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .single_line(),
                        ),
                ),
        )
    }
}
