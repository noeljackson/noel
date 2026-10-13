//! Ephemeral owned-screen transcript state. Entries/session state stay in the app.
use super::{
    pending_entry_rows, render_entry_lines, EntryScrollback, PreviewState, ThemePalette,
    TranscriptPosition, TranscriptRow, TuiEntry, TuiEntryKind,
};
use ratatui::{
    layout::{Position, Rect},
    style::Modifier,
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Anchor {
    entry: usize,
    position: TranscriptPosition,
}

#[derive(Debug)]
struct EntryLayout {
    rows: Vec<TranscriptRow>,
    logical: Vec<String>,
}

// Normal endpoints use source bytes, so rewrapping cannot move the selection.
// Folded previews use their own displayed bytes and never expose hidden text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TextPoint {
    anchor: Anchor,
    preview_byte: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hit {
    start: TextPoint,
    end: TextPoint,
}

#[derive(Debug)]
struct Selection {
    origin: Hit,
    focus: Hit,
    dragging: bool,
    moved: bool,
    was_latest: bool,
}

impl Selection {
    fn range(&self) -> Option<(TextPoint, TextPoint)> {
        self.moved.then(|| {
            (
                self.origin.start.min(self.focus.start),
                self.origin.end.max(self.focus.end),
            )
        })
    }
}

struct TextUnit {
    text: String,
    source: String,
    start: TextPoint,
    end: TextPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LayoutKey {
    width: u16,
    palette: ThemePalette,
    hide_thinking: bool,
    expanded_from: Option<usize>,
    live_thinking: Option<usize>,
}

#[derive(Debug, Default)]
pub(super) struct TranscriptView {
    anchor: Option<Anchor>,
    layouts: Vec<Option<EntryLayout>>,
    rows: Vec<(usize, usize)>,
    key: Option<LayoutKey>,
    dirty: bool,
    pub(super) area: Rect,
    pub(super) hint_area: Rect,
    new_output: bool,
    // Preserve bottom alignment when selecting a short transcript; otherwise
    // pressing the mouse would move the text before the first drag event.
    reading_padding: u16,
    // Live thinking previews are a moving tail. Freeze their source while
    // reading so that rows cannot disappear under a logical reading anchor.
    thinking_snapshot: Option<(usize, TuiEntry)>,
    selection: Option<Selection>,
    copy_status: Option<String>,
    pending_copy: Option<String>,
}

impl TranscriptView {
    pub(super) fn is_reading(&self) -> bool {
        self.anchor.is_some()
    }

    pub(super) fn jump_to_latest(&mut self) {
        if let Some((index, _)) = self.thinking_snapshot.take() {
            if let Some(layout) = self.layouts.get_mut(index) {
                *layout = None;
            }
            self.dirty = true;
        }
        self.anchor = None;
        self.reading_padding = 0;
        self.new_output = false;
    }

    pub(super) fn replaced(&mut self, index: usize) {
        // Replacement is not an append: endpoints may now refer to unrelated text.
        if self.selection.as_ref().is_some_and(|selection| {
            let low = selection
                .origin
                .start
                .anchor
                .entry
                .min(selection.focus.start.anchor.entry);
            let high = selection
                .origin
                .end
                .anchor
                .entry
                .max(selection.focus.end.anchor.entry);
            (low..=high).contains(&index)
        }) {
            self.clear_selection();
        }
        self.invalidate(index);
    }

    pub(super) fn invalidate(&mut self, index: usize) {
        // A folded preview can replace its tail rather than append. Its displayed
        // byte offsets are not source offsets; never copy a different tail silently.
        if self.selection.as_ref().is_some_and(|selection| {
            [selection.origin.start, selection.focus.start]
                .iter()
                .any(|point| point.anchor.entry == index && point.preview_byte.is_some())
        }) {
            self.clear_selection();
        }
        if let Some(layout) = self.layouts.get_mut(index) {
            *layout = None;
        }
        self.dirty = true;
        if self.is_reading() {
            self.new_output = true;
        }
    }

    pub(super) fn insert(&mut self, index: usize) {
        if let Some(selection) = &mut self.selection {
            for point in [
                &mut selection.origin.start,
                &mut selection.origin.end,
                &mut selection.focus.start,
                &mut selection.focus.end,
            ] {
                if index <= point.anchor.entry {
                    point.anchor.entry += 1;
                }
            }
        }
        if let Some((snapshot_index, _)) = &mut self.thinking_snapshot {
            if index <= *snapshot_index {
                *snapshot_index += 1;
            }
        }
        if index < self.layouts.len() {
            self.layouts.insert(index, None);
        }
        if let Some(anchor) = &mut self.anchor {
            if index <= anchor.entry {
                anchor.entry += 1;
            }
        }
        self.invalidate(index);
    }

    pub(super) fn remove(&mut self, index: usize) {
        if let Some(selection) = &mut self.selection {
            let low = selection
                .origin
                .start
                .anchor
                .entry
                .min(selection.focus.start.anchor.entry);
            let high = selection
                .origin
                .end
                .anchor
                .entry
                .max(selection.focus.end.anchor.entry);
            if (low..=high).contains(&index) {
                self.clear_selection();
            } else if index < low {
                for point in [
                    &mut selection.origin.start,
                    &mut selection.origin.end,
                    &mut selection.focus.start,
                    &mut selection.focus.end,
                ] {
                    point.anchor.entry -= 1;
                }
            }
        }
        if let Some((snapshot_index, _)) = &mut self.thinking_snapshot {
            if index < *snapshot_index {
                *snapshot_index -= 1;
            } else if index == *snapshot_index {
                self.thinking_snapshot = None;
            }
        }
        if index < self.layouts.len() {
            self.layouts.remove(index);
        }
        if let Some(anchor) = &mut self.anchor {
            if index < anchor.entry {
                anchor.entry -= 1;
            } else if index == anchor.entry {
                anchor.position = TranscriptPosition::default();
            }
        }
        self.dirty = true;
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn sync(
        &mut self,
        entries: &[TuiEntry],
        area: Rect,
        palette: &ThemePalette,
        hide_thinking: bool,
        previews: &PreviewState,
    ) {
        self.hint_area = Rect::new(
            area.x,
            area.bottom().saturating_sub(1),
            area.width,
            u16::from(area.height > 0),
        );
        self.area = Rect {
            height: area.height.saturating_sub(1),
            ..area
        };
        self.reading_padding = self.reading_padding.min(self.area.height.saturating_sub(1));
        if !self.is_reading()
            && (self.dirty
                || self
                    .key
                    .is_none_or(|key| key.live_thinking != previews.live_thinking))
        {
            self.thinking_snapshot = previews
                .live_thinking
                .and_then(|index| entries.get(index).map(|entry| (index, entry.clone())));
        }
        let effective_previews = PreviewState {
            expanded_from: previews.expanded_from,
            live_thinking: self
                .thinking_snapshot
                .as_ref()
                .map(|(index, _)| *index)
                .or(previews.live_thinking),
        };
        let key = LayoutKey {
            width: area.width,
            palette: *palette,
            hide_thinking,
            expanded_from: previews.expanded_from,
            live_thinking: effective_previews.live_thinking,
        };
        if self.key.is_some_and(|old| {
            old.hide_thinking != key.hide_thinking
                || old.expanded_from != key.expanded_from
                || (old.width != key.width
                    && self.selection.as_ref().is_some_and(|selection| {
                        selection.origin.start.preview_byte.is_some()
                            || selection.focus.start.preview_byte.is_some()
                    }))
        }) {
            self.clear_selection();
        }
        if self.key != Some(key) {
            self.layouts.clear();
            self.key = Some(key);
            self.dirty = true;
        }
        if self.layouts.len() != entries.len() {
            self.layouts.resize_with(entries.len(), || None);
            self.dirty = true;
        }
        if !self.dirty {
            return;
        }
        self.rows.clear();
        for (index, entry) in entries.iter().enumerate() {
            let entry = self
                .thinking_snapshot
                .as_ref()
                .filter(|(snapshot_index, _)| *snapshot_index == index)
                .map(|(_, entry)| entry)
                .unwrap_or(entry);
            let layout = self.layouts[index].get_or_insert_with(|| EntryLayout {
                logical: render_entry_lines(std::slice::from_ref(entry), palette)
                    .iter()
                    .map(|line| {
                        line.spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect()
                    })
                    .collect(),
                rows: if hide_thinking && entry.kind == TuiEntryKind::Thinking {
                    Vec::new()
                } else {
                    pending_entry_rows(
                        entry,
                        index,
                        area.width.max(1) as usize,
                        palette,
                        &effective_previews,
                        EntryScrollback::default(),
                    )
                },
            });
            self.rows
                .extend((0..layout.rows.len()).map(|row| (index, row)));
        }
        while self
            .rows
            .last()
            .is_some_and(|&(entry, row)| self.layout(entry).rows[row].line.width() == 0)
        {
            self.rows.pop();
        }
        if entries.is_empty() {
            self.jump_to_latest();
        } else if let Some(anchor) = &mut self.anchor {
            anchor.entry = anchor.entry.min(entries.len() - 1);
        }
        self.dirty = false;
    }

    fn layout(&self, entry: usize) -> &EntryLayout {
        self.layouts[entry]
            .as_ref()
            .expect("synced transcript layout")
    }

    fn bottom(&self) -> usize {
        self.rows.len().saturating_sub(self.area.height as usize)
    }

    fn start(&self) -> usize {
        let Some(anchor) = self.anchor else {
            return self.bottom();
        };
        // Keep the original source position across reflow; do not replace it
        // with the start of a wider row or repeated resizing drifts backwards.
        let before = self.rows.partition_point(|&(entry, row)| {
            entry < anchor.entry
                || (entry == anchor.entry && self.layout(entry).rows[row].start <= anchor.position)
        });
        before
            .saturating_sub(1)
            .min(self.rows.len().saturating_sub(1))
    }

    pub(super) fn scroll(&mut self, delta: isize) {
        if self.rows.is_empty()
            || self.area.height == 0
            || delta == 0
            || (self.anchor.is_none() && self.bottom() == 0)
        {
            return;
        }
        let start = self.start().saturating_add_signed(delta);
        self.reading_padding = 0;
        if delta > 0
            && start >= self.bottom()
            && !self
                .selection
                .as_ref()
                .is_some_and(|selection| selection.dragging)
        {
            self.jump_to_latest();
            return;
        }
        let (entry, row) = self.rows[start.min(self.bottom())];
        self.anchor = Some(Anchor {
            entry,
            position: self.layout(entry).rows[row].start,
        });
    }

    pub(super) fn page(&mut self, down: bool) {
        let rows = self.area.height.saturating_sub(1).max(1) as isize;
        self.scroll(if down { rows } else { -rows });
    }

    pub(super) fn visible_top(&self) -> u16 {
        if self.is_reading() {
            self.area.y + self.reading_padding
        } else {
            self.area
                .bottom()
                .saturating_sub(self.rows.len().min(self.area.height as usize) as u16)
        }
    }

    pub(super) fn visible_lines(&self) -> Vec<Line<'static>> {
        if self.area.height == 0 || self.area.width == 0 {
            return Vec::new();
        }
        self.rows
            .iter()
            .skip(self.start())
            .take(self.area.height.saturating_sub(self.reading_padding) as usize)
            .map(|&(entry, row)| self.selected_line(entry, row))
            .collect()
    }

    pub(super) fn has_selection(&self) -> bool {
        self.selection.as_ref().and_then(Selection::range).is_some()
    }

    pub(super) fn clear_selection(&mut self) -> bool {
        self.copy_status = None;
        self.pending_copy = None;
        self.selection.take().is_some()
    }

    pub(super) fn request_copy(&mut self) {
        self.pending_copy = self.selected_text().filter(|text| !text.is_empty());
    }

    pub(super) fn take_pending_copy(&mut self) -> Option<String> {
        self.pending_copy.take()
    }

    pub(super) fn set_copy_status(&mut self, status: String) {
        self.copy_status = Some(status);
    }

    fn point(&self, entry: usize, row: usize, byte: usize) -> TextPoint {
        let row = &self.layout(entry).rows[row];
        TextPoint {
            anchor: Anchor {
                entry,
                position: TranscriptPosition {
                    line: row.start.line,
                    byte: if row.synthetic {
                        row.start.byte
                    } else {
                        row.start.byte + byte
                    },
                },
            },
            preview_byte: row.synthetic.then_some(byte),
        }
    }

    fn units(&self, entry: usize, row_index: usize) -> Vec<TextUnit> {
        let layout = self.layout(entry);
        let row = &layout.rows[row_index];
        let text = if row.synthetic {
            row.line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        } else {
            let text = layout
                .logical
                .get(row.start.line)
                .map(String::as_str)
                .unwrap_or("");
            let end = if row.end.line == row.start.line {
                row.end.byte
            } else {
                text.len()
            };
            text.get(row.start.byte..end).unwrap_or("").to_string()
        };
        text.grapheme_indices(true)
            .map(|(byte, grapheme)| TextUnit {
                source: grapheme
                    .chars()
                    .filter(|ch| *ch == '\t' || !ch.is_control())
                    .collect(),
                text: if grapheme == "\t" {
                    "    ".into()
                } else {
                    grapheme.chars().filter(|ch| !ch.is_control()).collect()
                },
                start: self.point(entry, row_index, byte),
                end: self.point(entry, row_index, byte + grapheme.len()),
            })
            .collect()
    }

    fn hit(&self, position: Position) -> Option<Hit> {
        if !self.area.contains(position) {
            return None;
        }
        let count = self
            .rows
            .len()
            .saturating_sub(self.start())
            .min(self.area.height.saturating_sub(self.reading_padding) as usize);
        let top = self.visible_top();
        let offset = position.y.checked_sub(top)? as usize;
        if offset >= count {
            return None;
        }
        let &(entry, row) = self.rows.get(self.start() + offset)?;
        let column = position.x.saturating_sub(self.area.x) as usize;
        let units = self.units(entry, row);
        let mut cells = 0;
        for unit in &units {
            cells += unit.text.width();
            if cells > column {
                return Some(Hit {
                    start: unit.start,
                    end: unit.end,
                });
            }
        }
        let end = units
            .last()
            .map(|unit| unit.end)
            .unwrap_or_else(|| self.point(entry, row, 0));
        Some(Hit { start: end, end })
    }

    pub(super) fn begin_selection(&mut self, position: Position) -> bool {
        self.clear_selection();
        let Some(hit) = self.hit(position) else {
            return false;
        };
        let was_latest = !self.is_reading();
        // Pin the painted window immediately, including while a turn streams.
        if was_latest && !self.rows.is_empty() {
            self.reading_padding = self
                .area
                .height
                .saturating_sub(self.rows.len().min(u16::MAX as usize) as u16);
            let (entry, row) = self.rows[self.start()];
            self.anchor = Some(Anchor {
                entry,
                position: self.layout(entry).rows[row].start,
            });
        }
        self.selection = Some(Selection {
            origin: hit,
            focus: hit,
            dragging: true,
            moved: false,
            was_latest,
        });
        true
    }

    pub(super) fn drag_selection(&mut self, position: Position) -> bool {
        if !self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.dragging)
            || self.area.height == 0
            || self.area.width == 0
        {
            return false;
        }
        // Dragging past a transcript edge scrolls history without touching the composer.
        if position.y < self.area.y {
            self.scroll(-1);
        }
        if position.y >= self.area.bottom() {
            self.scroll(1);
        }
        let position = Position::new(
            position.x.clamp(self.area.x, self.area.right() - 1),
            position.y.clamp(self.area.y, self.area.bottom() - 1),
        );
        let Some(hit) = self.hit(position) else {
            return false;
        };
        let selection = self.selection.as_mut().expect("dragging selection");
        selection.focus = hit;
        selection.moved |= hit != selection.origin;
        self.copy_status = None;
        true
    }

    pub(super) fn finish_selection(&mut self) -> Option<String> {
        let selection = self.selection.as_mut()?;
        if !selection.dragging {
            return None;
        }
        selection.dragging = false;
        if !selection.moved {
            let was_latest = selection.was_latest;
            self.clear_selection();
            if was_latest {
                self.jump_to_latest();
            }
            return None;
        }
        let text = self.selected_text().filter(|text| !text.is_empty());
        self.pending_copy = text.clone();
        text
    }

    pub(super) fn selected_text(&self) -> Option<String> {
        let (low, high) = self.selection.as_ref()?.range()?;
        let mut text = String::new();
        let mut previous: Option<(usize, &TranscriptRow)> = None;
        for &(entry, row_index) in &self.rows {
            if entry < low.anchor.entry {
                continue;
            }
            if entry > high.anchor.entry {
                break;
            }
            let row = &self.layout(entry).rows[row_index];
            if (Anchor {
                entry,
                position: row.end,
            }) < low.anchor
                || (Anchor {
                    entry,
                    position: row.start,
                }) > high.anchor
            {
                continue;
            }
            let units = self.units(entry, row_index);
            let selected = units
                .iter()
                .filter(|unit| unit.start < high && unit.end > low)
                .map(|unit| unit.source.as_str())
                .collect::<String>();
            let point = self.point(entry, row_index, 0);
            if selected.is_empty() && !(units.is_empty() && low <= point && point < high) {
                continue;
            }
            if let Some((prev_entry, prev)) = previous {
                let wrapped = prev_entry == entry
                    && !prev.synthetic
                    && !row.synthetic
                    && prev.start.line == row.start.line;
                if !wrapped {
                    text.truncate(text.trim_end_matches([' ', '\t']).len());
                    text.push('\n');
                }
            }
            text.push_str(&selected);
            previous = Some((entry, row));
        }
        Some(text.trim_end().to_string())
    }

    fn selected_line(&self, entry: usize, row: usize) -> Line<'static> {
        let line = &self.layout(entry).rows[row].line;
        let Some((low, high)) = self.selection.as_ref().and_then(Selection::range) else {
            return line.clone();
        };
        if entry < low.anchor.entry || entry > high.anchor.entry {
            return line.clone();
        }
        let units = self.units(entry, row);
        let mut byte = 0;
        let ranges = units
            .iter()
            .filter_map(|unit| {
                let start = byte;
                byte += unit.text.len();
                (unit.start < high && unit.end > low).then_some(start..byte)
            })
            .collect::<Vec<_>>();
        let mut spans = Vec::new();
        let mut byte = 0;
        for span in &line.spans {
            for grapheme in span.content.graphemes(true) {
                let selected = ranges.iter().any(|range| range.contains(&byte));
                let style = if selected {
                    span.style.add_modifier(Modifier::REVERSED)
                } else {
                    span.style
                };
                spans.push(Span::styled(grapheme.to_string(), style));
                byte += grapheme.len();
            }
        }
        Line::from(spans).style(line.style)
    }

    pub(super) fn hint(&self) -> String {
        let mut parts = Vec::new();
        if self.is_reading() {
            parts.push(if self.new_output {
                "↓ Scroll to bottom · Esc · new output"
            } else {
                "↓ Scroll to bottom · Esc"
            });
        }
        if let Some(status) = &self.copy_status {
            parts.push(status);
        }
        if self.has_selection() {
            parts.push("Ctrl+C to copy · Esc clears");
        }
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{line_text, minimal_test_config};
    use crate::{
        handle_streaming_tui_key, handle_transcript_key, handle_transcript_mouse, redraw_tui,
        DiffDetail, DiffPanelState, FollowUpQueue, KeyCode, KeyEvent, KeyModifiers, MouseButton,
        MouseEvent, MouseEventKind, Position, SteeringMailbox, StreamingKeyOutcome, TuiApp,
    };
    use ratatui::{backend::TestBackend, Terminal};

    fn app_and_terminal() -> (TuiApp, Terminal<TestBackend>) {
        let mut app = TuiApp {
            owned_screen: true,
            ..Default::default()
        };
        app.push(
            TuiEntryKind::Assistant,
            (0..100)
                .map(|n| format!("row-{n:03}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        (app, Terminal::new(TestBackend::new(40, 20)).unwrap())
    }

    fn paint(app: &mut TuiApp, terminal: &mut Terminal<TestBackend>) {
        redraw_tui(terminal, app, &minimal_test_config()).unwrap();
        terminal.backend().assert_scrollback_empty();
    }

    fn visible(view: &TranscriptView) -> String {
        view.visible_lines()
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn selection_view(text: &str, width: u16) -> TranscriptView {
        let mut view = TranscriptView::default();
        view.sync(
            &[TuiEntry {
                kind: TuiEntryKind::Assistant,
                text: text.into(),
            }],
            Rect::new(0, 0, width, 20),
            &ThemePalette::default(),
            false,
            &PreviewState::default(),
        );
        view
    }

    #[test]
    fn selection_hit_testing_matches_bottom_alignment_and_highlights_only_text() {
        let mut view = selection_view("hello world", 40);
        let y = view.visible_top();
        assert!(view.hit(Position::new(2, 0)).is_none());
        assert!(view.begin_selection(Position::new(2, y)));
        assert_eq!(view.visible_top(), y); // Clicking must not move a short transcript.
        assert!(!view.has_selection());
        assert!(view.drag_selection(Position::new(6, y)));
        assert_eq!(view.finish_selection().as_deref(), Some("hello"));
        assert_eq!(view.take_pending_copy().as_deref(), Some("hello"));
        assert!(view.take_pending_copy().is_none());
        let line = &view.visible_lines()[0];
        let highlighted = line
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(highlighted, "hello");
        view.clear_selection();
        view.jump_to_latest();
        assert!(!view.has_selection());
        assert!(view.begin_selection(Position::new(2, y)));
        assert!(view.finish_selection().is_none());
        assert!(!view.is_reading());
    }

    #[test]
    fn selection_joins_soft_wraps_preserves_newlines_tabs_and_unicode_across_reflow() {
        let text = "abcdefghijklmnop\n界e\u{301}👩‍💻\tend";
        let mut view = selection_view(text, 10);
        let y = view.visible_top();
        // Forward select the first line and the Unicode/tab line; blank-cell endpoints are safe.
        view.begin_selection(Position::new(2, y));
        view.drag_selection(Position::new(9, y + 3));
        let selected = view.finish_selection().unwrap();
        assert_eq!(selected, "abcdefghijklmnop\n  界e\u{301}👩‍💻\tend");
        for width in [40, 7, 20, 10] {
            view.sync(
                &[TuiEntry {
                    kind: TuiEntryKind::Assistant,
                    text: text.into(),
                }],
                Rect::new(0, 0, width, 20),
                &ThemePalette::default(),
                false,
                &PreviewState::default(),
            );
            assert_eq!(view.selected_text().unwrap(), selected);
        }
        // Either cell of a wide glyph resolves to its complete grapheme.
        let mut view = selection_view("界e\u{301}👩‍💻xy", 40);
        let y = view.visible_top();
        view.begin_selection(Position::new(3, y));
        view.drag_selection(Position::new(6, y));
        assert_eq!(view.finish_selection().as_deref(), Some("界e\u{301}👩‍💻"));
        view.begin_selection(Position::new(6, y));
        view.drag_selection(Position::new(3, y));
        assert_eq!(view.finish_selection().as_deref(), Some("界e\u{301}👩‍💻"));
    }

    #[test]
    fn selection_crosses_entries_and_blank_lines_without_copying_padding() {
        let entries = vec![
            TuiEntry {
                kind: TuiEntryKind::Assistant,
                text: "one\n\nthree".into(),
            },
            TuiEntry {
                kind: TuiEntryKind::Assistant,
                text: "four".into(),
            },
        ];
        let mut view = TranscriptView::default();
        view.sync(
            &entries,
            Rect::new(0, 0, 40, 20),
            &ThemePalette::default(),
            false,
            &PreviewState::default(),
        );
        let y = view.visible_top();
        view.begin_selection(Position::new(2, y));
        view.drag_selection(Position::new(39, y + 4));
        assert_eq!(
            view.finish_selection().as_deref(),
            Some("one\n\n  three\n\n● four")
        );
    }

    #[test]
    fn collapsed_selection_copies_only_the_visible_preview_and_clears_on_expansion() {
        let entries = vec![TuiEntry {
            kind: TuiEntryKind::Tool,
            text:
                "completed read\nfile.rs\nfirst-secret\nsecond-secret\nthird-secret\nvisible-tail"
                    .into(),
        }];
        let mut view = TranscriptView::default();
        view.sync(
            &entries,
            Rect::new(0, 0, 40, 20),
            &ThemePalette::default(),
            false,
            &PreviewState::default(),
        );
        let y = view.visible_top();
        view.begin_selection(Position::new(0, y));
        view.drag_selection(Position::new(39, y + 1));
        let selected = view.finish_selection().unwrap();
        assert!(selected.contains("Used Read"));
        assert!(selected.contains("visible-tail"));
        assert!(!selected.contains("secret"));
        view.sync(
            &entries,
            Rect::new(0, 0, 40, 20),
            &ThemePalette::default(),
            false,
            &PreviewState {
                expanded_from: Some(0),
                live_thinking: None,
            },
        );
        assert!(!view.has_selection());
        assert!(view.take_pending_copy().is_none());
    }

    #[test]
    fn selecting_mutable_folded_preview_never_copies_a_replaced_tail() {
        let entries = vec![TuiEntry {
            kind: TuiEntryKind::Tool,
            text: "completed read\nfile.rs\none\ntwo\nthree\ntail".into(),
        }];
        let mut view = TranscriptView::default();
        let sync = |view: &mut TranscriptView, width| {
            view.sync(
                &entries,
                Rect::new(0, 0, width, 20),
                &ThemePalette::default(),
                false,
                &PreviewState::default(),
            )
        };
        sync(&mut view, 40);
        let y = view.visible_top();
        view.begin_selection(Position::new(4, y + 1));
        view.drag_selection(Position::new(7, y + 1));
        assert_eq!(view.finish_selection().as_deref(), Some("tail"));
        view.invalidate(0);
        assert!(!view.has_selection());
        assert!(view.take_pending_copy().is_none());
        sync(&mut view, 40);
        view.begin_selection(Position::new(4, y + 1));
        view.drag_selection(Position::new(7, y + 1));
        view.finish_selection();
        sync(&mut view, 20);
        assert!(!view.has_selection());
    }

    #[test]
    fn replacing_selected_text_clears_range_and_pending_clipboard() {
        let mut view = selection_view("original answer", 40);
        let y = view.visible_top();
        view.begin_selection(Position::new(2, y));
        view.drag_selection(Position::new(9, y));
        view.finish_selection();
        assert!(view.has_selection());
        view.replaced(0);
        assert!(!view.has_selection());
        assert!(view.take_pending_copy().is_none());
    }

    #[test]
    fn mouse_release_updates_endpoint_and_composer_click_clears_selection_only() {
        let (mut app, mut terminal) = app_and_terminal();
        app.set_input("retained draft");
        paint(&mut app, &mut terminal);
        let area = app.transcript_view.get_mut().area;
        let mouse = |kind, x, y| MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        handle_transcript_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 2, area.y),
            &mut app,
        );
        handle_transcript_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), 8, area.y),
            &mut app,
        );
        assert!(app.transcript_view.get_mut().has_selection());
        let anchor = app.transcript_view.get_mut().anchor;
        assert!(handle_transcript_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 4, 17),
            &mut app
        ));
        assert!(!app.transcript_view.get_mut().has_selection());
        assert_eq!(app.transcript_view.get_mut().anchor, anchor);
        assert_eq!(app.input, "retained draft");
    }

    #[test]
    fn dragging_scrolls_edges_and_selection_survives_streaming_insert_and_resize() {
        let (mut app, mut terminal) = app_and_terminal();
        paint(&mut app, &mut terminal);
        let area = app.transcript_view.get_mut().area;
        app.transcript_view
            .get_mut()
            .begin_selection(Position::new(2, area.y));
        let before = app.transcript_view.get_mut().anchor;
        app.append_entry(0, "\nnew output");
        paint(&mut app, &mut terminal);
        assert_eq!(app.transcript_view.get_mut().anchor, before);
        let view = app.transcript_view.get_mut();
        view.drag_selection(Position::new(8, area.bottom()));
        view.finish_selection();
        let selected = view.selected_text().unwrap();
        assert!(selected.contains("row-"));
        assert!(!selected.contains("new output"));
        app.insert_entry(0, TuiEntryKind::System, "unrelated");
        paint(&mut app, &mut terminal);
        assert_eq!(
            app.transcript_view.get_mut().selected_text().unwrap(),
            selected
        );
        app.remove_entry(0);
        paint(&mut app, &mut terminal);
        assert_eq!(
            app.transcript_view.get_mut().selected_text().unwrap(),
            selected
        );
        app.remove_entry(0);
        paint(&mut app, &mut terminal);
        assert!(!app.transcript_view.get_mut().has_selection());
    }

    #[test]
    fn ctrl_c_and_copy_command_copy_selection_without_interrupting_or_clearing_draft() {
        let (mut app, mut terminal) = app_and_terminal();
        app.turn_running = true;
        app.set_input("keep draft");
        paint(&mut app, &mut terminal);
        let area = app.transcript_view.get_mut().area;
        let mouse = |kind, x| MouseEvent {
            kind,
            column: x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(handle_transcript_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 2),
            &mut app
        ));
        assert!(handle_transcript_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), 8),
            &mut app
        ));
        assert!(handle_transcript_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), 8),
            &mut app
        ));
        let selected = app.transcript_view.get_mut().take_pending_copy().unwrap();
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let config = minimal_test_config();
        assert_eq!(
            handle_streaming_tui_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &mut app,
                &followups,
                &steering,
                &config
            ),
            StreamingKeyOutcome::Changed
        );
        assert_eq!(
            app.transcript_view.get_mut().take_pending_copy().unwrap(),
            selected
        );
        assert_eq!(app.input, "keep draft");
        assert!(app.turn_running);
        crate::handle_streaming_command("copy", &mut app, &followups, &config);
        assert_eq!(
            app.transcript_view.get_mut().take_pending_copy().unwrap(),
            selected
        );
        assert!(handle_transcript_key(
            &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app
        ));
        assert!(!app.transcript_view.get_mut().has_selection());
        assert_eq!(app.input, "keep draft");
        assert_eq!(
            handle_streaming_tui_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &mut app,
                &followups,
                &steering,
                &config
            ),
            StreamingKeyOutcome::Interrupt
        );
    }

    #[test]
    fn resumed_history_remains_navigable_without_native_scrollback() {
        let (mut app, mut terminal) = app_and_terminal();
        app.set_input("draft");
        paint(&mut app, &mut terminal);
        assert!(visible(app.transcript_view.get_mut()).contains("row-099"));
        let mut all = String::new();
        for _ in 0..110 {
            all.push_str(&visible(app.transcript_view.get_mut()));
            app.transcript_view.get_mut().scroll(-1);
        }
        for n in 0..100 {
            assert!(all.contains(&format!("row-{n:03}")));
        }
        assert!(visible(app.transcript_view.get_mut()).contains("row-000"));
        assert!(app.transcript_view.get_mut().is_reading());
        for _ in 0..110 {
            app.transcript_view.get_mut().scroll(1);
        }
        assert!(!app.transcript_view.get_mut().is_reading());
        assert_eq!(app.input, "draft");
    }

    #[test]
    fn anchored_reading_survives_output_reflow_height_and_entry_edits() {
        let (mut app, mut terminal) = app_and_terminal();
        paint(&mut app, &mut terminal);
        app.transcript_view.get_mut().scroll(-50);
        let anchor = app.transcript_view.get_mut().anchor;
        let first = app.transcript_view.get_mut().visible_lines()[0].clone();
        app.append_entry(0, "\nnew line\n".repeat(100).as_str());
        paint(&mut app, &mut terminal);
        assert_eq!(app.transcript_view.get_mut().visible_lines()[0], first);
        assert!(app.transcript_view.get_mut().hint().contains("new output"));
        for (w, h) in [(18, 12), (80, 30), (40, 20)] {
            terminal.backend_mut().resize(w, h);
            terminal.resize(Rect::new(0, 0, w, h)).unwrap();
            paint(&mut app, &mut terminal);
            assert_eq!(app.transcript_view.get_mut().anchor, anchor);
            assert_eq!(app.transcript_view.get_mut().visible_lines()[0], first);
        }
        app.insert_entry(0, TuiEntryKind::Tool, "completed read\nfile\ncontents");
        paint(&mut app, &mut terminal);
        assert_eq!(app.transcript_view.get_mut().anchor.unwrap().entry, 1);
        assert_eq!(app.transcript_view.get_mut().visible_lines()[0], first);
        app.remove_entry(0);
        paint(&mut app, &mut terminal);
        assert_eq!(app.transcript_view.get_mut().anchor, anchor);
        app.diff_panel = Some(DiffPanelState {
            files: vec![],
            selected: 0,
            detail: None,
        });
        app.previews_expanded = true;
        paint(&mut app, &mut terminal);
        assert_eq!(app.transcript_view.get_mut().anchor, anchor);
        app.remove_entry(0);
        paint(&mut app, &mut terminal);
        assert!(!app.transcript_view.get_mut().is_reading());
    }

    #[test]
    fn unicode_source_anchor_does_not_drift_on_repeated_resize() {
        let mut app = TuiApp {
            owned_screen: true,
            ..Default::default()
        };
        app.push(TuiEntryKind::Assistant, "界e\u{301}👩‍💻abcd\t".repeat(200));
        let mut terminal = Terminal::new(TestBackend::new(20, 20)).unwrap();
        paint(&mut app, &mut terminal);
        app.transcript_view.get_mut().scroll(-40);
        let anchor = app.transcript_view.get_mut().anchor;
        let first = line_text(&app.transcript_view.get_mut().visible_lines()[0]);
        for width in [33, 17, 60, 20, 33, 17, 60, 20] {
            terminal.backend_mut().resize(width, 20);
            terminal.resize(Rect::new(0, 0, width, 20)).unwrap();
            paint(&mut app, &mut terminal);
            assert_eq!(app.transcript_view.get_mut().anchor, anchor);
        }
        assert_eq!(
            line_text(&app.transcript_view.get_mut().visible_lines()[0]),
            first
        );
    }

    #[test]
    fn esc_returns_to_latest_without_touching_draft_or_interrupting_stream() {
        let (mut app, mut terminal) = app_and_terminal();
        app.turn_running = true;
        app.set_input("draft");
        app.paste_text("pasted\ntext");
        app.input_media.push(crate::MediaInput {
            mime_type: "image/png".into(),
            data_base64: "AAAA".into(),
            path: None,
            width: None,
            height: None,
        });
        paint(&mut app, &mut terminal);
        let draft = app.input.clone();
        let cursor = app.input_cursor;
        let pastes = app.input_pastes.len();
        let followups = FollowUpQueue::default();
        let steering = SteeringMailbox::default();
        let config = minimal_test_config();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.transcript_view.get_mut().page(false);
        assert!(matches!(
            handle_streaming_tui_key(esc, &mut app, &followups, &steering, &config),
            StreamingKeyOutcome::Changed
        ));
        assert!(!app.transcript_view.get_mut().is_reading());
        assert_eq!(app.input, draft);
        assert_eq!(app.input_cursor, cursor);
        assert_eq!(app.input_pastes.len(), pastes);
        assert_eq!(app.input_media.len(), 1);
        assert!(app.turn_running);
        assert!(matches!(
            handle_streaming_tui_key(esc, &mut app, &followups, &steering, &config),
            StreamingKeyOutcome::Interrupt
        ));
    }

    #[test]
    fn modal_esc_precedes_reading_and_mouse_does_not_capture_editor() {
        let (mut app, mut terminal) = app_and_terminal();
        paint(&mut app, &mut terminal);
        let position = app.transcript_view.get_mut().area.as_position();
        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: position.x,
            row: position.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(handle_transcript_mouse(mouse, &mut app));
        let anchor = app.transcript_view.get_mut().anchor;
        app.diff_panel = Some(DiffPanelState {
            files: vec![],
            selected: 0,
            detail: Some(DiffDetail {
                path: "file".into(),
                lines: vec![],
                scroll: 0,
            }),
        });
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            handle_streaming_tui_key(
                esc,
                &mut app,
                &FollowUpQueue::default(),
                &SteeringMailbox::default(),
                &minimal_test_config()
            ),
            StreamingKeyOutcome::Changed
        ));
        assert_eq!(app.transcript_view.get_mut().anchor, anchor);
        assert!(app.diff_panel.as_ref().unwrap().detail.is_none());
        let hint = app.transcript_view.get_mut().hint_area.as_position();
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hint.x,
            row: hint.y,
            ..mouse
        };
        assert!(handle_transcript_mouse(click, &mut app));
        assert!(!app.transcript_view.get_mut().is_reading());
        let editor = Position::new(0, 19);
        assert!(!handle_transcript_mouse(
            MouseEvent {
                column: editor.x,
                row: editor.y,
                ..mouse
            },
            &mut app
        ));
        app.owned_screen = false;
        assert!(!handle_transcript_mouse(mouse, &mut app));
        assert!(!handle_transcript_key(
            &KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut app
        ));
    }

    #[test]
    fn moving_thinking_preview_is_frozen_until_the_reader_returns_to_latest() {
        let (mut app, mut terminal) = app_and_terminal();
        let thought = app.push_placeholder(
            TuiEntryKind::Thinking,
            "first thought\nsecond thought\nthird thought",
        );
        app.live_thinking_index = Some(thought);
        paint(&mut app, &mut terminal);
        app.transcript_view.get_mut().scroll(-1);
        let window = visible(app.transcript_view.get_mut());
        app.append_entry(thought, "\nfourth thought\nfifth thought");
        paint(&mut app, &mut terminal);
        assert_eq!(visible(app.transcript_view.get_mut()), window);
        app.finish_live_entry();
        paint(&mut app, &mut terminal);
        assert_eq!(visible(app.transcript_view.get_mut()), window);
        app.transcript_view.get_mut().jump_to_latest();
        app.previews_expanded = true;
        paint(&mut app, &mut terminal);
        assert!(visible(app.transcript_view.get_mut()).contains("fifth thought"));
    }

    #[test]
    fn short_transcript_does_not_enter_reading_and_tiny_owned_frames_are_safe() {
        let mut app = TuiApp {
            owned_screen: true,
            ..Default::default()
        };
        app.push(TuiEntryKind::Assistant, "short answer");
        app.set_input("draft\nsecond line");
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        paint(&mut app, &mut terminal);
        app.transcript_view.get_mut().page(false);
        assert!(!app.transcript_view.get_mut().is_reading());
        for width in [1, 2, 4, 8, 20] {
            for height in [1, 2, 3, 5, 10] {
                terminal.backend_mut().resize(width, height);
                terminal.resize(Rect::new(0, 0, width, height)).unwrap();
                paint(&mut app, &mut terminal);
                app.transcript_view.get_mut().page(false);
                paint(&mut app, &mut terminal);
                app.transcript_view.get_mut().jump_to_latest();
            }
        }
        assert_eq!(app.input, "draft\nsecond line");
    }

    #[test]
    fn idle_layouts_are_cached_and_clear_removes_all_view_state() {
        let (mut app, mut terminal) = app_and_terminal();
        paint(&mut app, &mut terminal);
        let rows = app.transcript_view.get_mut().layout(0).rows.as_ptr();
        app.transcript_view.get_mut().page(false);
        paint(&mut app, &mut terminal);
        assert_eq!(app.transcript_view.get_mut().layout(0).rows.as_ptr(), rows);
        app.clear_visible();
        paint(&mut app, &mut terminal);
        assert!(app.transcript_view.get_mut().rows.is_empty());
        assert!(!app.transcript_view.get_mut().is_reading());
    }
}
