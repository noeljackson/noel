//! The in-process TUI: its app state, transcript layout and rendering, the input box, selectors and overlays, and their keys.

use super::*;

/// The TUI's hold on the terminal. Dropping it restores the terminal unless a
/// fatal signal already did (see `signals`).
pub(crate) struct TerminalRestore;

impl TerminalRestore {
    pub(crate) fn claim(owned_screen: bool) -> Self {
        signals::claim_tui_terminal(owned_screen);
        Self
    }
}

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        signals::release_tui_terminal();
    }
}

/// Leaves every terminal mode the TUI enabled. The cursor is shown too: on a
/// signal exit ratatui's own drop, which normally does it, never runs.
pub(crate) fn restore_terminal_modes(out: &mut impl Write, owned_screen: bool) {
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = execute!(out, DisableBracketedPaste);
    if owned_screen {
        let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen);
    }
    let _ = execute!(out, crossterm::cursor::Show);
    let _ = disable_raw_mode();
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TuiEntryKind {
    Thinking,
    System,
    User,
    Assistant,
    Tool,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TuiEntry {
    pub(crate) kind: TuiEntryKind,
    pub(crate) text: String,
}

/// A listed model's thinking levels, lowest first, and the level selecting
/// it gives the session when the user adjusts nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelThinking {
    pub(crate) levels: Vec<&'static str>,
    pub(crate) switch_level: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TuiSelectorState {
    pub(crate) kind: String,
    pub(crate) title: String,
    pub(crate) items: Vec<SelectorItem>,
    pub(crate) filtered_indices: Vec<usize>,
    pub(crate) selected: usize,
    pub(crate) query: String,
    /// The thinking level the user adjusted to with left/right; `None`
    /// until then, when selecting a model gives the session the level a
    /// model switch does.
    pub(crate) thinking_level: Option<String>,
    /// What each listed model supports of thinking, by row value.
    pub(crate) model_thinking: BTreeMap<String, ModelThinking>,
    /// Codex-style two-layer model picker: selecting a provider pushes this
    /// selector here and shows the provider's models; leaving the model layer
    /// (esc) pops back instead of closing the whole selector.
    pub(crate) parent: Option<Box<TuiSelectorState>>,
    /// Provider filter for the model layer (e.g. "zai-coding").
    pub(crate) provider_filter: Option<String>,
    /// A tree selector's rows by value (/tree's branches, #108): each row's
    /// parent, whether it is named, and its label before fold markers.
    /// Empty for a flat list.
    pub(crate) tree: BTreeMap<String, TreeRow>,
    /// Rows whose descendants are hidden.
    pub(crate) folded: BTreeSet<String>,
    pub(crate) tree_filter: TreeFilter,
}

/// One row of a tree selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeRow {
    pub(crate) parent: Option<String>,
    pub(crate) named: bool,
    pub(crate) label: String,
}

/// Which rows a tree selector shows (upstream's `treeFilterMode`; a branch
/// here is a session, so "labeled" means a named session).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum TreeFilter {
    #[default]
    All,
    Named,
}

impl TreeFilter {
    /// The `treeFilterMode` setting's value.
    pub(crate) fn setting(self) -> &'static str {
        match self {
            TreeFilter::All => "default",
            TreeFilter::Named => "labeled-only",
        }
    }

    pub(crate) fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("labeled-only") => TreeFilter::Named,
            _ => TreeFilter::All,
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            TreeFilter::All => TreeFilter::Named,
            TreeFilter::Named => TreeFilter::All,
        }
    }
}

impl TuiSelectorState {
    /// A selector a worker sent ([`TuiSelectorState::to_json`]).
    pub(crate) fn from_json(value: &serde_json::Value) -> Option<Self> {
        let items = value["items"].as_array()?.iter().map(|item| SelectorItem {
            label: item["label"].as_str().unwrap_or_default().to_owned(),
            value: item["value"].as_str().unwrap_or_default().to_owned(),
            active: item["active"] == true,
        });
        let mut state = Self::new(
            value["kind"].as_str()?,
            Selector::new(value["title"].as_str().unwrap_or_default(), items.collect()),
            value["query"].as_str().unwrap_or_default(),
            None,
        );
        if let Some(rows) = value["tree"].as_object() {
            let rows = rows.iter().map(|(row, data)| {
                (
                    row.clone(),
                    (
                        data["parent"].as_str().map(str::to_owned),
                        data["named"] == true,
                    ),
                )
            });
            state.set_tree(rows.collect());
        }
        Some(state)
    }

    /// The selector as a worker sends it to a client: its kind, title,
    /// query and rows; a pick comes back as `/select <kind> <value>`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let items = self.items.iter().map(|item| {
            serde_json::json!({"label": item.label, "value": item.value, "active": item.active})
        });
        let mut value = serde_json::json!({
            "kind": self.kind,
            "title": self.title,
            "query": self.query,
            "items": items.collect::<Vec<_>>(),
        });
        if !self.tree.is_empty() {
            let rows = self.tree.iter().map(|(row, data)| {
                (
                    row.clone(),
                    serde_json::json!({"parent": data.parent, "named": data.named}),
                )
            });
            value["tree"] = serde_json::Value::Object(rows.collect());
        }
        value
    }

    /// Makes the selector a tree: each row's parent and whether it is named,
    /// by value. Labels as they are now are the rows' own.
    pub(crate) fn set_tree(&mut self, rows: BTreeMap<String, (Option<String>, bool)>) {
        self.tree = self
            .items
            .iter()
            .filter_map(|item| {
                let (parent, named) = rows.get(&item.value)?.clone();
                Some((
                    item.value.clone(),
                    TreeRow {
                        parent,
                        named,
                        label: item.label.clone(),
                    },
                ))
            })
            .collect();
        self.refresh_filter();
    }

    fn has_children(&self, value: &str) -> bool {
        self.tree
            .values()
            .any(|row| row.parent.as_deref() == Some(value))
    }

    /// Whether a tree row shows: the filter admits it and no ancestor is
    /// folded.
    fn tree_shows(&self, value: &str) -> bool {
        let Some(row) = self.tree.get(value) else {
            return true;
        };
        if self.tree_filter == TreeFilter::Named && !row.named {
            return false;
        }
        let mut parent = row.parent.as_deref();
        while let Some(ancestor) = parent {
            if self.folded.contains(ancestor) {
                return false;
            }
            parent = self
                .tree
                .get(ancestor)
                .and_then(|row| row.parent.as_deref());
        }
        true
    }

    /// Selects the row `value` if it shows.
    fn select_value(&mut self, value: &str) {
        let shown = self
            .filtered_indices
            .iter()
            .position(|index| self.items[*index].value == value);
        if let Some(position) = shown {
            self.selected = position;
        }
    }

    /// Folds the selected row's branch, or moves to its parent (upstream's
    /// `foldOrUp`).
    pub(crate) fn fold_or_up(&mut self) {
        let Some(value) = self.selected_item().map(|item| item.value.clone()) else {
            return;
        };
        if self.has_children(&value) && !self.folded.contains(&value) {
            self.folded.insert(value.clone());
            self.refresh_filter();
            self.select_value(&value);
        } else if let Some(parent) = self.tree.get(&value).and_then(|row| row.parent.clone()) {
            self.select_value(&parent);
        }
    }

    /// Unfolds the selected row's branch, or moves to its first child
    /// (upstream's `unfoldOrDown`).
    pub(crate) fn unfold_or_down(&mut self) {
        let Some(value) = self.selected_item().map(|item| item.value.clone()) else {
            return;
        };
        if self.folded.remove(&value) {
            self.refresh_filter();
            self.select_value(&value);
            return;
        }
        let child = self
            .items
            .iter()
            .find(|item| {
                self.tree
                    .get(&item.value)
                    .and_then(|row| row.parent.as_deref())
                    == Some(value.as_str())
            })
            .map(|item| item.value.clone());
        if let Some(child) = child {
            self.select_value(&child);
        }
    }

    /// Shows the rows `filter` admits, keeping the selection where it can.
    pub(crate) fn set_tree_filter(&mut self, filter: TreeFilter) {
        let selected = self.selected_item().map(|item| item.value.clone());
        // The title says when a filter hides rows.
        const NAMED: &str = " · named only";
        let base = self
            .title
            .strip_suffix(NAMED)
            .unwrap_or(&self.title)
            .to_owned();
        self.title = match filter {
            TreeFilter::All => base,
            TreeFilter::Named => format!("{base}{NAMED}"),
        };
        self.tree_filter = filter;
        self.refresh_filter();
        if let Some(selected) = selected {
            self.select_value(&selected);
        }
    }
}

impl TuiSelectorState {
    pub(crate) fn new(
        kind: impl Into<String>,
        selector: Selector,
        query: impl Into<String>,
        thinking_level: Option<String>,
    ) -> Self {
        let mut state = Self {
            kind: kind.into(),
            title: selector.title,
            items: selector.items,
            filtered_indices: Vec::new(),
            selected: 0,
            query: query.into(),
            thinking_level,
            model_thinking: BTreeMap::new(),
            parent: None,
            provider_filter: None,
            tree: BTreeMap::new(),
            folded: BTreeSet::new(),
            tree_filter: TreeFilter::All,
        };
        state.refresh_filter();
        state
    }

    /// Records what the listed models support of thinking, so the picker
    /// can show and adjust the level a selection applies.
    pub(crate) fn with_model_thinking(mut self, config: &LoadedConfig, runtime: &Runtime) -> Self {
        let before = effective_thinking_level(runtime, config);
        let by_value = config
            .models
            .iter()
            .map(|model| (format!("{}/{}", model.provider, model.id), model))
            .collect::<BTreeMap<_, _>>();
        self.model_thinking = self
            .items
            .iter()
            .filter_map(|item| {
                let model = by_value.get(&item.value)?;
                // The level a switch to this model gives the session
                // (`switch_model`).
                let requested = [
                    config
                        .settings
                        .model_thinking_level(&model.provider, &model.id),
                    config.settings.default_thinking_level.as_deref(),
                ]
                .into_iter()
                .flatten()
                .find_map(normalized_thinking_level)
                .unwrap_or(before);
                Some((
                    item.value.clone(),
                    ModelThinking {
                        levels: model.supported_thinking_levels(),
                        switch_level: model.clamp_thinking_level(requested),
                    },
                ))
            })
            .collect();
        self
    }

    /// The selected model's thinking, if it has levels to choose from.
    pub(crate) fn selected_model_thinking(&self) -> Option<&ModelThinking> {
        self.model_thinking
            .get(&self.selected_item()?.value)
            .filter(|thinking| thinking.levels.len() > 1)
    }

