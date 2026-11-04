//! The window: sessions on the left, the selected session's transcript and
//! the composer in the middle, the plugins' commands and panels on the
//! right, and a command palette over them (secondary-k).

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use gpui::{
    actions, colors::Colors, div, prelude::*, px, size, AnyElement, App, Bounds, Context, Entity,
    FocusHandle, FontWeight, KeyBinding, QuitMode, SharedString, Window, WindowBounds,
    WindowOptions,
};
use gpui_elements::editable_text::{
    actions::{default_bindings, DEFAULT_INPUT_CONTEXT},
    text_area, text_input, EditableTextState, StringStorage,
};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::backend::{Command, Endpoint, Event};

const CONTEXT: &str = "NoelApp";

actions!(
    noel_app,
    [
        SendMessage,
        TogglePalette,
        NewSession,
        AbortRun,
        CloseOverlay
    ]
);

pub fn run(
    events: UnboundedReceiver<Event>,
    commands: UnboundedSender<Command>,
    endpoint: Endpoint,
) {
    gpui_platform::application()
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx: &mut App| {
            cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
            cx.bind_keys([
                KeyBinding::new("secondary-enter", SendMessage, Some(CONTEXT)),
                KeyBinding::new("secondary-k", TogglePalette, Some(CONTEXT)),
                KeyBinding::new("secondary-n", NewSession, Some(CONTEXT)),
                KeyBinding::new("secondary-.", AbortRun, Some(CONTEXT)),
                KeyBinding::new("escape", CloseOverlay, Some(CONTEXT)),
            ]);
            let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
            cx.open_window(
                WindowOptions::default().window_bounds(WindowBounds::Windowed(bounds)),
                |window, cx| cx.new(|cx| NoelApp::new(events, commands, endpoint, window, cx)),
            )
            .expect("a window");
            cx.activate(true);
        });
}

/// One thing the palette can do.
#[derive(Clone)]
enum PaletteItem {
    NewSession,
    Abort,
    Open(String, String),
    PluginRun { plugin: String, name: String },
}

impl PaletteItem {
    fn label(&self) -> String {
        match self {
            PaletteItem::NewSession => "New session".to_owned(),
            PaletteItem::Abort => "Abort the run".to_owned(),
            PaletteItem::Open(_, title) => format!("Open {title}"),
            PaletteItem::PluginRun { plugin, name } => format!("Plugin {plugin}: {name}"),
        }
    }
}

struct NoelApp {
    commands: UnboundedSender<Command>,
    endpoint: Endpoint,
    connected: bool,
    status: String,
    sessions: Vec<Value>,
    selected: Option<String>,
    view: Value,
    entries: Vec<Value>,
    plugins: Vec<Value>,
    composer: Entity<EditableTextState>,
    palette_input: Entity<EditableTextState>,
    palette_open: bool,
    focus: FocusHandle,
}

