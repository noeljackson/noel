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
use noel_app_ui::{block, button, entry_element, panel_element, session_title, text_of_parts};
use serde_json::Value;
use std::collections::BTreeSet;
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
        CloseOverlay,
        QuitApp
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
                KeyBinding::new("secondary-q", QuitApp, Some(CONTEXT)),
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
    Directory(String),
    Abort,
    Open(String, String),
    PluginRun { plugin: String, name: String },
}

impl PaletteItem {
    fn label(&self) -> String {
        match self {
            PaletteItem::NewSession => "New session".to_owned(),
            PaletteItem::Directory(path) => path.clone(),
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
    session_search: Entity<EditableTextState>,
    palette_input: Entity<EditableTextState>,
    palette_open: bool,
    plugins_open: bool,
    hovered_session: Option<String>,
    menu_session: Option<String>,
    confirm_delete: Option<String>,
    archived_sessions: BTreeSet<String>,
    show_archived: bool,
    server_menu_open: bool,
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
            session_search: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            palette_input: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            palette_open: false,
            plugins_open: false,
            hovered_session: None,
            menu_session: None,
            confirm_delete: None,
            archived_sessions: BTreeSet::new(),
            show_archived: false,
            server_menu_open: false,
            focus,
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Connected => {
                self.connected = true;
                self.status = "connected".to_owned();
            }
            Event::Reconnecting => {
                self.connected = false;
                self.status = "reconnecting".to_owned();
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

    fn archive_session(&mut self, session: String) {
        self.archived_sessions.insert(session.clone());
        self.menu_session = None;
        if self.selected.as_deref() == Some(session.as_str()) {
            self.selected = None;
            self.view = Value::Null;
            self.entries.clear();
        }
    }

    fn delete_session(&mut self, session: String) {
        self.confirm_delete = None;
        self.menu_session = None;
        if self.selected.as_deref() == Some(session.as_str()) {
            self.selected = None;
            self.view = Value::Null;
            self.entries.clear();
        }
        self.send(Command::Delete(session));
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).as_str().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.composer.update(cx, |state, cx| state.emplace("", cx));
        self.send(Command::Send(text));
    }

    fn open_new_session_picker(&mut self, cx: &mut Context<Self>) {
        let cwd = if self.endpoint.remote.is_none() {
            std::env::current_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|_| "/".to_owned())
        } else {
            recent_directories(&self.sessions)
                .into_iter()
                .next()
                .unwrap_or_else(|| "/".to_owned())
        };
        self.palette_input
            .update(cx, |state, cx| state.emplace(&cwd, cx));
        self.palette_open = true;
    }

    fn new_session(&mut self, cx: &mut Context<Self>) {
        let typed = self.palette_input.read(cx).as_str().trim().to_owned();
        let cwd = if typed.starts_with('/') {
            typed
        } else if self.endpoint.remote.is_none() {
            std::env::current_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|_| "/".to_owned())
        } else {
            recent_directories(&self.sessions)
                .into_iter()
                .next()
                .unwrap_or_else(|| "/".to_owned())
        };
        self.send(Command::New { cwd });
    }