    pub(crate) fn refresh_filter(&mut self) {
        // A folded tree row says so after its label.
        for index in 0..self.items.len() {
            let value = self.items[index].value.clone();
            if let Some(row) = self.tree.get(&value) {
                let marker = match (self.has_children(&value), self.folded.contains(&value)) {
                    (true, true) => " [+]",
                    _ => "",
                };
                self.items[index].label = format!("{}{marker}", row.label);
            }
        }
        let query = self.query.to_lowercase();
        let filtered = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let matches = query.is_empty()
                    || item.label.to_lowercase().contains(&query)
                    || item.value.to_lowercase().contains(&query);
                (matches && self.tree_shows(&item.value)).then_some(index)
            })
            .collect();
        self.filtered_indices = filtered;
        if self.selected >= self.filtered_indices.len() {
            self.selected = self.filtered_indices.len().saturating_sub(1);
        }
    }

    pub(crate) fn selected_item(&self) -> Option<&SelectorItem> {
        self.filtered_indices
            .get(self.selected)
            .and_then(|index| self.items.get(*index))
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let len = self.filtered_indices.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta as usize).min(len - 1)
        };
    }

    pub(crate) fn jump_to_start(&mut self) {
        self.selected = 0;
    }

    pub(crate) fn jump_to_end(&mut self) {
        let len = self.filtered_indices.len();
        self.selected = len.saturating_sub(1);
    }

    pub(crate) fn push_query_char(&mut self, ch: char) {
        self.query.push(ch);
        self.selected = 0;
        self.refresh_filter();
    }

    pub(crate) fn pop_query_char(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.refresh_filter();
    }

    /// The level selecting the highlighted model applies: the one the user
    /// adjusted to, clamped to the model, else the one a model switch
    /// gives. `None` for a model without thinking levels.
    pub(crate) fn selected_thinking_level(&self) -> Option<String> {
        let thinking = self.selected_model_thinking()?;
        let level = match self
            .thinking_level
            .as_deref()
            .and_then(normalized_thinking_level)
        {
            Some(adjusted) => noel_ai::catalog::clamp_to_supported(&thinking.levels, adjusted),
            None => thinking.switch_level,
        };
        Some(level.to_string())
    }
}

pub(crate) fn model_ref_from_value(value: &str) -> Option<ModelRef> {
    let (provider, id) = value.split_once('/')?;
    Some(ModelRef {
        provider: provider.to_string(),
        id: id.to_string(),
    })
}

#[derive(Debug, Default)]
pub(crate) struct TuiApp {
    pub(crate) entries: Vec<TuiEntry>,
    pub(crate) transcript_scrollback: TranscriptScrollback,
    pub(crate) owned_screen: bool,
    pub(crate) transcript_view: RefCell<TranscriptView>,
    pub(crate) input: String,
    pub(crate) input_media: Vec<MediaInput>,
    pub(crate) input_cursor: usize,
    pub(crate) quit_armed_since: Option<Instant>,
    pub(crate) editor_state: EditorState,
    pub(crate) last_shell_command: Option<String>,
    /// The overlay open over the input, at most one (#175).
    pub(crate) overlay: Overlay,
    /// The filter a tree selector opens on (`treeFilterMode`).
    pub(crate) tree_filter: TreeFilter,
    pub(crate) header_line: String,
    /// The session's workspace on another machine, which the footer shows
    /// with its connection state in place of the directory.
    pub(crate) workspace: Option<SessionWorkspace>,
    pub(crate) status: String,
    pub(crate) show_hardware_cursor: bool,
    pub(crate) live_entry_index: Option<usize>,
    pub(crate) history_cursor: Option<usize>,
    pub(crate) history_draft: Option<InputDraft>,
    pub(crate) input_pastes: Vec<CollapsedPaste>,
    pub(crate) next_paste_id: usize,
    pub(crate) todos: Vec<TodoItem>,
    pub(crate) todos_expanded: bool,
    pub(crate) previews_expanded: bool,
    pub(crate) live_thinking_index: Option<usize>,
    pub(crate) live_tool_indices: Vec<usize>,
    /// The running `codemode` call's nested calls as rows, kept above its
    /// result once it ends.
    pub(crate) nested_rows: Option<String>,
    pub(crate) queued_messages: Vec<String>,
    pub(crate) turn_running: bool,
    /// True once the running turn has made a tool call; a prose-only turn
    /// has no interior round boundary, so ctrl+s steering only lands after
    /// the response finishes and the hint must not promise a boundary.
    pub(crate) turn_saw_tools: bool,
}

/// What covers the input: a multiline prompt being typed, a selector, or
/// the diff panel. Opening one closes another.
#[derive(Debug, Default)]
pub(crate) enum Overlay {
    #[default]
    None,
    Multiline(Vec<String>),
    Selector(TuiSelectorState),
    DiffPanel(DiffPanelState),
}

impl TuiApp {
    pub(crate) fn multiline(&self) -> Option<&Vec<String>> {
        match &self.overlay {
            Overlay::Multiline(lines) => Some(lines),
            _ => None,
        }
    }

    pub(crate) fn multiline_mut(&mut self) -> Option<&mut Vec<String>> {
        match &mut self.overlay {
            Overlay::Multiline(lines) => Some(lines),
            _ => None,
        }
    }

    pub(crate) fn selector(&self) -> Option<&TuiSelectorState> {
        match &self.overlay {
            Overlay::Selector(selector) => Some(selector),
            _ => None,
        }
    }

    pub(crate) fn selector_mut(&mut self) -> Option<&mut TuiSelectorState> {
        match &mut self.overlay {
            Overlay::Selector(selector) => Some(selector),
            _ => None,
        }
    }

    /// The selector, closed.
    pub(crate) fn take_selector(&mut self) -> Option<TuiSelectorState> {
        match std::mem::take(&mut self.overlay) {
            Overlay::Selector(selector) => Some(selector),
            other => {
                self.overlay = other;
                None
            }
        }
    }

    pub(crate) fn diff_panel(&self) -> Option<&DiffPanelState> {
        match &self.overlay {
            Overlay::DiffPanel(panel) => Some(panel),
            _ => None,
        }
    }

    pub(crate) fn diff_panel_mut(&mut self) -> Option<&mut DiffPanelState> {
        match &mut self.overlay {
            Overlay::DiffPanel(panel) => Some(panel),
            _ => None,
        }
    }

    /// Closes the overlay `is` matches; another stays open.
    pub(crate) fn close_overlay(&mut self, is: fn(&Overlay) -> bool) {
        if is(&self.overlay) {
            self.overlay = Overlay::None;
        }
    }
}

// Paste ranges refer to the full input, never to marker text. Submission,
// queues, history, and external editors therefore always receive real text.
#[derive(Debug, Clone)]
pub(crate) struct CollapsedPaste {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) id: usize,
    pub(crate) lines: usize,
}

impl CollapsedPaste {
    pub(crate) fn marker(&self) -> String {
        format!(
            "[paste #{} +{} {}]",
            self.id,
            self.lines,
            if self.lines == 1 { "line" } else { "lines" }
        )
    }
}

#[derive(Debug)]
pub(crate) struct InputDraft {
    pub(crate) text: String,
    pub(crate) cursor: usize,
    pub(crate) pastes: Vec<CollapsedPaste>,
}

pub(crate) const LARGE_PASTE_CHARS: usize = 1_000;

#[derive(Debug)]
pub(crate) struct DiffPanelState {
    pub(crate) files: Vec<DiffFileEntry>,
    pub(crate) selected: usize,
    pub(crate) detail: Option<DiffDetail>,
}

#[derive(Debug)]
pub(crate) struct DiffFileEntry {
    pub(crate) path: String,
    pub(crate) added: Option<u64>,
    pub(crate) removed: Option<u64>,
    pub(crate) untracked: bool,
}

#[derive(Debug)]
pub(crate) struct DiffDetail {
    pub(crate) path: String,
    pub(crate) lines: Vec<String>,
    pub(crate) scroll: usize,
}

impl TuiApp {
    pub(crate) fn push(&mut self, kind: TuiEntryKind, text: impl Into<String>) {
        let text = text.into();
        if !text.trim().is_empty() {
            // Expansion is for inspecting existing output, not an opt-in to
            // dump every tool result from subsequent prompts onto the screen.
            if kind == TuiEntryKind::User {
                self.previews_expanded = false;
            }
            self.transcript_view
                .get_mut()
                .invalidate(self.entries.len());
            self.entries.push(TuiEntry { kind, text });
        }
    }

