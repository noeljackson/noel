use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use gpui::{
    actions, colors::Colors, div, prelude::*, px, App, Context, ElementId, Entity, FocusHandle,
    FontWeight, KeyBinding, SharedString, WeakEntity, Window, WindowOptions,
};
use gpui_elements::editable_text::{
    actions::{default_bindings, DEFAULT_INPUT_CONTEXT},
    text_area, text_input, EditableTextState, StringStorage,
};
use noel_app_ui::{block, button, entry_element, panel_element, session_title, text_of_parts};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::backend::{Command, Endpoint, Event};

const CONTEXT: &str = "NoelIos";

actions!(noel_ios, [SendMessage, Back]);

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Connect,
    Sessions,
    Chat,
    Plugins,
}

pub fn run(
    events: UnboundedReceiver<Event>,
    commands: UnboundedSender<Command>,
    endpoints: UnboundedSender<Endpoint>,
) {
    gpui_platform::application().run(move |cx: &mut App| {
        cx.bind_keys(default_bindings().as_keybindings(Some(DEFAULT_INPUT_CONTEXT)));
        cx.bind_keys([
            KeyBinding::new("secondary-enter", SendMessage, Some(CONTEXT)),
            KeyBinding::new("escape", Back, Some(CONTEXT)),
        ]);
        cx.open_window(WindowOptions::default(), |window, cx| {
            cx.new(|cx| IosApp::new(events, commands, endpoints, window, cx))
        })
        .expect("a window");
        cx.activate(true);
    });
}

struct IosApp {
    commands: UnboundedSender<Command>,
    endpoints: UnboundedSender<Endpoint>,
    endpoint_started: bool,
    screen: Screen,
    connected: bool,
    status: String,
    sessions: Vec<Value>,
    selected: Option<String>,
    view: Value,
    entries: Vec<Value>,
    plugins: Vec<Value>,
    host: Entity<EditableTextState>,
    user: Entity<EditableTextState>,
    password: Entity<EditableTextState>,
    program: Entity<EditableTextState>,
    cwd: Entity<EditableTextState>,
    composer: Entity<EditableTextState>,
    focus: FocusHandle,
}

