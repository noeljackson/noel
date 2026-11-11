//! The window: sessions on the left, the selected session's transcript and
//! the composer in the middle, the plugins' commands and panels on the
//! right, and a command palette over them (secondary-k).

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use gpui::{
    actions, div, prelude::*, px, size, AnyElement, App, Bounds, Context, Entity, FocusHandle,
    FontWeight, KeyBinding, PathPromptOptions, QuitMode, SharedString, Window, WindowBounds,
    WindowOptions,
};
use gpui_elements::editable_text::{
    actions::{default_bindings, DEFAULT_INPUT_CONTEXT},
    text_area, text_input, EditableTextState, StringStorage,
};
use noel_app_ui::{
    block, button,
    design::{
        brand_mark, folder_icon, home_mark, icon_button, menu_action, sidebar_action, Design,
        COMPOSER_RADIUS, CONTENT_WIDTH, FONT_ACTION, FONT_BODY, FONT_META, SELECTOR_CHIP,
        SESSION_INDENT, WEIGHT_CAPTION, WEIGHT_OPTION,
    },
    icons::{icon, Icon},
    panel_element, session_title, text_of_parts,
    transcript::Transcript,
};
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
    transcript: Transcript,
    plugins: Vec<Value>,
    composer: Entity<EditableTextState>,
    session_search: Entity<EditableTextState>,
    palette_input: Entity<EditableTextState>,
    palette_open: bool,
    folder_picker_open: bool,
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
            transcript: Transcript::default(),
            plugins: Vec::new(),
            composer: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            session_search: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            palette_input: cx.new(|cx| {
                EditableTextState::new(StringStorage::from(default_directory(&[], false)), cx)
            }),
            palette_open: false,
            folder_picker_open: false,
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
            Event::Status(status) => {
                if status.starts_with("new session:") {
                    self.folder_picker_open = true;
                }
                self.status = status;
            }
            Event::Sessions(sessions) => self.sessions = sessions,
            Event::View {
                session,
                view,
                entries,
            } => {
                self.selected = Some(session);
                self.view = view;
                self.transcript.set(entries);
                self.folder_picker_open = false;
                self.palette_open = false;
                self.status = "ready".to_owned();
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
        self.transcript.clear();
        self.view = Value::Null;
        self.folder_picker_open = false;
        self.palette_open = false;
        self.menu_session = None;
        self.send(Command::Select(session));
    }

    fn archive_session(&mut self, session: String) {
        let archiving = !self.archived_sessions.contains(&session);
        if archiving {
            self.archived_sessions.insert(session.clone());
        } else {
            self.archived_sessions.remove(&session);
        }
        self.menu_session = None;
        if archiving && self.selected.as_deref() == Some(session.as_str()) {
            self.selected = None;
            self.view = Value::Null;
            self.transcript.clear();
            self.folder_picker_open = true;
        }
    }

    fn delete_session(&mut self, session: String) {
        self.confirm_delete = None;
        self.menu_session = None;
        if self.selected.as_deref() == Some(session.as_str()) {
            self.selected = None;
            self.view = Value::Null;
            self.transcript.clear();
            self.folder_picker_open = true;
        }
        self.send(Command::Delete(session));
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_none() {
            self.open_new_session_picker(cx);
            return;
        }
        let text = self.composer.read(cx).as_str().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.composer.update(cx, |state, cx| state.emplace("", cx));
        self.send(Command::Send(text));
    }

    fn open_new_session_picker(&mut self, cx: &mut Context<Self>) {
        let cwd = default_directory(&self.sessions, self.endpoint.remote.is_some());
        self.palette_input
            .update(cx, |state, cx| state.emplace(&cwd, cx));
        self.palette_open = false;
        self.folder_picker_open = true;
        self.menu_session = None;
        self.server_menu_open = false;
        cx.notify();
    }

    fn create_session(&mut self, cwd: String) {
        self.status = "creating session…".to_owned();
        self.folder_picker_open = false;
        self.palette_open = false;
        self.send(Command::New { cwd });
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(SharedString::from("Choose a folder for the new session")),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = prompt.await {
                if let Some(path) = paths.first() {
                    let cwd = path.display().to_string();
                    let _ = this.update(cx, |app, cx| {
                        app.create_session(cwd);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
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
            .filter(|item| filter.is_empty() || item.label().to_lowercase().contains(&filter))
            .collect()
    }

    fn run_item(&mut self, item: PaletteItem, cx: &mut Context<Self>) {
        if matches!(item, PaletteItem::NewSession) {
            self.open_new_session_picker(cx);
            return;
        }
        match item {
            PaletteItem::NewSession => unreachable!(),
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

    fn sidebar(&self, colors: &Design, cx: &mut Context<Self>) -> impl IntoElement {
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
            .overflow_y_scroll()
            .flex_1()
            .min_h(px(0.));
        for group in noel_app_ui::sessions::group_sessions(&filtered) {
            list = list.child(
                div()
                    .h(px(32.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .mx_2()
                    .px_2()
                    .child(folder_icon(colors))
                    .child(
                        div()
                            .text_size(px(FONT_ACTION))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.muted)
                            .text_ellipsis()
                            .child(group.workspace),
                    ),
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
                        .h(px(32.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_2()
                        .mx_2()
                        .pl(px(SESSION_INDENT - 8.))
                        .pr_2()
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |row| row.bg(colors.selected))
                        .hover(|row| row.bg(colors.hover))
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
                                .min_w(px(0.))
                                .text_size(px(FONT_ACTION))
                                .font_weight(WEIGHT_CAPTION)
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(session_title(&session)),
                        )
                        .when(!hovered && !menu_open, |row| {
                            row.child(
                                div()
                                    .text_size(px(FONT_META))
                                    .text_color(colors.faint)
                                    .child(if archived {
                                        "archived".to_owned()
                                    } else {
                                        noel_app_ui::sessions::relative_time_label(
                                            session["modified"].as_u64().unwrap_or(0),
                                            std::time::SystemTime::now(),
                                        )
                                    }),
                            )
                        })
                        .when(hovered || menu_open, |row| {
                            row.child(
                                icon_button(
                                    SharedString::from(format!("options-{id}")),
                                    Icon::More,
                                    colors,
                                )
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |app, _, _, cx| {
                                        app.menu_session =
                                            if app.menu_session.as_deref() == Some(id.as_str()) {
                                                None
                                            } else {
                                                Some(id.clone())
                                            };
                                        cx.notify();
                                    }
                                })),
                            )
                        })
                        .on_click(cx.listener(move |app, _, _, _| app.select(clicked.clone()))),
                );
                if menu_open {
                    list = list.child(
                        div()
                            .ml(px(38.))
                            .mr_2()
                            .mb_2()
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .p_1()
                            .rounded_lg()
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.menu)
                            .child(
                                menu_action(
                                    SharedString::from(format!("archive-{id}")),
                                    Icon::Archive,
                                    if archived { "Unarchive" } else { "Archive" },
                                    colors,
                                )
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |app, _, _, cx| {
                                        app.archive_session(id.clone());
                                        cx.notify();
                                    }
                                })),
                            )
                            .child(
                                menu_action(
                                    SharedString::from(format!("delete-{id}")),
                                    Icon::Trash,
                                    "Delete",
                                    colors,
                                )
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |app, _, _, cx| {
                                        app.confirm_delete = Some(id.clone());
                                        cx.notify();
                                    }
                                })),
                            )
                            .child(
                                menu_action(
                                    SharedString::from(format!("quit-{id}")),
                                    Icon::X,
                                    "Quit",
                                    colors,
                                )
                                .on_click(cx.listener(|_, _, _, cx| cx.quit())),
                            ),
                    );
                }
            }
        }
        let server_name = self
            .endpoint
            .remote
            .clone()
            .unwrap_or_else(|| "Local server".to_owned());
        div()
            .w(px(278.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .pt_2()
            .border_r_1()
            .border_color(colors.border)
            .bg(colors.sidebar)
            .child(
                div()
                    .h(px(42.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .child(brand_mark())
                    .child(div().flex_1())
                    .child(icon_button("server-options", Icon::More, colors).on_click(
                        cx.listener(|app, _, _, cx| {
                            app.server_menu_open = !app.server_menu_open;
                            cx.notify();
                        }),
                    )),
            )
            .when(self.server_menu_open, |sidebar| {
                sidebar.child(
                    div()
                        .mx_2()
                        .mb_2()
                        .flex()
                        .flex_col()
                        .p_1()
                        .rounded_lg()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.menu)
                        .child(
                            menu_action(
                                "toggle-archived",
                                Icon::Archive,
                                "Show / hide archived",
                                colors,
                            )
                            .on_click(cx.listener(|app, _, _, cx| {
                                app.show_archived = !app.show_archived;
                                cx.notify();
                            })),
                        )
                        .child(
                            menu_action("quit-server", Icon::X, "Quit Noel", colors)
                                .on_click(cx.listener(|_, _, _, cx| cx.quit())),
                        ),
                )
            })
            .child(
                sidebar_action("new-session", Icon::CirclePlus, "New Session", colors)
                    .mx_2()
                    .on_click(cx.listener(|app, _, _, cx| app.open_new_session_picker(cx))),
            )
            .child(
                div()
                    .h(px(32.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .mx_2()
                    .px_2()
                    .child(icon(Icon::Search, 16., colors.muted))
                    .child(
                        text_input("session-search")
                            .state(self.session_search.downgrade())
                            .placeholder("Search")
                            .placeholder_color(colors.faint)
                            .caret_color(gpui::rgb_to_hsla(colors.text))
                            .h_full()
                            .flex_1()
                            .text_size(px(FONT_ACTION))
                            .font_weight(FontWeight::MEDIUM)
                            .whitespace_nowrap(),
                    ),
            )
            .child(
                div()
                    .h(px(46.))
                    .flex_shrink_0()
                    .flex()
                    .items_end()
                    .gap_2()
                    .mx_2()
                    .px_2()
                    .pb_2()
                    .child(
                        div()
                            .text_size(px(FONT_META))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.faint)
                            .child("SESSIONS"),
                    )
                    .child(div().flex_1())
                    .child(
                        icon_button("section-new", Icon::Plus, colors)
                            .on_click(cx.listener(|app, _, _, cx| app.open_new_session_picker(cx))),
                    ),
            )
            .child(list)
            .child(
                div()
                    .h(px(42.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .w(px(7.))
                            .h(px(7.))
                            .rounded_full()
                            .bg(if self.connected {
                                gpui::rgb(0x55b96f)
                            } else {
                                colors.disabled
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(FONT_ACTION))
                            .text_ellipsis()
                            .child(server_name),
                    )
                    .child(
                        div()
                            .text_size(px(FONT_META))
                            .text_color(colors.faint)
                            .child(if self.connected {
                                "Connected"
                            } else {
                                "Offline"
                            }),
                    ),
            )
    }

    fn home_hint(&self, colors: &Design) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .px_4()
            .pb_4()
            .child(home_mark())
            .child(
                div()
                    .text_size(px(FONT_BODY + 16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.accent)
                    .child("‹Code›"),
            )
    }

    fn transcript(&self, colors: &Design) -> impl IntoElement {
        // What is still streaming, drawn as the row after the last entry.
        let live = self.view["docs"]["pi.live"].clone();
        let tail_colors = colors.colors.clone();
        let tail = move || {
            let colors = &tail_colors;
            let mut column = div().flex().flex_col().gap_4();
            if let Some(partial) = live["generation"]["message"]["content"].as_array() {
                let text = text_of_parts(partial);
                if !text.is_empty() {
                    column = column.child(block("", &text, colors, false));
                }
            }
            if let Some(tools) = live["tools"].as_array() {
                for tool in tools {
                    column =
                        column.child(div().text_xs().text_color(colors.disabled).child(format!(
                            "{} {}",
                            tool["name"].as_str().unwrap_or_default(),
                            tool["status"].as_str().unwrap_or_default()
                        )));
                }
            }
            column.into_any_element()
        };
        div().flex().justify_center().flex_1().min_h(px(0.)).child(
            self.transcript
                .element(colors, tail)
                .max_w(px(760.))
                .w_full(),
        )
    }

    fn composer(&self, colors: &Design, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = !self.composer.read(cx).as_str().trim().is_empty();
        let status = if matches!(self.status.as_str(), "connected" | "ready") {
            String::new()
        } else {
            self.status.clone()
        };
        div()
            .flex()
            .justify_center()
            .px_4()
            .pt_2()
            .pb_3()
            .when(self.selected.is_none(), |row| row.pt_0())
            .child(
                div()
                    .max_w(px(CONTENT_WIDTH))
                    .w_full()
                    .flex()
                    .flex_col()
                    .rounded(px(COMPOSER_RADIUS))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.container)
                    .child(
                        text_area("composer")
                            .state(self.composer.downgrade())
                            .placeholder("Type a message…")
                            .placeholder_color(colors.faint)
                            .caret_color(gpui::rgb_to_hsla(colors.text))
                            .w_full()
                            .h(px(64.))
                            .text_size(px(FONT_BODY))
                            .whitespace_normal()
                            .overflow_y_scroll()
                            .px_4()
                            .pt_4(),
                    )
                    .child(
                        div()
                            .h(px(42.))
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .pb_2()
                            .child(
                                div()
                                    .text_size(px(FONT_META))
                                    .text_color(colors.faint)
                                    .child(status),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("send")
                                    .w(px(32.))
                                    .h(px(32.))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .bg(if ready { colors.text } else { colors.selected })
                                    .text_color(if ready {
                                        colors.background
                                    } else {
                                        colors.disabled
                                    })
                                    .cursor_pointer()
                                    .hover(|button| button.opacity(0.85))
                                    .active(|button| button.opacity(0.7))
                                    .child(icon(
                                        Icon::ArrowUp,
                                        16.,
                                        if ready {
                                            colors.background
                                        } else {
                                            colors.disabled
                                        },
                                    ))
                                    .on_click(cx.listener(|app, _, _, cx| app.submit(cx))),
                            ),
                    ),
            )
    }

    fn plugins_column(&self, colors: &Design, cx: &mut Context<Self>) -> impl IntoElement {
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

    fn plugin_bar(&self, colors: &Design, cx: &mut Context<Self>) -> impl IntoElement {
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

    fn delete_confirmation(&self, colors: &Design, cx: &mut Context<Self>) -> AnyElement {
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

    fn folder_picker(&self, colors: &Design, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.palette_input.read(cx).as_str().trim().to_owned();
        let mut directories = recent_directories(&self.sessions);
        if current.starts_with('/') && !directories.contains(&current) {
            directories.insert(0, current.clone());
        }
        let mut list = div().flex().flex_col();
        for (index, path) in directories.into_iter().take(5).enumerate() {
            let name = directory_name(&path);
            let selected = path == current;
            list = list.child(
                div()
                    .id(SharedString::from(format!("folder-{index}")))
                    .h(px(46.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(colors.selected))
                    .hover(|row| row.bg(colors.hover))
                    .child(folder_icon(colors))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(FONT_BODY))
                                    .font_weight(WEIGHT_OPTION)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_size(px(FONT_META))
                                    .font_weight(WEIGHT_OPTION)
                                    .text_color(colors.faint)
                                    .text_ellipsis()
                                    .child(display_path(&path)),
                            ),
                    )
                    .when(selected, |row| {
                        row.child(div().ml_2().child(icon(Icon::Check, 16., colors.text)))
                    })
                    .on_click(cx.listener(move |app, _, _, _| app.create_session(path.clone()))),
            );
        }
        div()
            .relative()
            .h(px(36.))
            .w_full()
            .flex()
            .justify_center()
            .px_4()
            .when(self.selected.is_some(), |row| {
                row.absolute().left(px(0.)).right(px(0.)).bottom(px(128.))
            })
            .child(
                div()
                    .relative()
                    .w_full()
                    .max_w(px(CONTENT_WIDTH))
                    .pl_4()
                    .child(
                        div()
                            .relative()
                            .child(
                                div()
                                    .id("folder-selector")
                                    .h(px(SELECTOR_CHIP))
                                    .mb_1()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .px_2()
                                    .rounded(px(10.))
                                    .bg(colors.selected)
                                    .cursor_pointer()
                                    .child(folder_icon(colors))
                                    .child(
                                        div()
                                            .text_size(px(FONT_ACTION))
                                            .font_weight(WEIGHT_OPTION)
                                            .child(directory_name(&current)),
                                    )
                                    .child(icon(
                                        if self.folder_picker_open {
                                            Icon::ChevronUp
                                        } else {
                                            Icon::ChevronDown
                                        },
                                        14.,
                                        colors.muted,
                                    ))
                                    .on_click(cx.listener(|app, _, _, cx| {
                                        app.folder_picker_open = !app.folder_picker_open;
                                        cx.notify();
                                    })),
                            )
                            .when(self.folder_picker_open, |picker| {
                                picker.child(
                                    div()
                                        .absolute()
                                        .left(px(0.))
                                        .top(px(32.))
                                        .w(px(296.))
                                        .flex()
                                        .flex_col()
                                        .p_1()
                                        .rounded_lg()
                                        .border_1()
                                        .border_color(colors.separator)
                                        .bg(colors.menu)
                                        .shadow_lg()
                                        .child(
                                            div()
                                                .px_2()
                                                .py_1()
                                                .text_size(px(FONT_BODY))
                                                .text_color(colors.muted)
                                                .child("Recent folders"),
                                        )
                                        .child(list)
                                        .child(
                                            div()
                                                .id("choose-folder")
                                                .h(px(36.))
                                                .mt_1()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .px_2()
                                                .rounded_md()
                                                .border_t_1()
                                                .border_color(colors.separator)
                                                .cursor_pointer()
                                                .text_size(px(FONT_BODY))
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(colors.muted)
                                                .hover(|row| {
                                                    row.bg(colors.hover).text_color(colors.text)
                                                })
                                                .child(icon(Icon::FolderPlus, 16., colors.muted))
                                                .child("Choose folder…")
                                                .on_click(cx.listener(|app, _, _, cx| {
                                                    app.choose_folder(cx)
                                                })),
                                        ),
                                )
                            }),
                    ),
            )
    }

    fn palette(&self, colors: &Design, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().flex().flex_col().gap_1();
        for (index, item) in self.palette_items(cx).into_iter().enumerate().take(20) {
            list = list.child(
                div()
                    .id(SharedString::from(format!("palette-{index}")))
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .px_3()
                    .rounded_md()
                    .text_sm()
                    .cursor_pointer()
                    .hover(|row| row.bg(colors.selected).text_color(colors.selected_text))
                    .child(item.label())
                    .on_click(cx.listener(move |app, _, _, cx| app.run_item(item.clone(), cx))),
            );
        }
        div().absolute().inset_0().flex().justify_center().child(
            div()
                .w(px(600.))
                .h(px(460.))
                .mt(px(72.))
                .p_2()
                .flex()
                .flex_col()
                .gap_2()
                .rounded_2xl()
                .border_1()
                .border_color(colors.border)
                .bg(colors.container)
                .child(
                    text_input("palette")
                        .state(self.palette_input.downgrade())
                        .placeholder("Search commands and sessions")
                        .placeholder_color(colors.disabled)
                        .caret_color(gpui::rgb_to_hsla(colors.text))
                        .h(px(40.))
                        .w_full()
                        .text_sm()
                        .whitespace_nowrap()
                        .px_3()
                        .rounded_lg()
                        .bg(colors.background),
                )
                .child(list.id("palette-results").overflow_y_scroll()),
        )
    }
}

impl Render for NoelApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Design::for_window(window);
        let mut main = div().relative().flex().flex_col().flex_1().min_w(px(0.));
        if self.selected.is_none() {
            main = main
                .child(div().flex_1())
                .child(self.home_hint(&colors))
                .child(self.folder_picker(&colors, cx))
                .child(self.composer(&colors, cx))
                .child(div().flex_1());
        } else {
            main = main
                .child(self.transcript(&colors))
                .child(self.composer(&colors, cx));
            if self.folder_picker_open {
                main = main.child(self.folder_picker(&colors, cx));
            }
        }
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|app, _: &SendMessage, _, cx| app.submit(cx)))
            .on_action(cx.listener(|app, _: &TogglePalette, _, cx| {
                app.palette_open = !app.palette_open;
                app.folder_picker_open = false;
                app.menu_session = None;
                app.server_menu_open = false;
                if app.palette_open {
                    app.palette_input
                        .update(cx, |state, cx| state.emplace("", cx));
                }
                cx.notify();
            }))
            .on_action(cx.listener(|app, _: &NewSession, _, cx| app.open_new_session_picker(cx)))
            .on_action(cx.listener(|app, _: &AbortRun, _, _| app.send(Command::Abort)))
            .on_action(cx.listener(|_, _: &QuitApp, _, cx| cx.quit()))
            .on_action(cx.listener(|app, _: &CloseOverlay, _, cx| {
                app.palette_open = false;
                app.folder_picker_open = false;
                app.menu_session = None;
                app.server_menu_open = false;
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

fn default_directory(sessions: &[Value], remote: bool) -> String {
    if !remote {
        if let Ok(directory) = std::env::current_dir() {
            if directory.is_absolute() && directory.as_os_str() != "/" {
                return directory.display().to_string();
            }
        }
    }
    if let Some(directory) = recent_directories(sessions).into_iter().next() {
        return directory;
    }
    if !remote {
        if let Ok(home) = std::env::var("HOME") {
            if home.starts_with('/') {
                return home;
            }
        }
    }
    "/".to_owned()
}

fn directory_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_owned()
}

fn display_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_owned()
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn directory_names_include_the_root_and_final_component() {
        assert_eq!(super::directory_name("/"), "/");
        assert_eq!(super::directory_name("/Users/noel"), "noel");
        assert_eq!(super::directory_name("/src/noel/"), "noel");
    }

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