    pub(crate) fn history_previous(&mut self) {
        let history = self.editor_state.history();
        if history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            Some(cursor) => cursor.saturating_sub(1),
            None => {
                self.history_draft = Some(InputDraft {
                    text: self.input.clone(),
                    cursor: self.input_cursor,
                    pastes: self.input_pastes.clone(),
                });
                history.len().saturating_sub(1)
            }
        };
        self.history_cursor = Some(next);
        self.input = history[next].clone();
        self.input_cursor = self.input.len();
        self.input_pastes.clear();
    }

    pub(crate) fn history_next(&mut self) {
        let Some(cursor) = self.history_cursor else {
            return;
        };
        let history = self.editor_state.history();
        if cursor + 1 < history.len() {
            let next = cursor + 1;
            self.history_cursor = Some(next);
            self.input = history[next].clone();
            self.input_pastes.clear();
        } else {
            self.history_cursor = None;
            if let Some(draft) = self.history_draft.take() {
                self.input = draft.text;
                self.input_cursor = draft.cursor;
                self.input_pastes = draft.pastes;
                return;
            }
            self.input.clear();
            self.input_pastes.clear();
        }
        self.input_cursor = self.input.len();
    }

    pub(crate) fn reset_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_draft = None;
    }

    pub(crate) fn paste_text(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let start = self.input_cursor;
        self.insert_input_str(&text);
        if text.contains('\n') || text.chars().count() > LARGE_PASTE_CHARS {
            self.next_paste_id += 1;
            self.input_pastes.push(CollapsedPaste {
                start,
                end: self.input_cursor,
                id: self.next_paste_id,
                lines: text.lines().count().max(1),
            });
            self.input_pastes.sort_by_key(|paste| paste.start);
        }
    }

    pub(crate) fn display_input(&self) -> (String, usize) {
        let mut display = String::new();
        let mut source_start = 0;
        let mut cursor = None;
        for paste in &self.input_pastes {
            if cursor.is_none() && self.input_cursor <= paste.start {
                cursor = Some(display.len() + self.input_cursor - source_start);
            }
            display.push_str(&self.input[source_start..paste.start]);
            display.push_str(&paste.marker());
            if cursor.is_none() && self.input_cursor < paste.end {
                cursor = Some(display.len());
            }
            source_start = paste.end;
        }
        let cursor = cursor.unwrap_or_else(|| display.len() + self.input_cursor - source_start);
        display.push_str(&self.input[source_start..]);
        (display, cursor)
    }

    pub(crate) fn insert_input_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let cursor = self.input_cursor;
        self.input.insert_str(cursor, text);
        let inserted = text.len();
        self.input_pastes.retain_mut(|paste| {
            if paste.start < cursor && cursor < paste.end {
                return false;
            }
            if paste.start >= cursor {
                paste.start += inserted;
                paste.end += inserted;
            }
            true
        });
        self.input_cursor += inserted;
        self.reset_history_navigation();
    }

    pub(crate) fn set_input(&mut self, value: impl Into<String>) {
        self.input = value.into();
        self.input_cursor = self.input.len();
        self.input_pastes.clear();
        self.reset_history_navigation();
    }

    pub(crate) fn push_input_char(&mut self, ch: char) {
        let mut buf = [0u8; 4];
        self.insert_input_str(ch.encode_utf8(&mut buf));
    }

    pub(crate) fn pop_input_char(&mut self) {
        let start = self
            .input_pastes
            .iter()
            .find(|paste| paste.end == self.input_cursor)
            .map(|paste| paste.start)
            .or_else(|| {
                self.input[..self.input_cursor]
                    .grapheme_indices(true)
                    .next_back()
                    .map(|(offset, _)| offset)
            });
        if let Some(start) = start {
            self.remove_input_range(start, self.input_cursor);
        }
    }

    pub(crate) fn delete_input_char(&mut self) {
        let end = self
            .input_pastes
            .iter()
            .find(|paste| paste.start == self.input_cursor)
            .map(|paste| paste.end)
            .or_else(|| {
                self.input[self.input_cursor..]
                    .graphemes(true)
                    .next()
                    .map(|grapheme| self.input_cursor + grapheme.len())
            });
        if let Some(end) = end {
            self.remove_input_range(self.input_cursor, end);
        }
    }

    pub(crate) fn remove_input_range(&mut self, start: usize, end: usize) {
        self.input.drain(start..end);
        self.input_cursor = start;
        self.input_pastes.retain_mut(|paste| {
            if paste.start < end && paste.end > start {
                return false;
            }
            if paste.start >= end {
                paste.start -= end - start;
                paste.end -= end - start;
            }
            true
        });
        self.reset_history_navigation();
    }

    pub(crate) fn move_cursor_left(&mut self) {
        if let Some(paste) = self
            .input_pastes
            .iter()
            .find(|paste| paste.end == self.input_cursor)
        {
            self.input_cursor = paste.start;
        } else if let Some((offset, _)) = self.input[..self.input_cursor]
            .grapheme_indices(true)
            .next_back()
        {
            self.input_cursor = offset;
        }
    }

    pub(crate) fn move_cursor_right(&mut self) {
        if let Some(paste) = self
            .input_pastes
            .iter()
            .find(|paste| paste.start == self.input_cursor)
        {
            self.input_cursor = paste.end;
        } else if let Some(grapheme) = self.input[self.input_cursor..].graphemes(true).next() {
            self.input_cursor += grapheme.len();
        }
    }

    pub(crate) fn input_newline_is_visible(&self, offset: usize) -> bool {
        !self
            .input_pastes
            .iter()
            .any(|paste| paste.start <= offset && offset < paste.end)
    }

    pub(crate) fn cursor_to_line_start(&mut self) {
        self.input_cursor = self.input[..self.input_cursor]
            .match_indices('\n')
            .rfind(|(offset, _)| self.input_newline_is_visible(*offset))
            .map(|(index, _)| index + 1)
            .unwrap_or(0);
    }

    pub(crate) fn cursor_to_line_end(&mut self) {
        self.input_cursor = self.input[self.input_cursor..]
            .match_indices('\n')
            .find(|(offset, _)| self.input_newline_is_visible(self.input_cursor + *offset))
            .map(|(index, _)| self.input_cursor + index)
            .unwrap_or(self.input.len());
    }

    pub(crate) fn clear_input(&mut self) {
        self.input_media.clear();
        self.input.clear();
        self.input_cursor = 0;
        self.input_pastes.clear();
        self.reset_history_navigation();
    }

    pub(crate) fn clear_visible(&mut self) {
        self.entries.clear();
        self.transcript_scrollback = TranscriptScrollback::default();
        self.transcript_view.get_mut().reset();
        self.clear_input();
        self.close_overlay(|overlay| {
            matches!(overlay, Overlay::Multiline(_) | Overlay::Selector(_))
        });
        self.live_entry_index = None;
        self.live_thinking_index = None;
        self.live_tool_indices.clear();
        self.nested_rows = None;
        self.previews_expanded = false;
    }

    pub(crate) fn insert_input_newline(&mut self) {
        self.insert_input_str("\n");
    }

    /// Double-press gate for quit keys on an empty input: the first press
    /// arms a confirmation window; a second press inside the window quits.
    pub(crate) fn quit_requested(&mut self, now: Instant) -> bool {
        let armed = self
            .quit_armed_since
            .map(|since| now.saturating_duration_since(since) < QUIT_CONFIRM_WINDOW)
            .unwrap_or(false);
        if armed {
            return true;
        }
        self.quit_armed_since = Some(now);
        false
    }

    pub(crate) fn push_placeholder(
        &mut self,
        kind: TuiEntryKind,
        text: impl Into<String>,
    ) -> usize {
        self.transcript_view
            .get_mut()
            .invalidate(self.entries.len());
        self.entries.push(TuiEntry {
            kind,
            text: text.into(),
        });
        let index = self.entries.len() - 1;
        self.live_entry_index = Some(index);
        index
    }

    pub(crate) fn replace_entry(&mut self, index: usize, text: impl Into<String>) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.text = text.into();
            self.transcript_view.get_mut().replaced(index);
            if let Some(progress) = self.transcript_scrollback.entries.get_mut(index) {
                *progress = EntryScrollback::default();
            }
        }
    }

    pub(crate) fn append_entry(&mut self, index: usize, text: &str) {
        if let Some(entry) = self.entries.get_mut(index) {
            entry.text.push_str(text);
            self.transcript_view.get_mut().invalidate(index);
        }
    }

    pub(crate) fn remove_entry(&mut self, index: usize) {
        self.entries.remove(index);
        self.transcript_scrollback.remove(index);
        self.transcript_view.get_mut().remove(index);
    }

    pub(crate) fn finish_live_entry(&mut self) {
        self.live_thinking_index = None;
        self.turn_running = false;
        self.turn_saw_tools = false;
        self.live_entry_index = None;
    }

    #[cfg(test)]
    pub(crate) fn finalized_entry_count(&self) -> usize {
        self.live_entry_index.unwrap_or(self.entries.len())
    }

    pub(crate) fn transcript_text(&self) -> String {
        self.entries
            .iter()
            .map(|entry| {
                let label = match entry.kind {
                    TuiEntryKind::Thinking => "thinking",
                    TuiEntryKind::System => "system",
                    TuiEntryKind::User => "user",
                    TuiEntryKind::Assistant => "assistant",
                    TuiEntryKind::Tool => "tool",
                    TuiEntryKind::Error => "error",
                };
                format!("{label}> {}", entry.text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// Scrollback is a rendering watermark, not conversation state. Logical-line
// and UTF-8 offsets survive rewrapping at a different terminal width. Tracking
// only a count of painted rows would duplicate or skip text after a resize.
#[derive(Debug, Default)]
pub(crate) struct TranscriptScrollback {
    pub(crate) entries: Vec<EntryScrollback>,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct EntryScrollback {
    pub(crate) position: TranscriptPosition,
    pub(crate) complete: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TranscriptPosition {
    pub(crate) line: usize,
    pub(crate) byte: usize,
}

impl TranscriptScrollback {
    pub(crate) fn remove(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.remove(index);
        }
    }
}

#[derive(Debug)]
pub(crate) struct TranscriptRow {
    pub(crate) line: Line<'static>,
    pub(crate) start: TranscriptPosition,
    pub(crate) end: TranscriptPosition,
    // Preview text can stand for a larger source range, not a literal slice.
    pub(crate) synthetic: bool,
}

pub(crate) fn wrap_positioned_line(
    line: Line<'static>,
    index: usize,
    width: usize,
) -> Vec<TranscriptRow> {
    let width = width.max(1);
    let line_style = line.style;
    let mut rows = Vec::new();
    let mut current = Vec::new();
    let mut cells = 0;
    let mut byte = 0;
    let mut start = TranscriptPosition {
        line: index,
        byte: 0,
    };
    for span in line.spans {
        let mut segment = String::new();
        for grapheme in span.content.graphemes(true) {
            // Expand tabs, and keep terminal control bytes out of painted text.
            let text = if grapheme == "\t" { "    " } else { grapheme };
            let text = text
                .chars()
                .filter(|ch| !ch.is_control())
                .collect::<String>();
            let used = text.width();
            if cells > 0 && cells + used > width {
                push_transcript_segment(&mut current, &mut segment, span.style);
                let end = TranscriptPosition { line: index, byte };
                rows.push(TranscriptRow {
                    line: Line::from(std::mem::take(&mut current)).style(line_style),
                    start,
                    end,
                    synthetic: false,
                });
                start = end;
                cells = 0;
            }
            segment.push_str(&text);
            byte += grapheme.len();
            cells += used;
        }
        push_transcript_segment(&mut current, &mut segment, span.style);
    }
    rows.push(TranscriptRow {
        line: Line::from(current).style(line_style),
        start,
        end: if byte == 0 {
            TranscriptPosition {
                line: index + 1,
                byte: 0,
            }
        } else {
            TranscriptPosition { line: index, byte }
        },
        synthetic: false,
    });
    rows
}

pub(crate) fn collapsed_tool_rows(
    logical: &[Line<'static>],
    rows: Vec<TranscriptRow>,
    width: usize,
    palette: &ThemePalette,
) -> Vec<TranscriptRow> {
    // Keep command/path headers on one row so a long command cannot consume
    // the preview. On large results show the final output row, like Kimi.
    let mut header = wrap_positioned_line(logical[0].clone(), 0, usize::MAX)
        .remove(0)
        .line;
    let output = rows
        .into_iter()
        .filter(|row| row.start.line > 0)
        .collect::<Vec<_>>();
    let folded = output.len() > 2;
    if folded {
        let suffix = format!(" · {} more lines", output.len() - 1);
        header = clip_line_width(header, width.saturating_sub(suffix.width()));
        header.spans.push(Span::styled(suffix, palette.secondary()));
    }
    let mut preview = vec![TranscriptRow {
        line: clip_line_width(header, width),
        start: TranscriptPosition::default(),
        end: TranscriptPosition { line: 1, byte: 0 },
        synthetic: true,
    }];
    if folded {
        let tail = logical.last().expect("tool output");
        let text = tail
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let line = wrap_positioned_line(
            Line::from(Span::styled(
                format!("  … {}", text.trim()),
                palette.secondary(),
            )),
            0,
            usize::MAX,
        )
        .remove(0)
        .line;
        preview.push(TranscriptRow {
            line: clip_line_width(line, width),
            start: TranscriptPosition { line: 1, byte: 0 },
            end: output.last().expect("folded output").end,
            synthetic: true,
        });
    } else {
        preview.extend(output);
    }
    preview
}

pub(crate) fn pending_entry_rows(
    entry: &TuiEntry,
    index: usize,
    width: usize,
    palette: &ThemePalette,
    state: &PreviewState,
    progress: EntryScrollback,
) -> Vec<TranscriptRow> {
    if progress.complete {
        return Vec::new();
    }
    let mut logical = render_entry_lines(std::slice::from_ref(entry), palette);
    if logical.is_empty() {
        return Vec::new();
    }
    logical.pop(); // spacer is added after preview selection
    let mut rows = Vec::new();
    for (line_index, line) in logical.iter().cloned().enumerate() {
        let mut wrapped = wrap_positioned_line(line, line_index, width);
        if line_index + 1 < logical.len() || entry.text.ends_with('\n') {
            wrapped.last_mut().expect("logical row").end = TranscriptPosition {
                line: line_index + 1,
                byte: 0,
            };
        }
        rows.extend(wrapped);
    }
    let collapsed = state.expanded_from.is_none_or(|start| index < start);
    if collapsed && entry.kind == TuiEntryKind::Tool {
        rows = collapsed_tool_rows(&logical, rows, width, palette);
    }
    // Markdown emphasis restyles wrapped assistant rows in place. Selection
    // geometry stays anchored to the plain logical lines; only span styles
    // (and link display text) differ.
    {
        let logical_lines: Vec<String> = logical
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        let base = Style::default().fg(palette.foreground);
        restyle_rows_with_markdown(
            &mut rows,
            entry,
            |line| logical_lines.get(line).cloned(),
            base,
            palette.accent,
        );
    }
    let limit = if entry.kind == TuiEntryKind::Thinking {
        2
    } else {
        usize::MAX
    };
    if collapsed && rows.len() > limit {
        let hidden = rows.len() - limit;
        if state.live_thinking == Some(index) {
            rows = rows.split_off(hidden);
        } else {
            let start = rows[limit].start;
            let end = rows.last().expect("nonempty preview").end;
            rows.truncate(limit);
            rows.push(TranscriptRow {
                line: clip_line_width(
                    Line::from(Span::styled(
                        format!("  … ({hidden} more lines, ctrl+o to expand)"),
                        palette.secondary(),
                    )),
                    width,
                ),
                start,
                end,
                synthetic: true,
            });
        }
    }
    rows.push(TranscriptRow {
        line: Line::from(""),
        start: TranscriptPosition {
            line: logical.len(),
            byte: 0,
        },
        end: TranscriptPosition {
            line: logical.len() + 1,
            byte: 0,
        },
        synthetic: false,
    });
    // A new wrap can straddle the watermark after a resize. Trim only the
    // archived prefix, using source bytes (not tab-expanded painted bytes).
    let mut pending = Vec::new();
    for mut row in rows {
        if row.end <= progress.position {
            continue;
        }
        if !row.synthetic && row.start < progress.position && progress.position.line < logical.len()
        {
            let source = &logical[progress.position.line];
            let end = if row.end.line == progress.position.line {
                row.end.byte
            } else {
                source.spans.iter().map(|span| span.content.len()).sum()
            };
            let mut offset = 0;
            let spans = source
                .spans
                .iter()
                .filter_map(|span| {
                    let from = progress
                        .position
                        .byte
                        .saturating_sub(offset)
                        .min(span.content.len());
                    let to = end.saturating_sub(offset).min(span.content.len());
                    offset += span.content.len();
                    (from < to)
                        .then(|| Span::styled(span.content[from..to].to_string(), span.style))
                })
                .collect::<Vec<_>>();
            row.line = wrap_positioned_line(Line::from(spans).style(source.style), 0, width)
                .into_iter()
                .map(|row| row.line)
                .next()
                .unwrap_or_default();
            row.start = progress.position;
        }
        pending.push(row);
    }
    pending
}

pub(crate) fn transcript_preview_state(app: &TuiApp) -> PreviewState {
    PreviewState {
        expanded_from: app
            .previews_expanded
            .then(|| recent_turn_start(&app.entries)),
        live_thinking: app.live_thinking_index,
    }
}

pub(crate) fn transcript_overflow(
    app: &mut TuiApp,
    config: &LoadedConfig,
    area: Rect,
    palette: &ThemePalette,
) -> Vec<Line<'static>> {
    // Overlays and narrower diff views are temporary: do not irreversibly
    // archive transcript text merely because a panel covers it.
    if area.width == 0 || area.height == 0 || app.selector().is_some() || app.diff_panel().is_some()
    {
        return Vec::new();
    }
    app.transcript_scrollback
        .entries
        .resize(app.entries.len(), EntryScrollback::default());
    let state = transcript_preview_state(app);
    let mut chunks = Vec::new();
    let mut height = 0usize;
    // Include all pending entries, not only overscan: a provider can deliver
    // several screenfuls in one delta, and session replay is a single paint.
    for (index, entry) in app.entries.iter().enumerate() {
        if config.settings.hide_thinking_block.unwrap_or(false)
            && entry.kind == TuiEntryKind::Thinking
        {
            continue;
        }
        let progress = app.transcript_scrollback.entries[index];
        let rows = pending_entry_rows(entry, index, area.width as usize, palette, &state, progress);
        height += rows.len();
        if !rows.is_empty() {
            chunks.push((index, rows));
        }
    }
    if let Some((_, rows)) = chunks.last_mut() {
        while rows.last().is_some_and(|row| row.line.width() == 0) {
            rows.pop();
            height = height.saturating_sub(1);
        }
    }
    let mut remaining = height.saturating_sub(area.height as usize);
    let mut overflow = Vec::new();
    for (index, rows) in chunks {
        let live = app.live_entry_index == Some(index)
            || app.live_thinking_index == Some(index)
            || app.live_tool_indices.contains(&index);
        // A live entry's spacer is not final. Do not consume it or any later
        // entry until this one finishes, preserving chronological scrollback.
        let available = rows.len().saturating_sub(usize::from(live));
        let count = remaining.min(available);
        if count == 0 {
            break;
        }
        let progress = &mut app.transcript_scrollback.entries[index];
        progress.position = rows[count - 1].end;
        // Never seal a live entry: future deltas must still be paintable.
        progress.complete = count == rows.len() && !live;
        overflow.extend(rows.into_iter().take(count).map(|row| row.line));
        remaining -= count;
        if live {
            break;
        }
    }
    overflow
}

pub(crate) type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

pub(crate) struct TuiSurface<'a> {
    pub(crate) terminal: &'a mut TuiTerminal,
    pub(crate) viewport_height: &'a mut u16,
}

pub(crate) fn new_tui_terminal(height: u16, owned_screen: bool) -> Result<TuiTerminal> {
    let backend = CrosstermBackend::new(io::stdout());
    Ok(Terminal::with_options(
        backend,
        TerminalOptions {
            // Leave a valid two-row native scroll region above the live UI.
            // A full-screen inline viewport makes ratatui borrow a one-row
            // region, which tmux (and VT terminals) rejects and scrolls the
            // entire editor instead. The reserved rows hold transcript text.
            viewport: if owned_screen {
                Viewport::Fullscreen
            } else {
                Viewport::Inline(height.saturating_sub(2).max(1))
            },
        },
    )?)
}

pub(crate) fn terminal_viewport_height() -> u16 {
    crossterm::terminal::size()
        .map(|(_, height)| height.max(1))
        .unwrap_or(24)
}

pub(crate) fn resize_tui_viewport(
    terminal: &mut TuiTerminal,
    viewport_height: &mut u16,
    owned_screen: bool,
) -> Result<()> {
    let desired = terminal_viewport_height();
    if owned_screen || desired == *viewport_height {
        *viewport_height = desired;
        terminal.autoresize()?;
        return Ok(());
    }
    *terminal = new_tui_terminal(desired, owned_screen)?;
    terminal.clear()?;
    *viewport_height = desired;
    Ok(())
}

pub(crate) fn redraw_tui<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut TuiApp,
    config: &LoadedConfig,
) -> Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    terminal.autoresize()?;
    let area = tui_layout(terminal.get_frame().area(), app, config)[0];
    let palette = terminal_theme(config).unwrap_or_default().palette;
    let overflow = if app.owned_screen {
        Vec::new()
    } else {
        transcript_overflow(app, config, area, &palette)
    };
    // Bound temporary terminal buffers even when a resumed message is huge.
    for rows in overflow.chunks(128) {
        terminal.insert_before(rows.len() as u16, |buffer| {
            Paragraph::new(rows.to_vec())
                .style(palette.base())
                .render(buffer.area, buffer);
        })?;
    }
    terminal.draw(|frame| draw_tui(frame, app, config))?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn rendered_lines_height(lines: &[Line<'_>], width: usize) -> u16 {
    lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum::<usize>()
        .min(u16::MAX as usize) as u16
}

pub(crate) const TRANSCRIPT_RENDER_OVERSCAN_ROWS: u16 = 4;
pub(crate) const STREAM_RENDER_INTERVAL_MS: u64 = 16;

// Entry marker glyph. A terminal cell is a fixed size, so a glyph cannot be
// literally scaled; use a larger filled circle instead of the small `•`
// (U+2022). `●` (U+25CF) is single-width and reliably bigger. Swap to `⬤`
// (U+2B24) for a still-larger circle, but that one is wide/font-dependent and
// can misalign the two-space continuation indent.
pub(crate) const BULLET: &str = "●";

pub(crate) struct VisibleTranscript {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) visual_height: u16,
    #[cfg(test)]
    pub(crate) entries_used: usize,
}

#[cfg(test)]
pub(crate) fn visible_transcript(
    entries: &[TuiEntry],
    width: usize,
    height: u16,
) -> VisibleTranscript {
    visible_transcript_with_thinking(entries, width, height, false, &ThemePalette::default())
}

#[cfg(test)]
pub(crate) fn visible_transcript_with_thinking(
    entries: &[TuiEntry],
    width: usize,
    height: u16,
    hide_thinking: bool,
    palette: &ThemePalette,
) -> VisibleTranscript {
    visible_transcript_with_previews(
        entries,
        width,
        height,
        hide_thinking,
        palette,
        PreviewState::default(),
    )
}

#[derive(Default)]
pub(crate) struct PreviewState {
    pub(crate) expanded_from: Option<usize>,
    pub(crate) live_thinking: Option<usize>,
}

pub(crate) fn recent_turn_start(entries: &[TuiEntry]) -> usize {
    entries
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, entry)| entry.kind == TuiEntryKind::User)
        .nth(2)
        .map(|(index, _)| index)
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) fn preview_entry(
    entry: &TuiEntry,
    index: usize,
    width: usize,
    palette: &ThemePalette,
    state: &PreviewState,
) -> Vec<Line<'static>> {
    pending_entry_rows(
        entry,
        index,
        width,
        palette,
        state,
        EntryScrollback::default(),
    )
    .into_iter()
    .map(|row| row.line)
    .collect()
}