impl NoelApp {
    fn new(
        mut events: UnboundedReceiver<Event>,
        commands: UnboundedSender<Command>,
        endpoint: Endpoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let applied = this.update(cx, |app, cx| {
                    app.apply(event);
                    cx.notify();
                });
                if applied.is_err() {
                    break;
                }
            }
        })
        .detach();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        NoelApp {
            commands,
            endpoint,
            connected: false,
            status: "connecting".to_owned(),
            sessions: Vec::new(),
            selected: None,
            view: Value::Null,
            entries: Vec::new(),
            plugins: Vec::new(),
            composer: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            palette_input: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            palette_open: false,
            focus,
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Connected => {
                self.connected = true;
                self.status = "connected".to_owned();
            }
            Event::Status(status) => self.status = status,
            Event::Sessions(sessions) => self.sessions = sessions,
            Event::View {
                session,
                view,
                entries,
            } => {
                self.selected = Some(session);
                self.view = view;
                self.entries = entries;
            }
            Event::Plugins(plugins) => self.plugins = plugins,
        }
    }

    fn send(&mut self, command: Command) {
        if self.commands.send(command).is_err() {
            self.status = "the link to the server ended".to_owned();
        }
    }

    fn select(&mut self, session: String) {
        self.selected = Some(session.clone());
        self.entries.clear();
        self.view = Value::Null;
        self.send(Command::Select(session));
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).as_str().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.composer.update(cx, |state, cx| state.emplace("", cx));
        self.send(Command::Send(text));
    }

    fn new_session(&mut self, cx: &mut Context<Self>) {
        // A path typed in the palette, else this directory (locally).
        let typed = self.palette_input.read(cx).as_str().trim().to_owned();
        let cwd = if typed.starts_with('/') {
            typed
        } else if self.endpoint.remote.is_none() {
            std::env::current_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|_| "/".to_owned())
        } else {
            self.status = "type the remote directory in the palette, then New session".to_owned();
            self.palette_open = true;
            return;
        };
        self.send(Command::New { cwd });
    }

    fn palette_items(&self, cx: &App) -> Vec<PaletteItem> {
        let filter = self.palette_input.read(cx).as_str().trim().to_lowercase();
        let mut items = vec![PaletteItem::NewSession, PaletteItem::Abort];
        for session in &self.sessions {
            let id = session["sessionId"].as_str().unwrap_or_default().to_owned();
            items.push(PaletteItem::Open(id.clone(), session_title(session)));
        }
        for plugin in &self.plugins {
            let name = plugin["name"].as_str().unwrap_or_default();
            for command in plugin["contributions"]["commands"]
                .as_array()
                .into_iter()
                .flatten()
            {
                items.push(PaletteItem::PluginRun {
                    plugin: name.to_owned(),
                    name: command["name"].as_str().unwrap_or_default().to_owned(),
                });
            }
        }
        items
            .into_iter()
            .filter(|item| {
                filter.is_empty()
                    || filter.starts_with('/')
                    || item.label().to_lowercase().contains(&filter)
            })
            .collect()
    }

    fn run_item(&mut self, item: PaletteItem, cx: &mut Context<Self>) {
        match item {
            PaletteItem::NewSession => self.new_session(cx),
            PaletteItem::Abort => self.send(Command::Abort),
            PaletteItem::Open(id, _) => self.select(id),
            PaletteItem::PluginRun { plugin, name } => {
                self.send(Command::PluginRun { plugin, name })
            }
        }
        self.palette_open = false;
        self.palette_input
            .update(cx, |state, cx| state.emplace("", cx));
    }

    fn sidebar(&self, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div()
            .id("sessions")
            .flex()
            .flex_col()
            .gap_1()
            .overflow_y_scroll()
            .flex_1();
        for session in &self.sessions {
            let id = session["sessionId"].as_str().unwrap_or_default().to_owned();
            let selected = self.selected.as_deref() == Some(id.as_str());
            let busy = session["live"]["busy"] == Value::Bool(true);
            let clicked = id.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("session-{id}")))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |row| {
                        row.bg(colors.selected).text_color(colors.selected_text)
                    })
                    .hover(|row| row.opacity(0.85))
                    .child(div().text_sm().child(session_title(session)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(if selected {
                                colors.selected_text
                            } else {
                                colors.disabled
                            })
                            .child(format!(
                                "{}{}",
                                session["messageCount"].as_u64().unwrap_or(0),
                                if busy {
                                    " messages, running"
                                } else {
                                    " messages"
                                }
                            )),
                    )
                    .on_click(cx.listener(move |app, _, _, _| app.select(clicked.clone()))),
            );
        }
        div()
            .w(px(260.))
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .border_r_1()
            .border_color(colors.border)
            .bg(colors.container)
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Sessions"))
            .child(
                button("new-session", "New session", colors)
                    .on_click(cx.listener(|app, _, _, cx| app.new_session(cx))),
            )
            .child(list)
    }

    fn transcript(&self, colors: &Colors) -> impl IntoElement {
        let mut column = div()
            .id("transcript")
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .flex_1()
            .overflow_y_scroll();
        if self.selected.is_none() {
            column = column.child(
                div()
                    .text_color(colors.disabled)
                    .child("Pick a session, or make a new one (secondary-n)."),
            );
        }
        for entry in &self.entries {
            column = column.child(entry_element(entry, colors));
        }
        let live = &self.view["docs"]["pi.live"];
        if let Some(partial) = live["generation"]["message"]["content"].as_array() {
            let text = text_of_parts(partial);
            if !text.is_empty() {
                column = column.child(block("assistant (streaming)", &text, colors, false));
            }
        }
        if let Some(tools) = live["tools"].as_array() {
            for tool in tools {
                column = column.child(div().text_xs().text_color(colors.disabled).child(format!(
                    "{} {}",
                    tool["name"].as_str().unwrap_or_default(),
                    tool["status"].as_str().unwrap_or_default()
                )));
            }
        }
        column
    }

    fn composer(&self, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .gap_2()
            .p_2()
            .border_t_1()
            .border_color(colors.border)
            .child(
                text_area("composer")
                    .state(self.composer.downgrade())
                    .placeholder("Message (secondary-enter sends)")
                    .placeholder_color(colors.disabled)
                    .caret_color(gpui::rgb_to_hsla(colors.text))
                    .flex_1()
                    .h(px(72.))
                    .whitespace_normal()
                    .overflow_y_scroll()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.container),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        button("send", "Send", colors)
                            .on_click(cx.listener(|app, _, _, cx| app.submit(cx))),
                    )
                    .child(
                        button("abort", "Abort", colors)
                            .on_click(cx.listener(|app, _, _, _| app.send(Command::Abort))),
                    ),
            )
    }

    fn plugins_column(&self, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mut column = div()
            .id("plugins")
            .w(px(280.))
            .flex()
            .flex_col()
            .gap_3()
            .p_2()
            .border_l_1()
            .border_color(colors.border)
            .bg(colors.container)
            .overflow_y_scroll()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Plugins"));
        if self.plugins.is_empty() {
            column = column.child(
                div()
                    .text_sm()
                    .text_color(colors.disabled)
                    .child("No plugins."),
            );
        }
        for plugin in &self.plugins {
            let name = plugin["name"].as_str().unwrap_or_default().to_owned();
            let state = if plugin["running"] == Value::Bool(true) {
                "running"
            } else {
                "stopped"
            };
            let mut card = div().flex().flex_col().gap_1().child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{name} ({state})")),
            );
            for command in plugin["contributions"]["commands"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let command = command["name"].as_str().unwrap_or_default().to_owned();
                let (plugin_name, command_name) = (name.clone(), command.clone());
                card = card.child(
                    button(format!("command-{name}-{command}"), &command, colors).on_click(
                        cx.listener(move |app, _, _, _| {
                            app.send(Command::PluginRun {
                                plugin: plugin_name.clone(),
                                name: command_name.clone(),
                            })
                        }),
                    ),
                );
            }
            for panel in plugin["contributions"]["panels"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let id = panel["id"].as_str().unwrap_or_default().to_owned();
                card = card
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.disabled)
                            .child(panel["title"].as_str().unwrap_or_default().to_owned()),
                    )
                    .child(panel_element(&panel["view"], &name, &id, colors, cx));
            }
            column = column.child(card);
        }
        column
    }

    fn palette(&self, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().flex().flex_col().gap_1();
        for (index, item) in self.palette_items(cx).into_iter().enumerate().take(20) {
            let label = item.label();
            list = list.child(
                div()
                    .id(SharedString::from(format!("palette-{index}")))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|row| row.bg(colors.selected).text_color(colors.selected_text))
                    .child(label)
                    .on_click(cx.listener(move |app, _, _, cx| app.run_item(item.clone(), cx))),
            );
        }
        div()
            .absolute()
            .top(px(60.))
            .left(px(340.))
            .w(px(600.))
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(colors.border)
            .bg(colors.container)
            .child(
                text_input("palette")
                    .state(self.palette_input.downgrade())
                    .placeholder("Filter, or a directory for New session")
                    .placeholder_color(colors.disabled)
                    .caret_color(gpui::rgb_to_hsla(colors.text))
                    .w_full()
                    .whitespace_nowrap()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border),
            )
            .child(list)
    }
}