impl IosApp {
    fn new(
        mut events: UnboundedReceiver<Event>,
        commands: UnboundedSender<Command>,
        endpoints: UnboundedSender<Endpoint>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |app, cx| {
                        app.apply(event);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        IosApp {
            commands,
            endpoints,
            endpoint_started: false,
            screen: Screen::Connect,
            connected: false,
            status: "enter an SSH server".to_owned(),
            sessions: Vec::new(),
            selected: None,
            view: Value::Null,
            entries: Vec::new(),
            plugins: Vec::new(),
            host: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            user: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            password: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            program: cx.new(|cx| EditableTextState::new(StringStorage::from("noel"), cx)),
            cwd: cx.new(|cx| EditableTextState::new(StringStorage::from("/"), cx)),
            composer: cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)),
            focus,
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Connected => {
                self.connected = true;
                self.status = "connected".to_owned();
                if self.screen == Screen::Connect {
                    self.screen = Screen::Sessions;
                }
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

    fn connect(&mut self, cx: &mut Context<Self>) {
        if self.endpoint_started {
            return;
        }
        let host = self.host.read(cx).as_str().trim().to_owned();
        let username = self.user.read(cx).as_str().trim().to_owned();
        if host.is_empty() || username.is_empty() {
            self.status = "host and user are required".to_owned();
            return;
        }
        let endpoint = Endpoint::new(
            host,
            22,
            username,
            self.password.read(cx).as_str().to_owned(),
            self.program.read(cx).as_str().trim().to_owned(),
        );
        match self.endpoints.send(endpoint) {
            Ok(()) => {
                self.endpoint_started = true;
                self.status = "connecting".to_owned();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn select(&mut self, session: String) {
        self.selected = Some(session.clone());
        self.entries.clear();
        self.view = Value::Null;
        self.send(Command::Select(session));
        self.screen = Screen::Chat;
    }

    fn new_session(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cwd.read(cx).as_str().trim().to_owned();
        if cwd.starts_with('/') {
            self.send(Command::New { cwd });
            self.screen = Screen::Chat;
        } else {
            self.status = "the remote directory must be absolute".to_owned();
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).as_str().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.composer.update(cx, |state, cx| state.emplace("", cx));
        self.send(Command::Send(text));
    }

    fn header(&self, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(colors.border)
            .child(div().font_weight(FontWeight::SEMIBOLD).child("noel"))
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(if self.connected {
                        colors.text
                    } else {
                        colors.disabled
                    })
                    .child(self.status.clone()),
            )
            .child(
                button("sessions", "Sessions", colors).on_click(cx.listener(|app, _, _, _| {
                    app.screen = Screen::Sessions;
                })),
            )
            .child(
                button("plugins", "Plugins", colors).on_click(cx.listener(|app, _, _, _| {
                    app.screen = Screen::Plugins;
                })),
            )
    }

    fn connection(&self, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .child(div().text_lg().child("Server over SSH"))
            .child(field(
                "host",
                self.host.downgrade(),
                "host or user@host",
                colors,
            ))
            .child(field("user", self.user.downgrade(), "user", colors))
            .child(field(
                "password",
                self.password.downgrade(),
                "password (not stored)",
                colors,
            ))
            .child(field(
                "program",
                self.program.downgrade(),
                "remote noel program",
                colors,
            ))
            .child(
                button("connect", "Connect", colors)
                    .on_click(cx.listener(|app, _, _, cx| app.connect(cx))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(colors.disabled)
                    .child("The host key is fingerprinted on first use and must not change."),
            )
    }

    fn sessions(&self, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().id("sessions").flex().flex_col().gap_1().p_3();
        for session in &self.sessions {
            let id = session["sessionId"].as_str().unwrap_or_default().to_owned();
            list = list.child(
                div()
                    .id(SharedString::from(format!("session-{id}")))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|row| row.bg(colors.selected).text_color(colors.selected_text))
                    .child(div().text_sm().child(session_title(session)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.disabled)
                            .child(format!("{} messages", session["messageCount"])),
                    )
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |app, _, _, _| app.select(id.clone())
                    })),
            );
        }
        if self.sessions.is_empty() {
            list = list.child(
                div()
                    .text_color(colors.disabled)
                    .child("No sessions on this server."),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .gap_2()
                    .p_3()
                    .child(field(
                        "cwd",
                        self.cwd.downgrade(),
                        "/remote/directory",
                        colors,
                    ))
                    .child(
                        button("new-session", "New session", colors)
                            .on_click(cx.listener(|app, _, _, cx| app.new_session(cx))),
                    ),
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
                    .child("Pick a session or make a new one."),
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

    fn plugins(&self, colors: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mut column = div()
            .id("plugins")
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .overflow_y_scroll();
        for plugin in &self.plugins {
            let name = plugin["name"].as_str().unwrap_or_default().to_owned();
            let mut card = div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .rounded_lg()
                .bg(colors.container)
                .child(div().font_weight(FontWeight::SEMIBOLD).child(name.clone()));
            for command in plugin["contributions"]["commands"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let command = command.as_str().unwrap_or_default().to_owned();
                card = card.child(
                    button(format!("command-{name}-{command}"), &command, colors).on_click(
                        cx.listener({
                            let name = name.clone();
                            let command = command.clone();
                            move |app, _, _, _| {
                                app.send(Command::PluginRun {
                                    plugin: name.clone(),
                                    name: command.clone(),
                                })
                            }
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
            column = column.child(card);
        }
        column
    }
}

impl Render for IosApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::for_appearance(window);
        let viewport = window.viewport_size();
        let visible = window.fully_visible_bounds();
        let right = (viewport.width - visible.origin.x - visible.size.width).max(px(0.));
        let bottom = (viewport.height - visible.origin.y - visible.size.height).max(px(0.));
        let content = match self.screen {
            Screen::Connect => self.connection(&colors, cx).into_any_element(),
            Screen::Sessions => self.sessions(&colors, cx).into_any_element(),
            Screen::Chat => div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.))
                .child(self.transcript(&colors))
                .child(self.composer(&colors, cx))
                .into_any_element(),
            Screen::Plugins => self.plugins(&colors, cx).into_any_element(),
        };
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|app, _: &SendMessage, _, cx| app.submit(cx)))
            .on_action(cx.listener(|app, _: &Back, _, _| {
                app.screen = Screen::Sessions;
            }))
            .size_full()
            .pt(visible.origin.y)
            .pb(bottom)
            .pl(visible.origin.x)
            .pr(right)
            .flex()
            .flex_col()
            .bg(colors.background)
            .text_color(colors.text)
            .child(self.header(&colors, cx))
            .child(content)
    }
}

fn field(
    id: impl Into<ElementId>,
    state: WeakEntity<EditableTextState>,
    placeholder: &str,
    colors: &Colors,
) -> impl IntoElement {
    text_input(id)
        .state(state)
        .placeholder(placeholder)
        .placeholder_color(colors.disabled)
        .caret_color(gpui::rgb_to_hsla(colors.text))
        .w_full()
        .whitespace_nowrap()
        .px_3()
        .py_2()
        .rounded_lg()
        .border_1()
        .border_color(colors.border)
        .bg(colors.container)
}