#[cfg(test)]
pub(crate) fn visible_transcript_with_previews(
    entries: &[TuiEntry],
    width: usize,
    height: u16,
    hide_thinking: bool,
    palette: &ThemePalette,
    state: PreviewState,
) -> VisibleTranscript {
    visible_pending_transcript(entries, width, height, hide_thinking, palette, state, &[])
}

pub(crate) fn visible_pending_transcript(
    entries: &[TuiEntry],
    width: usize,
    height: u16,
    hide_thinking: bool,
    palette: &ThemePalette,
    state: PreviewState,
    archived: &[EntryScrollback],
) -> VisibleTranscript {
    let width = width.max(1);
    let target_height = height.saturating_add(TRANSCRIPT_RENDER_OVERSCAN_ROWS) as usize;
    let mut chunks: Vec<Vec<Line<'static>>> = Vec::new();
    let mut scan_height: usize = 0;
    #[cfg(test)]
    let mut entries_used = 0;

    for (index, entry) in entries.iter().enumerate().rev() {
        if hide_thinking && entry.kind == TuiEntryKind::Thinking {
            continue;
        }
        let progress = archived.get(index).copied().unwrap_or_default();
        if progress.complete {
            continue;
        }
        let mut chunk = pending_entry_rows(entry, index, width, palette, &state, progress)
            .into_iter()
            .map(|row| row.line)
            .collect::<Vec<_>>();
        if chunks.is_empty() {
            while chunk.last().map(Line::width) == Some(0) {
                chunk.pop();
            }
        }
        if chunk.is_empty() {
            continue;
        }

        scan_height = scan_height.saturating_add(chunk.len());
        chunks.push(chunk);
        #[cfg(test)]
        {
            entries_used += 1;
        }
        if scan_height >= target_height {
            break;
        }
    }

    chunks.reverse();
    let mut lines = chunks.into_iter().flatten().collect::<Vec<_>>();
    if lines.len() > height as usize {
        lines = lines[lines.len() - height as usize..].to_vec();
    }
    let visual_height = lines.len().min(u16::MAX as usize) as u16;
    VisibleTranscript {
        lines,
        visual_height,
        #[cfg(test)]
        entries_used,
    }
}