impl Render for NoelApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::for_appearance(window);
        let title = match &self.endpoint.remote {
            Some(remote) => format!("noel on {remote}"),
            None => "noel".to_owned(),
        };
        let header = div()
            .flex()
            .justify_between()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(colors.border)
            .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(
                div()
                    .text_sm()
                    .text_color(if self.connected {
                        colors.text
                    } else {
                        colors.disabled
                    })
                    .child(self.status.clone()),
            );
        let main = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .child(self.transcript(&colors))
            .child(self.composer(&colors, cx));
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|app, _: &SendMessage, _, cx| app.submit(cx)))
            .on_action(cx.listener(|app, _: &TogglePalette, _, cx| {
                app.palette_open = !app.palette_open;
                cx.notify();
            }))
            .on_action(cx.listener(|app, _: &NewSession, _, cx| app.new_session(cx)))
            .on_action(cx.listener(|app, _: &AbortRun, _, _| app.send(Command::Abort)))
            .on_action(cx.listener(|app, _: &CloseOverlay, _, cx| {
                app.palette_open = false;
                cx.notify();
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .text_color(colors.text)
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.sidebar(&colors, cx))
                    .child(main)
                    .child(self.plugins_column(&colors, cx)),
            )
            .when(self.palette_open, |root| {
                root.child(self.palette(&colors, cx))
            })
    }
}

fn button(id: impl Into<SharedString>, label: &str, colors: &Colors) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .px_3()
        .py_1()
        .rounded_md()
        .bg(colors.selected)
        .text_color(colors.selected_text)
        .text_sm()
        .cursor_pointer()
        .hover(|style| style.opacity(0.9))
        .active(|style| style.opacity(0.8))
        .child(label.to_owned())
}