    fn palette_items(&self, cx: &App) -> Vec<PaletteItem> {
        let filter = self.palette_input.read(cx).as_str().trim().to_lowercase();
        let mut items = vec![PaletteItem::NewSession];
        items.extend(
            recent_directories(&self.sessions)
                .into_iter()
                .map(PaletteItem::Directory),
        );
        items.push(PaletteItem::Abort);
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
            PaletteItem::Directory(cwd) => self.send(Command::New { cwd }),
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
        let query = self.session_search.read(cx).as_str().trim().to_lowercase();
        let filtered: Vec<Value> = self
            .sessions
            .iter()
            .filter(|session| {
                let id = session["sessionId"].as_str().unwrap_or_default();
                (self.show_archived || !self.archived_sessions.contains(id))
                    && (query.is_empty() || session_title(session).to_lowercase().contains(&query))
            })
            .cloned()
            .collect();
        let mut list = div()
            .id("sessions")
            .flex()
            .flex_col()
            .gap_1()
            .overflow_y_scroll()
            .flex_1();
        for group in noel_app_ui::sessions::group_sessions(&filtered) {
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pt_3()
                    .px_2()
                    .child(
                        div()
                            .w(px(12.))
                            .h(px(10.))
                            .rounded_sm()
                            .border_1()
                            .border_color(colors.disabled),
                    )
                    .child(div().text_sm().child(group.workspace)),
            );
            for session in group.sessions {
                let id = session["sessionId"].as_str().unwrap_or_default().to_owned();
                let selected = self.selected.as_deref() == Some(id.as_str());
                let clicked = id.clone();
                let hovered = self.hovered_session.as_deref() == Some(id.as_str());
                let menu_open = self.menu_session.as_deref() == Some(id.as_str());
                let archived = self.archived_sessions.contains(&id);
                list = list.child(
                    div()
                        .id(SharedString::from(format!("session-{id}")))
                        .relative()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |row| row.bg(colors.selected))
                        .hover(|row| row.opacity(0.85))
                        .on_hover(cx.listener({
                            let id = id.clone();
                            move |app, is_hovered, _, _| {
                                if *is_hovered {
                                    app.hovered_session = Some(id.clone());
                                } else if app.hovered_session.as_deref() == Some(id.as_str()) {
                                    app.hovered_session = None;
                                }
                            }
                        }))
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_ellipsis()
                                .child(session_title(&session)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(colors.disabled)
                                .child(if archived {
                                    "archived".to_owned()
                                } else {
                                    noel_app_ui::sessions::relative_time_label(
                                        session["modified"].as_u64().unwrap_or(0),
                                        std::time::SystemTime::now(),
                                    )
                                }),
                        )
                        .when(hovered || menu_open, |row| {
                            row.child(
                                button(SharedString::from(format!("options-{id}")), "⋯", colors)
                                    .on_click(cx.listener({
                                        let id = id.clone();
                                        move |app, _, _, cx| {
                                            app.menu_session = Some(id.clone());
                                            cx.notify();
                                        }
                                    })),
                            )
                        })
                        .when(menu_open, |row| {
                            row.child(
                                div()
                                    .absolute()
                                    .right_0()
                                    .top(px(28.))
                                    .w(px(160.))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .p_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(colors.border)
                                    .bg(colors.container)
                                    .child(button("archive-session", "Archive", colors).on_click(
                                        cx.listener({
                                            let id = id.clone();
                                            move |app, _, _, cx| {
                                                app.archive_session(id.clone());
                                                cx.notify();
                                            }
                                        }),
                                    ))
                                    .child(button("delete-session", "Delete", colors).on_click(
                                        cx.listener({
                                            let id = id.clone();
                                            move |app, _, _, cx| {
                                                app.confirm_delete = Some(id.clone());
                                                cx.notify();
                                            }
                                        }),
                                    ))
                                    .child(
                                        button("quit-app", "Quit", colors)
                                            .on_click(cx.listener(|_, _, _, cx| cx.quit())),
                                    ),
                            )
                        })
                        .on_click(cx.listener(move |app, _, _, _| app.select(clicked.clone()))),
                );
            }
        }
        div()
            .w(px(264.))
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .border_r_1()
            .border_color(colors.border)
            .bg(gpui::rgb(0x0d0d0d))
            .child(
                div()
                    .relative()
                    .h(px(48.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("noel"),
                    )
                    .child(
                        div()
                            .w(px(8.))
                            .h(px(8.))
                            .rounded_full()
                            .bg(if self.connected {
                                gpui::rgb(0x3fa65a)
                            } else {
                                colors.disabled
                            }),
                    )
                    .child(div().flex_1())
                    .child(button("server-options", "⋯", colors).on_click(cx.listener(
                        |app, _, _, cx| {
                            app.server_menu_open = !app.server_menu_open;
                            cx.notify();
                        },
                    )))
                    .when(self.server_menu_open, |row| {
                        row.child(
                            div()
                                .absolute()
                                .right_0()
                                .top(px(44.))
                                .w(px(190.))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .p_2()
                                .rounded_md()
                                .border_1()
                                .border_color(colors.border)
                                .bg(colors.container)
                                .child(
                                    button("toggle-archived", "Show / hide archived", colors)
                                        .on_click(cx.listener(|app, _, _, cx| {
                                            app.show_archived = !app.show_archived;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    button("quit-server", "Quit", colors)
                                        .on_click(cx.listener(|_, _, _, cx| cx.quit())),
                                ),
                        )
                    }),
            )
            .child(
                button("new-session", "New session", colors)
                    .h(px(30.))
                    .on_click(cx.listener(|app, _, _, cx| app.open_new_session_picker(cx))),
            )
            .child(
                text_input("session-search")
                    .state(self.session_search.downgrade())
                    .placeholder("Search")
                    .placeholder_color(colors.disabled)
                    .caret_color(gpui::rgb_to_hsla(colors.text))
                    .h(px(32.))
                    .whitespace_nowrap()
                    .px_2()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.container),
            )
            .child(
                div()
                    .pt_6()
                    .text_xs()
                    .text_color(colors.disabled)
                    .child("SESSIONS"),
            )
            .child(list)
    }

    fn transcript(&self, colors: &Colors) -> impl IntoElement {
        let mut column = div()
            .id("transcript")
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
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
        div()
            .flex()
            .justify_center()
            .flex_1()
            .min_h(px(0.))
            .child(column.max_w(px(760.)).w_full())
    }

    fn composer(&self, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().justify_center().p_3().child(
            div()
                .max_w(px(760.))
                .w_full()
                .flex()
                .gap_2()
                .p_2()
                .rounded_2xl()
                .border_1()
                .border_color(colors.border)
                .bg(colors.container)
                .child(
                    text_area("composer")
                        .state(self.composer.downgrade())
                        .placeholder("Type a message…")
                        .placeholder_color(colors.disabled)
                        .caret_color(gpui::rgb_to_hsla(colors.text))
                        .flex_1()
                        .h(px(84.))
                        .whitespace_normal()
                        .overflow_y_scroll()
                        .px_3()
                        .py_2(),
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
                ),
        )
    }

    fn plugins_column(&self, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div().id("plugins").flex().gap_3().p_3().overflow_x_scroll();
        if self.plugins.is_empty() {
            row = row.child(
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
            let mut card = div()
                .w(px(280.))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .rounded_lg()
                .border_1()
                .border_color(colors.border)
                .bg(colors.container)
                .child(
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
                    .child(panel_element(&panel["view"], &name, &id, colors, {
                        let commands = self.commands.clone();
                        let plugin = name.clone();
                        let panel = id.clone();
                        move |action| {
                            let _ = commands.send(Command::PanelAction {
                                plugin: plugin.clone(),
                                panel: panel.clone(),
                                action,
                            });
                        }
                    }));
            }
            row = row.child(card);
        }
        div()
            .h(px(240.))
            .flex_shrink_0()
            .border_t_1()
            .border_color(colors.border)
            .bg(colors.background)
            .child(row)
    }

    fn plugin_bar(&self, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self
            .plugins
            .iter()
            .filter(|plugin| plugin["running"] == Value::Bool(true))
            .count();
        div()
            .h(px(36.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .border_t_1()
            .border_color(colors.border)
            .bg(colors.container)
            .child(
                button("toggle-plugins", "Plugins", colors).on_click(cx.listener(
                    |app, _, _, cx| {
                        app.plugins_open = !app.plugins_open;
                        cx.notify();
                    },
                )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(colors.disabled)
                    .child(format!("{running} running, {} loaded", self.plugins.len())),
            )
    }

    fn delete_confirmation(&self, colors: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let session = self.confirm_delete.clone().unwrap_or_default();
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x00000088))
            .child(
                div()
                    .w(px(420.))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_4()
                    .rounded_lg()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.container)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Delete session?"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(colors.disabled)
                            .child("This permanently deletes the session and its transcript."),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                button("cancel-delete", "Cancel", colors).on_click(cx.listener(
                                    |app, _, _, cx| {
                                        app.confirm_delete = None;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(button("confirm-delete", "Delete", colors).on_click(
                                cx.listener({
                                    let session = session.clone();
                                    move |app, _, _, cx| {
                                        app.delete_session(session.clone());
                                        cx.notify();
                                    }
                                }),
                            )),
                    ),
            )
            .into_any_element()
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
        let colors = desktop_colors(window);
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
            .on_action(cx.listener(|app, _: &NewSession, _, cx| app.open_new_session_picker(cx)))
            .on_action(cx.listener(|app, _: &AbortRun, _, _| app.send(Command::Abort)))
            .on_action(cx.listener(|_, _: &QuitApp, _, cx| cx.quit()))
            .on_action(cx.listener(|app, _: &CloseOverlay, _, cx| {
                app.palette_open = false;
                cx.notify();
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .text_color(colors.text)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.sidebar(&colors, cx))
                    .child(main),
            )
            .when(self.plugins_open, |root| {
                root.child(self.plugins_column(&colors, cx))
            })
            .child(self.plugin_bar(&colors, cx))
            .when(self.palette_open, |root| {
                root.child(self.palette(&colors, cx))
            })
            .when(self.confirm_delete.is_some(), |root| {
                root.child(self.delete_confirmation(&colors, cx))
            })
    }
}

fn recent_directories(sessions: &[Value]) -> Vec<String> {
    let mut sessions = sessions.to_vec();
    sessions.sort_by_key(|session| std::cmp::Reverse(session["modified"].as_u64().unwrap_or(0)));
    let mut directories = Vec::new();
    for session in sessions {
        let Some(cwd) = session["cwd"].as_str().filter(|cwd| cwd.starts_with('/')) else {
            continue;
        };
        if !directories.iter().any(|existing| existing == cwd) {
            directories.push(cwd.to_owned());
        }
    }
    directories
}

fn desktop_colors(window: &Window) -> Colors {
    let mut colors = Colors::for_appearance(window);
    if matches!(
        window.appearance(),
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
    ) {
        colors.background = gpui::rgb(0x121212);
        colors.container = gpui::rgb(0x1f1f1f);
        colors.border = gpui::rgb(0x2e2e2e);
        colors.separator = colors.border;
        colors.text = gpui::rgb(0xd6d6d6);
        colors.disabled = gpui::rgb(0x6b6b6b);
        colors.selected = gpui::rgb(0x2b2b2b);
        colors.selected_text = colors.text;
    }
    colors
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn recent_directories_are_unique_and_newest_first() {
        let sessions = vec![
            json!({"cwd":"/src/old","modified":1000}),
            json!({"cwd":"/src/new","modified":2000}),
            json!({"cwd":"/src/old","modified":3000}),
            json!({"cwd":"relative","modified":4000}),
        ];
        assert_eq!(
            super::recent_directories(&sessions),
            vec!["/src/old".to_owned(), "/src/new".to_owned()]
        );
    }
}