pub(crate) fn push_transcript_segment(
    current: &mut Vec<Span<'static>>,
    segment: &mut String,
    style: Style,
) {
    if !segment.is_empty() {
        current.push(Span::styled(std::mem::take(segment), style));
    }
}

pub(crate) fn tui_layout(frame_area: Rect, app: &TuiApp, config: &LoadedConfig) -> [Rect; 7] {
    let slash_matches = slash_command_matches(config, app);
    let slash_match_height = (slash_matches.len().min(SLASH_MATCH_LIMIT) as u16)
        .min(frame_area.height.saturating_sub(4));
    let spacer_height = u16::from(app.queued_messages.is_empty());
    let input_height = input_area_height(app, frame_area, slash_match_height);
    let panel_budget = frame_area
        .height
        .saturating_sub(input_height + slash_match_height + spacer_height + 1);
    let queue_height = queue_panel_height(app).min(panel_budget);
    let todo_budget = panel_budget.saturating_sub(queue_height) / 2;
    let todo_height = if todo_budget >= 4 {
        todo_panel_height(app).min(todo_budget)
    } else {
        0
    };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(todo_height),
            Constraint::Length(queue_height),
            Constraint::Length(spacer_height),
            Constraint::Length(input_height),
            Constraint::Length(slash_match_height),
            Constraint::Length(1),
        ])
        .areas::<7>(frame_area)
}

pub(crate) fn transcript_area(area: Rect, app: &TuiApp) -> Rect {
    if app.diff_panel().is_some() {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(DIFF_PANEL_WIDTH)])
            .split(area)[0]
    } else {
        area
    }
}

pub(crate) fn draw_tui(frame: &mut Frame<'_>, app: &TuiApp, config: &LoadedConfig) {
    let palette = terminal_theme(config).unwrap_or_default().palette;
    frame.render_widget(Block::default().style(palette.base()), frame.area());
    let root = tui_layout(frame.area(), app, config);
    let slash_matches = slash_command_matches(config, app);
    let transcript_area = transcript_area(root[0], app);
    if app.diff_panel().is_some() {
        let diff_area = Rect::new(
            transcript_area.right(),
            root[0].y,
            root[0].right().saturating_sub(transcript_area.right()),
            root[0].height,
        );
        draw_diff_panel(frame, diff_area, app, &palette);
    }
    draw_transcript(frame, transcript_area, app, config, &palette);
    draw_todo_panel(frame, root[1], app, &palette);
    draw_queue_panel(frame, root[2], app, &palette);
    // Queue hints sit directly against the editor; otherwise leave a blank row.
    draw_input(frame, root[4], app, &palette);
    draw_slash_matches(frame, root[5], &slash_matches, &palette);
    draw_footer(frame, root[6], app, &palette);
    draw_selector_overlay(frame, app, &palette);
    set_tui_cursor(frame, root[4], app);
}

pub(crate) const DIFF_PANEL_WIDTH: u16 = 48;
pub(crate) const TODO_COLLAPSED_ROWS: usize = 5;
pub(crate) const TODO_EXPANDED_ROW_LIMIT: usize = 15;

pub(crate) fn todo_row_limit(expanded: bool) -> usize {
    if expanded {
        TODO_EXPANDED_ROW_LIMIT
    } else {
        TODO_COLLAPSED_ROWS
    }
}

pub(crate) fn selected_todos_with_limit(
    todos: &[TodoItem],
    expanded: bool,
    limit: usize,
) -> Vec<usize> {
    if expanded {
        return (0..todos.len().min(limit)).collect();
    }
    let mut selected = todos
        .iter()
        .enumerate()
        .filter(|(_, todo)| todo.status == TodoStatus::InProgress)
        .map(|(index, _)| index)
        .take(limit)
        .collect::<Vec<_>>();
    let recent_done = todos
        .iter()
        .rposition(|todo| todo.status == TodoStatus::Completed);
    let pending_slots = limit.saturating_sub(selected.len() + usize::from(recent_done.is_some()));
    selected.extend(
        todos
            .iter()
            .enumerate()
            .filter(|(_, todo)| todo.status == TodoStatus::Pending)
            .map(|(index, _)| index)
            .take(pending_slots),
    );
    if let Some(index) = recent_done
        && selected.len() < limit
    {
        selected.push(index);
    }
    for index in (0..todos.len()).rev() {
        if selected.len() >= limit {
            break;
        }
        if !selected.contains(&index) {
            selected.push(index);
        }
    }
    selected.sort_unstable();
    selected
}

pub(crate) fn todo_panel_lines(app: &TuiApp, palette: &ThemePalette) -> Vec<Line<'static>> {
    todo_panel_lines_with_limit(app, palette, todo_row_limit(app.todos_expanded))
}

pub(crate) fn todo_panel_lines_with_limit(
    app: &TuiApp,
    palette: &ThemePalette,
    limit: usize,
) -> Vec<Line<'static>> {
    if app.todos.is_empty() {
        return Vec::new();
    }
    let selected = selected_todos_with_limit(&app.todos, app.todos_expanded, limit);
    let mut lines = vec![Line::from(Span::styled(
        "  Todo",
        palette.secondary().add_modifier(Modifier::BOLD),
    ))];
    for index in &selected {
        let todo = &app.todos[*index];
        let (marker, marker_style, title_style) = match todo.status {
            TodoStatus::Completed => (
                "✓",
                Style::default().fg(palette.success),
                palette.secondary().add_modifier(Modifier::CROSSED_OUT),
            ),
            TodoStatus::InProgress => (
                "●",
                Style::default().fg(palette.accent),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            TodoStatus::Pending => ("○", palette.secondary(), Style::default()),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {marker} "), marker_style),
            Span::styled(single_line(&todo.content), title_style),
        ]));
    }
    let hidden = app
        .todos
        .iter()
        .enumerate()
        .filter(|(index, _)| !selected.contains(index))
        .map(|(_, todo)| todo)
        .collect::<Vec<_>>();
    if !hidden.is_empty() {
        let mut counts = Vec::new();
        for (status, label) in [
            (TodoStatus::Pending, "pending"),
            (TodoStatus::InProgress, "active"),
            (TodoStatus::Completed, "done"),
        ] {
            let count = hidden.iter().filter(|todo| todo.status == status).count();
            if count > 0 {
                counts.push(format!("{count} {label}"));
            }
        }
        lines.push(Line::from(Span::styled(
            format!(
                "  … +{} more ({}) · ctrl+t to {}",
                hidden.len(),
                counts.join(", "),
                if app.todos_expanded {
                    "collapse"
                } else {
                    "expand"
                }
            ),
            palette.secondary(),
        )));
    } else if app.todos_expanded && !app.todos.is_empty() {
        lines.push(Line::from(Span::styled(
            "  ctrl+t to collapse",
            palette.secondary(),
        )));
    }
    lines
}

pub(crate) fn todo_panel_borders(app: &TuiApp) -> Borders {
    if app.queued_messages.is_empty() {
        Borders::TOP | Borders::BOTTOM
    } else {
        Borders::TOP
    }
}

pub(crate) fn todo_panel_height(app: &TuiApp) -> u16 {
    if app.todos.is_empty() {
        0
    } else {
        todo_panel_lines(app, &ThemePalette::default()).len() as u16
            + 1
            + u16::from(app.queued_messages.is_empty())
    }
}