fn session_title(session: &Value) -> String {
    session["name"]
        .as_str()
        .or(session["firstPrompt"].as_str())
        .map(|text| text.chars().take(40).collect())
        .unwrap_or_else(|| session["sessionId"].as_str().unwrap_or_default().to_owned())
}

fn text_of_parts(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|part| match part["type"].as_str() {
            Some("text") => part["text"].as_str().map(str::to_owned),
            Some("toolCall") => Some(format!(
                "-> {}({})",
                part["name"].as_str().unwrap_or_default(),
                part["arguments"]
            )),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An entry of the history (Durable's entry shape).
fn entry_element(entry: &Value, colors: &Colors) -> AnyElement {
    let message = &entry["model"][0];
    let text = match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => text_of_parts(parts),
        _ => String::new(),
    };
    match entry["kind"].as_str() {
        Some("pi.user") => block("you", &text, colors, false),
        Some("pi.assistant") => block("assistant", &text, colors, false),
        Some("pi.tool-result") => {
            let label = format!(
                "{}{}",
                message["toolName"].as_str().unwrap_or("tool"),
                if message["isError"] == Value::Bool(true) {
                    " (error)"
                } else {
                    ""
                }
            );
            block(&label, &text, colors, true)
        }
        Some("pi.compaction") => div()
            .text_xs()
            .text_color(colors.disabled)
            .child(format!(
                "compacted: {}",
                entry["data"]["summary"].as_str().unwrap_or_default()
            ))
            .into_any_element(),
        Some(other) => div()
            .text_xs()
            .text_color(colors.disabled)
            .child(other.to_owned())
            .into_any_element(),
        None => div().into_any_element(),
    }
}

/// A labelled block of text; a tool's shows diff lines in color.
fn block(label: &str, text: &str, colors: &Colors, tool: bool) -> AnyElement {
    let mut body = div().flex().flex_col().text_sm();
    for line in text.lines() {
        let color = if tool && line.starts_with('+') && !line.starts_with("+++") {
            Some(gpui::rgb(0x3fa65a))
        } else if tool && line.starts_with('-') && !line.starts_with("---") {
            Some(gpui::rgb(0xcc4d4d))
        } else {
            None
        };
        let row = div().child(if line.is_empty() {
            " ".to_owned()
        } else {
            line.to_owned()
        });
        body = body.child(match color {
            Some(color) => row.text_color(color),
            None => row,
        });
    }
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded_md()
        .when(tool, |block| {
            block.bg(colors.container).font_family("monospace")
        })
        .child(
            div()
                .text_xs()
                .text_color(colors.disabled)
                .child(label.to_owned()),
        )
        .child(body)
        .into_any_element()
}

/// A plugin panel's view, in the widget vocabulary of spec §13.
fn panel_element(
    view: &Value,
    plugin: &str,
    panel: &str,
    colors: &Colors,
    cx: &mut Context<NoelApp>,
) -> AnyElement {
    match view["type"].as_str() {
        Some("text") => div()
            .text_sm()
            .child(view["text"].as_str().unwrap_or_default().to_owned())
            .into_any_element(),
        Some("button") => {
            let action = view["action"].as_str().unwrap_or_default().to_owned();
            let (plugin, panel) = (plugin.to_owned(), panel.to_owned());
            button(
                format!("panel-{plugin}-{panel}-{action}"),
                view["label"].as_str().unwrap_or_default(),
                colors,
            )
            .on_click(cx.listener(move |app, _, _, _| {
                app.send(Command::PanelAction {
                    plugin: plugin.clone(),
                    panel: panel.clone(),
                    action: action.clone(),
                })
            }))
            .into_any_element()
        }
        Some("list") => {
            let mut list = div().flex().flex_col().gap_1();
            for item in view["items"].as_array().into_iter().flatten() {
                list = list.child(
                    div()
                        .flex()
                        .gap_1()
                        .child("•")
                        .child(panel_element(item, plugin, panel, colors, cx)),
                );
            }
            list.into_any_element()
        }
        Some("stack") => {
            let mut stack = div().flex().flex_col().gap_1();
            for child in view["children"].as_array().into_iter().flatten() {
                stack = stack.child(panel_element(child, plugin, panel, colors, cx));
            }
            stack.into_any_element()
        }
        _ => div()
            .text_xs()
            .text_color(colors.disabled)
            .child(view.to_string())
            .into_any_element(),
    }
}
