mod rpc;
mod state;

use async_channel::Receiver;
use editor::{Editor, EditorElement, EditorStyle};
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement,
    Render, SharedString, Styled, Task, TextStyle, Window, actions, div, px, relative,
};
use settings::Settings;
use terminal_view::default_working_directory;
use theme_settings::ThemeSettings;
use ui::{Button, Label, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

pub use rpc::{RpcClient, RpcClientEvent, RpcCommand, ZepiCommand, resolve_zepi_command};
pub use state::{DisplayRow, ExtensionUiResponse, ZepiPanelState};

const ZEPI_PANEL_KEY: &str = "ZepiPanel";

actions!(zepi, [TogglePanel]);

pub fn init(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, window, cx: &mut Context<Workspace>| {
            if let Some(window) = window {
                let workspace_handle = workspace.weak_handle();
                let panel = cx.new(|cx| ZepiPanel::new(workspace_handle, window, cx));
                workspace.add_panel(panel, window, cx);
            }

            workspace.register_action(|workspace, _: &TogglePanel, window, cx| {
                workspace.toggle_panel_focus::<ZepiPanel>(window, cx);
            });
        },
    )
    .detach();
}

pub struct ZepiPanel {
    focus_handle: FocusHandle,
    workspace: gpui::WeakEntity<Workspace>,
    state: ZepiPanelState,
    input_editor: Entity<Editor>,
    client: Option<RpcClient>,
    event_task: Task<Option<()>>,
    rpc_generation: u64,
    active: bool,
}

impl ZepiPanel {
    fn new(
        workspace: gpui::WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Ask Zepi or run /command…", window, cx);
            editor
        });
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            state: ZepiPanelState::new(),
            input_editor,
            client: None,
            event_task: Task::ready(None),
            rpc_generation: 0,
            active: false,
        }
    }

    fn start_rpc(&mut self, cx: &mut Context<Self>) {
        self.stop_rpc();
        self.rpc_generation += 1;
        let rpc_generation = self.rpc_generation;
        let cwd = self
            .workspace
            .update(cx, |workspace, cx| default_working_directory(workspace, cx))
            .ok()
            .flatten();

        match RpcClient::spawn(cwd) {
            Ok((client, events)) => {
                if let Err(err) = client.send(RpcCommand::GetCommands) {
                    self.state
                        .push_error(format!("Failed to request Zepi commands: {err}"));
                }
                self.state.set_status("Connected");
                self.event_task = Self::watch_events(rpc_generation, events, cx);
                self.client = Some(client);
            }
            Err(err) => {
                self.state.set_status("Failed to start");
                self.state.push_error(err.to_string());
            }
        }
        cx.notify();
    }

    fn stop_rpc(&mut self) {
        if let Some(client) = self.client.take() {
            client.kill();
        }
        self.event_task = Task::ready(None);
        self.rpc_generation += 1;
    }

    fn watch_events(
        rpc_generation: u64,
        events: Receiver<RpcClientEvent>,
        cx: &mut Context<Self>,
    ) -> Task<Option<()>> {
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this
                    .update(cx, |panel, cx| {
                        if panel.rpc_generation == rpc_generation {
                            panel.apply_rpc_event(event);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
            None
        })
    }

    fn apply_rpc_event(&mut self, event: RpcClientEvent) {
        match event {
            RpcClientEvent::StdoutLine(line) => {
                if let Err(err) = self.state.apply_json_line(&line) {
                    self.state
                        .push_error(format!("Invalid Zepi RPC JSON: {err}"));
                }
                let responses = self.state.take_pending_extension_ui_responses();
                for response in responses {
                    match serde_json::to_string(&response) {
                        Ok(line) => {
                            if let Some(client) = &self.client
                                && let Err(err) = client.send_json_line(line)
                            {
                                self.state.push_error(format!(
                                    "Failed to respond to extension UI: {err}"
                                ));
                            }
                        }
                        Err(err) => self
                            .state
                            .push_error(format!("Failed to encode extension UI response: {err}")),
                    }
                }
            }
            RpcClientEvent::StderrLine(line) => self.state.push_error(line),
            RpcClientEvent::Exited(code) => {
                self.client = None;
                self.state.set_status(match code {
                    Some(0) => "Exited",
                    Some(_) => "Exited with error",
                    None => "Terminated",
                });
            }
            RpcClientEvent::Failed(error) => self.state.push_error(error),
        }
    }

    fn send_command(&mut self, command: RpcCommand, cx: &mut Context<Self>) {
        if self.client.is_none() {
            self.start_rpc(cx);
        }
        if let Some(client) = &self.client
            && let Err(err) = client.send(command)
        {
            self.state.push_error(err.to_string());
        }
        cx.notify();
    }

    fn send_input_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let message = self.input_editor.read(cx).text(cx);
        if self.send_prompt(message, cx) {
            self.input_editor.update(cx, |editor, cx| {
                editor.set_text("", window, cx);
            });
        }
    }

    fn send_prompt(&mut self, message: String, cx: &mut Context<Self>) -> bool {
        if message.trim().is_empty() {
            self.state.push_error("Cannot send an empty prompt");
            cx.notify();
            return false;
        }
        if self.client.is_none() {
            self.start_rpc(cx);
        }
        if let Some(client) = &self.client {
            match client.send(RpcCommand::Prompt {
                message: message.clone(),
            }) {
                Ok(()) => {
                    self.state.push_user(message);
                    cx.notify();
                    return true;
                }
                Err(err) => self.state.push_error(err.to_string()),
            }
        }
        cx.notify();
        false
    }

    fn render_text_input(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features.clone(),
            font_fallbacks: settings.ui_font.fallbacks.clone(),
            font_size: rems(0.875).into(),
            font_weight: settings.ui_font.weight,
            line_height: relative(1.3),
            ..Default::default()
        };

        EditorElement::new(
            &self.input_editor,
            EditorStyle {
                background: cx.theme().colors().editor_background,
                local_player: cx.theme().players().local(),
                text: text_style,
                ..Default::default()
            },
        )
    }

    fn render_row(row: &DisplayRow) -> impl IntoElement {
        let (prefix, text) = match row {
            DisplayRow::Status(text) => ("• ", text),
            DisplayRow::Error(text) => ("! ", text),
            DisplayRow::User(text) => ("You: ", text),
            DisplayRow::Assistant(text) => ("Zepi: ", text),
            DisplayRow::Notification(text) => ("Note: ", text),
        };
        div().child(format!("{prefix}{text}"))
    }
}