pub(crate) fn draw_todo_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &TuiApp,
    palette: &ThemePalette,
) {
    if area.height == 0 || app.todos.is_empty() {
        return;
    }
    let block = Block::default()
        .borders(todo_panel_borders(app))
        .border_style(palette.secondary().fg(palette.border));
    let inner = block.inner(area);
    let capacity = (inner.height as usize).saturating_sub(1);
    let needs_hint =
        app.todos_expanded || app.todos.len() > capacity.min(todo_row_limit(app.todos_expanded));
    let limit = capacity
        .saturating_sub(usize::from(needs_hint))
        .min(todo_row_limit(app.todos_expanded));
    let lines = todo_panel_lines_with_limit(app, palette, limit)
        .into_iter()
        .map(|line| clip_line_width(line, inner.width as usize))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

pub(crate) const QUEUE_VISIBLE_ROWS: usize = 4;

pub(crate) fn queue_panel_height(app: &TuiApp) -> u16 {
    if app.queued_messages.is_empty() {
        0
    } else {
        (app.queued_messages.len().min(QUEUE_VISIBLE_ROWS) + 2) as u16
    }
}

pub(crate) fn draw_queue_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &TuiApp,
    palette: &ThemePalette,
) {
    if area.height == 0 || app.queued_messages.is_empty() {
        return;
    }
    let mut lines = vec![Line::from(Span::styled(
        "─".repeat(area.width as usize),
        palette.secondary().fg(palette.border),
    ))];
    let visible_rows = QUEUE_VISIBLE_ROWS.min(area.height.saturating_sub(2) as usize);
    let overflow = app.queued_messages.len().saturating_sub(visible_rows);
    for message in app.queued_messages.iter().skip(overflow) {
        let text = single_line(message);
        lines.push(Line::from(vec![
            Span::styled("  ❯ ", Style::default().fg(palette.accent)),
            Span::styled(
                truncate_width(&text, area.width.saturating_sub(4) as usize),
                Style::default().fg(palette.foreground),
            ),
        ]));
    }
    let hint = if app.turn_running {
        if app.turn_saw_tools {
            "↑ to edit · ctrl+s to steer at next boundary"
        } else {
            // A prose-only response has no tool-call round boundary to inject
            // into: steering is delivered only after this response finishes.
            "↑ to edit · ctrl+s queues after this response"
        }
    } else {
        "↑ to edit · runs after next turn"
    };
    lines.push(Line::from(Span::styled(
        if overflow > 0 {
            format!("  … +{overflow} more · {hint}")
        } else {
            format!("  {hint}")
        },
        palette.secondary(),
    )));
    let lines = lines
        .into_iter()
        .map(|line| clip_line_width(line, area.width as usize))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

pub(crate) fn recall_queued_message(app: &mut TuiApp, message: String) {
    app.clear_input();
    app.input = message;
    app.input_cursor = app.input.len();
}

pub(crate) fn draw_diff_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &TuiApp,
    palette: &ThemePalette,
) {
    let Some(panel) = app.diff_panel() else {
        return;
    };
    if area.height == 0 || area.width == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(palette.border));
    if let Some(detail) = panel.detail.as_ref() {
        let block = block.title(format!(" {} ", detail.path));
        let visible_height = area.height.saturating_sub(1) as usize;
        let lines = detail
            .lines
            .iter()
            .skip(detail.scroll)
            .take(visible_height)
            .map(|line| colorized_diff_line(line, palette))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }
    let block = block.title(format!(" edited files ({}) ", panel.files.len()));
    let mut lines = Vec::new();
    for (index, entry) in panel.files.iter().enumerate() {
        let stats = match (entry.added, entry.removed) {
            (Some(added), Some(removed)) => format!(" +{added} -{removed}"),
            _ if entry.untracked => " new".to_string(),
            _ => String::new(),
        };
        let line = Line::from(format!("{}{stats}", entry.path));
        lines.push(if index == panel.selected {
            line.style(palette.selected())
        } else {
            line
        });
    }
    if panel.files.is_empty() {
        lines.push(Line::from("no files edited this session").style(palette.secondary()));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

pub(crate) fn colorized_diff_line(line: &str, palette: &ThemePalette) -> Line<'static> {
    let style = if line.starts_with("+++") || line.starts_with("---") {
        palette.secondary()
    } else if line.starts_with('+') {
        Style::default().fg(palette.success)
    } else if line.starts_with('-') {
        Style::default().fg(palette.error)
    } else if line.starts_with("@@") {
        Style::default().fg(palette.accent)
    } else {
        Style::default()
    };
    Line::from(line.to_string()).style(style)
}

pub(crate) const MAX_INPUT_ROWS: usize = 10;

pub(crate) fn input_area_height(app: &TuiApp, frame_area: Rect, slash_match_height: u16) -> u16 {
    let desired = input_desired_height(app, frame_area.width);
    let available = frame_area
        .height
        .saturating_sub(slash_match_height)
        .saturating_sub(1 + u16::from(app.queued_messages.is_empty()));
    desired.min(available)
}

pub(crate) fn input_desired_height(app: &TuiApp, width: u16) -> u16 {
    let inner = input_block(&ThemePalette::default()).inner(Rect::new(0, 0, width, u16::MAX));
    (input_layout(app, inner.width as usize)
        .rows
        .len()
        .min(MAX_INPUT_ROWS)
        + 2
        + usize::from(app.multiline().is_some())
        + usize::from(!app.input_media.is_empty())) as u16
}

pub(crate) fn draw_transcript(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &TuiApp,
    config: &LoadedConfig,
    palette: &ThemePalette,
) {
    if app.owned_screen {
        let mut view = app.transcript_view.borrow_mut();
        view.sync(
            &app.entries,
            area,
            palette,
            config.settings.hide_thinking_block.unwrap_or(false),
            &transcript_preview_state(app),
        );
        let lines = view.visible_lines();
        let body = view.area;
        let y = view.visible_top();
        frame.render_widget(
            Paragraph::new(lines).style(palette.base()),
            Rect {
                y,
                height: body.bottom().saturating_sub(y),
                ..body
            },
        );
        if !view.hint().is_empty() {
            frame.render_widget(
                Paragraph::new(view.hint()).style(palette.secondary()),
                view.hint_area,
            );
        }
        return;
    }
    if area.height == 0 || area.width == 0 {
        return;
    }
    let visible = visible_pending_transcript(
        &app.entries,
        area.width as usize,
        area.height,
        config.settings.hide_thinking_block.unwrap_or(false),
        palette,
        transcript_preview_state(app),
        &app.transcript_scrollback.entries,
    );
    if visible.lines.is_empty() {
        return;
    }
    let y = area.y + area.height.saturating_sub(visible.visual_height);
    let height = visible.visual_height.min(area.height);
    let render_area = Rect { y, height, ..area };
    let paragraph = Paragraph::new(visible.lines).style(Style::default().fg(palette.foreground));
    frame.render_widget(paragraph, render_area);
}

pub(crate) fn render_entry_lines(
    entries: &[TuiEntry],
    palette: &ThemePalette,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for entry in entries {
        match entry.kind {
            TuiEntryKind::Thinking if entry.text.is_empty() => continue,
            TuiEntryKind::Thinking => push_marked_lines(
                &mut lines,
                "thinking",
                &entry.text,
                palette.secondary(),
                palette.secondary().add_modifier(Modifier::ITALIC),
            ),
            TuiEntryKind::Tool => push_tool_lines(&mut lines, &entry.text, palette),
            TuiEntryKind::Error => push_marked_lines(
                &mut lines,
                "error",
                &entry.text,
                Style::default()
                    .fg(palette.error)
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(palette.error),
            ),
            TuiEntryKind::System => push_marked_lines(
                &mut lines,
                "",
                &entry.text,
                palette.secondary(),
                palette.secondary(),
            ),
            TuiEntryKind::User | TuiEntryKind::Assistant => {
                push_marked_lines(
                    &mut lines,
                    "",
                    &entry.text,
                    palette.secondary(),
                    Style::default().fg(palette.foreground),
                );
            }
        }
        lines.push(Line::from(""));
    }
    lines
}

pub(crate) fn push_marked_lines(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    text: &str,
    marker_style: Style,
    text_style: Style,
) {
    let mut text_lines = text.lines();
    let first = text_lines.next().unwrap_or_default();
    let prefix = if label.is_empty() {
        format!("{BULLET} ")
    } else {
        format!("{BULLET} {label} ")
    };
    lines.push(Line::from(vec![
        Span::styled(prefix, marker_style),
        Span::styled(first.to_string(), text_style),
    ]));
    for line in text_lines {
        lines.push(Line::from(Span::styled(format!("  {line}"), text_style)));
    }
}

/// Restyle wrapped assistant rows with inline markdown emphasis. Runs after
/// wrapping so selection geometry (built from the plain logical lines) is
/// untouched: the same graphemes keep the same byte positions, only span
/// styles change. Links are the exception — their display text is the link
/// label only — so a link row's display differs from its source slice and
/// selection copies the raw markdown, like folded tool previews.
pub(crate) fn restyle_rows_with_markdown(
    rows: &mut [TranscriptRow],
    entry: &TuiEntry,
    logical_source_line: impl Fn(usize) -> Option<String>,
    base: Style,
    accent: ratatui::style::Color,
) {
    if entry.kind != TuiEntryKind::Assistant {
        return;
    }
    // Fence state walks rows in display order: emphasis is suppressed and
    // rows are dimmed between opening and closing fences. The fence marker
    // lines themselves collapse to a dim language label.
    // The marker of the code block a row is in, which alone closes it.
    let mut in_fence: Option<&'static str> = None;
    // Ordered-list state: an ordered item numbers itself from the list's
    // start marker plus its position (upstream's list renderer), so a
    // source that numbers every item `1.` still displays `1. 2. 3.`. A
    // plain line or a bullet ends the sequence.
    let mut ordered_list: Option<(u64, u64)> = None;
    // Inside an HTML comment that began on an earlier row: rows show
    // nothing until it closes (upstream tui 54280c4f).
    let mut in_comment = false;
    for row in rows.iter_mut() {
        if row.synthetic {
            continue;
        }
        let Some(source_line) = logical_source_line(row.start.line) else {
            continue;
        };
        // Only the first row of a logical line applies block transforms;
        // wrapped continuations render as plain content of that line.
        let starts_line = row.start.byte == 0;
        let end = if row.end.line == row.start.line {
            row.end.byte
        } else {
            source_line.len()
        };
        let Some(row_text) = source_line.get(row.start.byte..end) else {
            continue;
        };
        if let Some(marker) = in_fence {
            // Inside a code block: no emphasis, dim accent bar, indent.
            if source_line.trim_start().starts_with(marker) && starts_line {
                in_fence = None;
                row.line = Line::from(Span::styled(String::new(), base));
                continue;
            }
            row.line = Line::from(vec![
                Span::styled("  ┆ ", Style::default().fg(accent)),
                Span::styled(row_text.to_string(), base),
            ]);
            continue;
        }
        // HTML comments are not shown (upstream tui 54280c4f): what is
        // left of the row is plain text, and a row that was only a comment
        // is blank.
        if in_comment || row_text.contains("<!--") {
            let (visible, open) = without_html_comments(row_text, in_comment);
            in_comment = open;
            row.line = if visible.trim().is_empty() {
                Line::from(Span::styled(String::new(), base))
            } else {
                Line::from(styled_spans(&visible, base, accent))
            };
            continue;
        }
        // The first logical line carries the "[provider/model] " echo
        // prefix (and the bullet marker before that); classify on the
        // markdown content only.
        let classify_target: &str = if row.start.line == 0 {
            source_line
                .strip_prefix("● ")
                .and_then(|rest| rest.find("] ").map(|i| &rest[i + 2..]))
                .unwrap_or(&source_line)
        } else {
            &source_line
        };
        let mut kind = if starts_line {
            markdown::classify_line(classify_target)
        } else {
            markdown::BlockKind::Plain
        };
        if starts_line {
            match &mut kind {
                markdown::BlockKind::ListItem {
                    ordered: true,
                    marker,
                } => {
                    let (start, position) = match ordered_list {
                        Some(state) => (state.0, state.1 + 1),
                        None => (marker.unwrap_or(1), 1),
                    };
                    *marker = Some(start + position - 1);
                    ordered_list = Some((start, position));
                }
                markdown::BlockKind::ListItem {
                    ordered: false,
                    marker,
                } => {
                    *marker = None;
                    ordered_list = None;
                }
                _ => ordered_list = None,
            }
        } else if matches!(kind, markdown::BlockKind::ListItem { .. }) {
            ordered_list = None;
        }
        // Headings render their content after the markers; compute from the
        // classification target, not the raw source line.
        let heading_content = |kind: &markdown::BlockKind, target: &str| -> String {
            match kind {
                markdown::BlockKind::Heading(level) => {
                    let hashes = *level as usize + 1;
                    target
                        .get(hashes..)
                        .unwrap_or(target)
                        .trim_start()
                        .to_string()
                }
                _ => String::new(),
            }
        };
        match &kind {
            markdown::BlockKind::Fence { language, .. } => {
                in_fence = markdown::fence_marker(classify_target);
                let label = if language.is_empty() {
                    "  ┆ code".to_string()
                } else {
                    format!("  ┆ {language}")
                };
                row.line = Line::from(vec![Span::styled(label, Style::default().fg(accent))]);
            }
            markdown::BlockKind::Heading(level) => {
                let content = heading_content(&kind, classify_target);
                let style = base.add_modifier(Modifier::BOLD).fg(accent);
                row.line = Line::from(vec![Span::styled(
                    format!("{}{content}", " ".repeat(*level as usize)),
                    style,
                )]);
            }
            markdown::BlockKind::ListItem { .. } | markdown::BlockKind::Blockquote => {
                let render = markdown::block_render(&kind, classify_target);
                let content = classify_target
                    .get(render.content_byte..)
                    .unwrap_or(row_text);
                let marker_style = if matches!(kind, markdown::BlockKind::Blockquote) {
                    base.add_modifier(Modifier::DIM)
                } else {
                    base
                };
                let mut spans = vec![Span::styled(render.prefix.clone(), marker_style)];
                spans.extend(styled_spans(content.trim_start(), marker_style, accent));
                row.line = Line::from(spans);
            }
            markdown::BlockKind::Plain => {
                let spans = styled_spans(row_text, base, accent);
                row.line = Line::from(spans);
            }
        }
    }
}

/// `text` without its HTML comments, `<!-- ... -->`, starting inside one
/// when `in_comment`; and whether a comment is still open at its end.
pub(crate) fn without_html_comments(text: &str, in_comment: bool) -> (String, bool) {
    let mut visible = String::new();
    let mut rest = text;
    if in_comment {
        match rest.find("-->") {
            Some(end) => rest = &rest[end + 3..],
            None => return (visible, true),
        }
    }
    loop {
        let Some(start) = rest.find("<!--") else {
            visible.push_str(rest);
            return (visible, false);
        };
        visible.push_str(&rest[..start]);
        rest = &rest[start + 4..];
        match rest.find("-->") {
            Some(end) => rest = &rest[end + 3..],
            None => return (visible, true),
        }
    }
}

/// Convert one markdown line to styled spans over its plain base style.
pub(crate) fn styled_spans(
    line: &str,
    base: Style,
    accent: ratatui::style::Color,
) -> Vec<Span<'static>> {
    markdown::parse_inline_line(line)
        .into_iter()
        .map(|span| {
            let style = match span.style {
                markdown::SpanStyle::Plain => base,
                // base first: the markdown style's fg/modifiers must win over
                // the plain base (base.fg is Some(Reset) and would otherwise
                // mask the accent color via patch).
                other => base.patch(markdown::ratatui_style(other, accent)),
            };
            Span::styled(span.display, style)
        })
        .collect()
}

pub(crate) fn push_tool_lines(lines: &mut Vec<Line<'static>>, text: &str, palette: &ThemePalette) {
    if !text.starts_with("running ")
        && !text.starts_with("completed ")
        && !text.starts_with("failed ")
    {
        push_marked_lines(
            lines,
            "tool",
            text,
            palette.secondary(),
            palette.secondary(),
        );
        return;
    }
    let mut parts = text.splitn(3, '\n');
    let state = parts.next().unwrap_or_default();
    let detail = parts.next().unwrap_or_default();
    let output = parts.next().unwrap_or_default();
    let (phase, name) = state.split_once(' ').unwrap_or((state, "tool"));
    let name = name.trim_start_matches('/');
    let failed = phase == "failed";
    let running = phase == "running";
    let highlight = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled(
        if failed { "✗ " } else { "● " },
        if failed {
            Style::default().fg(palette.error)
        } else if running {
            palette.secondary()
        } else {
            Style::default().fg(palette.success)
        },
    )];
    if name == "bash" {
        spans.push(Span::styled(
            if failed {
                "Command failed"
            } else if running {
                "Running a command"
            } else {
                "Ran a command"
            },
            highlight,
        ));
    } else {
        let verb = if failed {
            "Failed "
        } else if running {
            "Using "
        } else {
            "Used "
        };
        spans.push(Span::raw(verb));
        let label = match name {
            "read" => "Read",
            "edit" => "Edit",
            "write" => "Write",
            "grep" => "Grep",
            "find" => "Find",
            "ls" => "Ls",
            "todo" => "Todo",
            "task" => "Task",
            "web_search" => "Web Search",
            "web_fetch" => "Web Fetch",
            _ => name,
        };
        spans.push(Span::styled(label.to_string(), highlight));
    }
    // A subagent reads Claude's shape, `Used researcher(Count files)`,
    // not `Used Task · researcher(...)`: the agent name is its label.
    if name == "task" && !detail.is_empty() {
        spans.push(Span::styled(format!(" {detail}"), highlight));
    } else if !detail.is_empty() {
        spans.push(Span::styled(format!(" · {detail}"), palette.secondary()));
    }
    if name == "read" && !running && !output.is_empty() {
        spans.push(Span::styled(
            format!(" · {} lines", output.lines().count()),
            palette.secondary(),
        ));
    }
    lines.push(Line::from(spans));
    for line in output.lines() {
        lines.push(Line::from(Span::styled(
            format!("  {line}"),
            palette.secondary(),
        )));
    }
    if output.is_empty() && !running {
        lines.push(Line::from(Span::styled(
            "  (no output)",
            palette.secondary(),
        )));
    }
}

pub(crate) fn input_block(palette: &ThemePalette) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(palette.secondary().fg(palette.border))
        .padding(Padding::horizontal(1))
        .style(Style::default().fg(palette.foreground).bg(palette.surface))
}

pub(crate) fn draw_input(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, palette: &ThemePalette) {
    let block = input_block(palette);
    let inner = block.inner(area);
    let input_style = Style::default().fg(palette.foreground).bg(palette.surface);
    let prompt_style = Style::default().fg(palette.accent).bg(palette.surface);
    let lines = render_input_lines(app, inner, input_style, prompt_style);
    frame.render_widget(Paragraph::new(lines).style(input_style).block(block), area);
}

pub(crate) fn input_prompt(app: &TuiApp) -> &'static str {
    if app.multiline().is_some() {
        "multi> "
    } else {
        "> "
    }
}

pub(crate) fn render_input_lines(
    app: &TuiApp,
    area: Rect,
    input_style: Style,
    prompt_style: Style,
) -> Vec<Line<'static>> {
    if area.height == 0 || area.width == 0 {
        return Vec::new();
    }
    let prompt = input_prompt(app);
    let layout = input_layout(app, area.width as usize);
    let visible_rows = input_visible_rows(app, area.height as usize);
    let start = layout.visible_start(visible_rows);
    let mut lines = layout
        .rows
        .into_iter()
        .skip(start)
        .take(visible_rows)
        .enumerate()
        .map(|(offset, text)| {
            let prefix = if offset == 0 {
                prompt.to_string()
            } else {
                " ".repeat(prompt.len())
            };
            Line::from(vec![
                Span::styled(clip_width(&prefix, area.width as usize), prompt_style),
                Span::styled(text, input_style),
            ])
        })
        .collect::<Vec<_>>();
    if !app.input_media.is_empty() && lines.len() < area.height as usize {
        lines.push(Line::from(Span::styled(
            clip_width(
                &format!(
                    "[{} image{} attached · esc clears draft when idle]",
                    app.input_media.len(),
                    if app.input_media.len() == 1 { "" } else { "s" }
                ),
                area.width as usize,
            ),
            prompt_style,
        )));
    }
    if app.multiline().is_some() && lines.len() < area.height as usize {
        lines.push(Line::from(Span::styled(
            clip_width("submit with . on its own line", area.width as usize),
            prompt_style,
        )));
    }
    lines
}

pub(crate) struct InputLayout {
    pub(crate) rows: Vec<String>,
    pub(crate) cursor_row: usize,
    pub(crate) cursor_column: usize,
}

impl InputLayout {
    pub(crate) fn visible_start(&self, capacity: usize) -> usize {
        self.rows
            .len()
            .saturating_sub(capacity)
            .min(self.cursor_row)
    }
}