impl Focusable for ZepiPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ZepiPanel {}

impl Panel for ZepiPanel {
    fn persistent_name() -> &'static str {
        "Zepi"
    }

    fn panel_key() -> &'static str {
        ZEPI_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(
            position,
            DockPosition::Left | DockPosition::Right | DockPosition::Bottom
        )
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
        px(420.)
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        self.active = active;
        if active && self.client.is_none() {
            self.start_rpc(cx);
        }
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ZedAssistant)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Zepi")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(TogglePanel)
    }

    fn activation_priority(&self) -> u32 {
        4
    }

    fn is_agent_panel(&self) -> bool {
        false
    }
}

impl Render for ZepiPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("ZepiPanel")
            .track_focus(&self.focus_handle)
            .p_4()
            .gap_2()
            .flex()
            .flex_col()
            .child(Label::new(SharedString::from("Zepi")))
            .child(format!("Status: {}", self.state.status()))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Button::new("zepi-restart", "Restart").on_click(cx.listener(
                        |panel, _, _window, cx| {
                            panel.start_rpc(cx);
                        },
                    )))
                    .child(Button::new("zepi-send", "Send").on_click(cx.listener(
                        |panel, _, window, cx| {
                            panel.send_input_prompt(window, cx);
                        },
                    )))
                    .child(
                        Button::new("zepi-new-session", "New Session").on_click(cx.listener(
                            |panel, _, _window, cx| {
                                panel.send_command(RpcCommand::NewSession, cx);
                            },
                        )),
                    )
                    .child(Button::new("zepi-abort", "Abort").on_click(cx.listener(
                        |panel, _, _window, cx| {
                            panel.send_command(RpcCommand::Abort, cx);
                        },
                    ))),
            )
            .child(
                div()
                    .p_2()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(self.render_text_input(cx)),
            )
            .child(format!(
                "Slash commands: {}",
                self.state.slash_commands().join(", ")
            ))
            .children(self.state.rows().iter().map(Self::render_row))
    }
}