/// The editor's height, rendering, and cursor use the same grapheme/cell wrap.
/// Only the display copy folds pastes; the underlying draft stays untouched.
pub(crate) fn input_layout(app: &TuiApp, inner_width: usize) -> InputLayout {
    let (display, cursor) = app.display_input();
    let width = inner_width.saturating_sub(input_prompt(app).len()).max(1);
    let mut layout = InputLayout {
        rows: Vec::new(),
        cursor_row: 0,
        cursor_column: 0,
    };
    let mut source_offset = 0;
    for part in display.split('\n') {
        let mut row = String::new();
        let mut column = 0;
        for (offset, grapheme) in part.grapheme_indices(true) {
            let text = if grapheme == "\t" { "    " } else { grapheme };
            let cells = text.width();
            if column > 0 && column + cells > width {
                layout.rows.push(std::mem::take(&mut row));
                column = 0;
            }
            if source_offset + offset == cursor {
                layout.cursor_row = layout.rows.len();
                layout.cursor_column = column;
            }
            row.push_str(text);
            column += cells;
        }
        // A full final row needs an insertion cell on the next row.
        if column >= width
            && (source_offset + part.len() == cursor || source_offset + part.len() == display.len())
        {
            layout.rows.push(std::mem::take(&mut row));
            column = 0;
        }
        if source_offset + part.len() == cursor {
            layout.cursor_row = layout.rows.len();
            layout.cursor_column = column;
        }
        layout.rows.push(row);
        source_offset += part.len() + 1;
    }
    layout
}

pub(crate) fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn clip_width(text: &str, width: usize) -> String {
    let mut used = 0;
    text.graphemes(true)
        .take_while(|grapheme| {
            used += grapheme.width();
            used <= width
        })
        .collect()
}

pub(crate) fn truncate_width(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", clip_width(text, width - 1))
}

pub(crate) fn clip_line_width(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    let mut remaining = width.saturating_sub(1);
    let mut spans = Vec::new();
    for span in line.spans {
        let clipped = span.width() > remaining;
        let content = clip_width(&span.content, remaining);
        remaining = remaining.saturating_sub(content.width());
        spans.push(Span::styled(content, span.style));
        if clipped {
            break;
        }
    }
    if width > 0 {
        spans.push(Span::styled("…", line.style));
    }
    Line::from(spans).style(line.style)
}

pub(crate) fn input_visible_rows(app: &TuiApp, area_height: usize) -> usize {
    if area_height == 0 {
        0
    } else {
        area_height
            .saturating_sub(
                usize::from(app.multiline().is_some()) + usize::from(!app.input_media.is_empty()),
            )
            .max(1)
    }
}

pub(crate) fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &TuiApp, palette: &ThemePalette) {
    let status = if app.owned_screen && app.transcript_view.borrow().has_selection() {
        app.status
            .replace("esc to interrupt", "ctrl+c copies selection; esc clears")
    } else if app.owned_screen && app.transcript_view.borrow().is_reading() {
        app.status
            .replace("esc to interrupt", "ctrl+c to interrupt")
    } else {
        app.status.clone()
    };
    let line = match &app.workspace {
        Some(record) => format!(
            "{}  {}  {}",
            status,
            workspace::footer_label(record),
            app.header_line
        ),
        None if app.header_line.is_empty() => status,
        None => format!("{}  {}", status, app.header_line),
    };
    let footer = Paragraph::new(line).style(palette.secondary());
    frame.render_widget(footer, area);
}

pub(crate) const SLASH_MATCH_LIMIT: usize = 6;
pub(crate) const SELECTOR_PAGE_STEP: usize = 10;

pub(crate) fn slash_command_matches(config: &LoadedConfig, app: &TuiApp) -> Vec<String> {
    if app.selector().is_some() || app.multiline().is_some() || app.input.contains('\n') {
        return Vec::new();
    }
    let input = app.input.trim();
    if !input.starts_with('/') {
        return Vec::new();
    }
    command_completions(config, input)
        .into_iter()
        .take(SLASH_MATCH_LIMIT)
        .collect()
}

pub(crate) fn draw_slash_matches(
    frame: &mut Frame<'_>,
    area: Rect,
    matches: &[String],
    palette: &ThemePalette,
) {
    if area.height == 0 || matches.is_empty() {
        return;
    }
    let lines = matches
        .iter()
        .take(area.height as usize)
        .map(|command| {
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(command.clone(), palette.secondary()),
            ])
        })
        .collect::<Vec<_>>();
    let paragraph = Paragraph::new(lines).style(palette.secondary());
    frame.render_widget(paragraph, area);
}

pub(crate) fn draw_selector_overlay(frame: &mut Frame<'_>, app: &TuiApp, palette: &ThemePalette) {
    let Some(selector) = app.selector() else {
        return;
    };
    let area = centered_rect(frame.area(), 82, 68);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(palette.base().bg(palette.surface)),
        area,
    );
    let available = area.height.saturating_sub(5) as usize;
    let start = selector
        .selected
        .saturating_sub(available.saturating_sub(1));
    let mut lines = vec![
        Line::from(vec![
            Span::styled("filter ", palette.secondary()),
            Span::raw(selector.query.as_str()),
        ]),
        Line::from(""),
    ];
    for (position, item_index) in selector
        .filtered_indices
        .iter()
        .enumerate()
        .skip(start)
        .take(available)
    {
        let item = &selector.items[*item_index];
        let marker = if position == selector.selected {
            ">"
        } else {
            " "
        };
        let active = if item.active { "*" } else { " " };
        let style = if position == selector.selected {
            palette.selected()
        } else if item.active {
            Style::default().fg(palette.accent)
        } else {
            Style::default()
        };
        let prefix = format!("{:>2}. {marker}{active} ", position + 1);
        let text_width = area
            .width
            .saturating_sub(4)
            .saturating_sub(prefix.chars().count() as u16) as usize;
        lines.push(Line::from(Span::styled(
            format!("{prefix}{}", truncate_chars(&item.label, text_width)),
            style,
        )));
    }
    if selector.filtered_indices.is_empty() {
        lines.push(Line::from(Span::styled("no matches", palette.secondary())));
    }
    if let Some(level) = selector.selected_thinking_level() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("thinking ", palette.secondary()),
            Span::styled(level, Style::default().fg(palette.accent)),
            Span::styled("  left/right to adjust", palette.secondary()),
        ]));
    }
    if !selector.tree.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "ctrl+left/right fold · ctrl+l named only · ctrl+o cycle · shift+L label",
            palette.secondary(),
        )));
    }
    let action = if selector.kind == "settings" {
        "enter toggle"
    } else {
        "enter select"
    };
    let total = selector.filtered_indices.len();
    let position = if total == 0 { 0 } else { selector.selected + 1 };
    let title = format!(
        " {} selector  {position}/{total}  {action}  pgup/pgdn home/end  esc cancel ",
        selector.title
    );
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(palette.border))
            .title(title),
    );
    frame.render_widget(paragraph, area);
}

pub(crate) fn set_tui_cursor(frame: &mut Frame<'_>, input_area: Rect, app: &TuiApp) {
    if !app.show_hardware_cursor {
        return;
    }
    if let Some(selector) = app.selector() {
        let area = centered_rect(frame.area(), 82, 68);
        let filter_prefix_width = 7;
        let max_x = area.right().saturating_sub(2);
        let x = area
            .x
            .saturating_add(1)
            .saturating_add(filter_prefix_width)
            .saturating_add(selector.query.chars().count() as u16)
            .min(max_x);
        let y = area
            .y
            .saturating_add(1)
            .min(area.bottom().saturating_sub(2));
        frame.set_cursor_position(Position::new(x, y));
        return;
    }
    let inner = input_block(&ThemePalette::default()).inner(input_area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let prompt_width = input_prompt(app).len() as u16;
    let layout = input_layout(app, inner.width as usize);
    let start = layout.visible_start(input_visible_rows(app, inner.height as usize));
    let x = inner
        .x
        .saturating_add(prompt_width)
        .saturating_add(layout.cursor_column.min(u16::MAX as usize) as u16)
        .min(inner.right().saturating_sub(1));
    let y = inner
        .y
        .saturating_add(
            layout
                .cursor_row
                .saturating_sub(start)
                .min(u16::MAX as usize) as u16,
        )
        .min(inner.bottom().saturating_sub(1));
    frame.set_cursor_position(Position::new(x, y));
}

pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut output = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() && max_chars > 0 {
        output.pop();
        output.push('~');
    }
    output
}

pub(crate) fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

pub(crate) fn handle_transcript_key(key: &KeyEvent, app: &mut TuiApp) -> bool {
    if !app.owned_screen {
        return false;
    }
    let view = app.transcript_view.get_mut();
    match key.code {
        KeyCode::Char('c')
            if key.modifiers.contains(KeyModifiers::CONTROL) && view.has_selection() =>
        {
            view.request_copy()
        }
        KeyCode::Esc if view.is_reading() => {
            view.clear_selection();
            view.jump_to_latest();
        }
        KeyCode::Esc if view.clear_selection() => {}
        KeyCode::PageUp => view.page(false),
        KeyCode::PageDown => view.page(true),
        _ => return false,
    }
    true
}

pub(crate) fn handle_transcript_mouse(mouse: MouseEvent, app: &mut TuiApp) -> bool {
    if !app.owned_screen || app.selector().is_some() {
        return false;
    }
    let view = app.transcript_view.get_mut();
    let position = Position::new(mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollUp if view.area.contains(position) => view.scroll(-3),
        MouseEventKind::ScrollDown if view.area.contains(position) => view.scroll(3),
        MouseEventKind::Down(MouseButton::Left)
            if view.is_reading() && view.hint_area.contains(position) =>
        {
            view.clear_selection();
            view.jump_to_latest()
        }
        MouseEventKind::Down(MouseButton::Left) if view.area.contains(position) => {
            return view.begin_selection(position);
        }
        MouseEventKind::Drag(MouseButton::Left) => return view.drag_selection(position),
        MouseEventKind::Up(MouseButton::Left) => {
            if view.area.contains(position) {
                view.drag_selection(position);
            }
            view.finish_selection();
        }
        MouseEventKind::Down(_) => return view.clear_selection(),
        _ => return false,
    }
    true
}

/// Opens the external editor seeded with the current draft and replaces the
/// input with the edited content (ctrl+g).
pub(crate) fn edit_input_in_external_editor(app: &mut TuiApp) {
    match read_external_editor_prompt(&app.input) {
        Ok(content) => {
            let media = std::mem::take(&mut app.input_media);
            app.clear_input();
            app.input_media = media;
            app.insert_input_str(content.trim_end_matches('\n'));
        }
        Err(error) => app.push(TuiEntryKind::Error, format!("{error}")),
    }
}

pub(crate) const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);

pub(crate) fn key_event_name(key: &KeyEvent) -> Option<String> {
    let base = match key.code {
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "escape".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        _ => return None,
    };
    let mut name = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        name.push_str("ctrl+");
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        name.push_str("super+");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        name.push_str("alt+");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        name.push_str("shift+");
    }
    name.push_str(&base);
    Some(name)
}
