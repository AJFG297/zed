use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayRow {
    Status(String),
    Error(String),
    User(String),
    Assistant(String),
    Notification(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtensionUiResponse {
    #[serde(rename = "type")]
    pub message_type: &'static str,
    pub id: String,
    pub cancelled: bool,
}

#[derive(Debug, Default)]
pub struct ZepiPanelState {
    status: String,
    rows: Vec<DisplayRow>,
    slash_commands: Vec<String>,
    pending_extension_ui_responses: Vec<ExtensionUiResponse>,
}

impl ZepiPanelState {
    pub fn new() -> Self {
        Self {
            status: "Not connected".to_owned(),
            rows: Vec::new(),
            slash_commands: Vec::new(),
            pending_extension_ui_responses: Vec::new(),
        }
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn rows(&self) -> &[DisplayRow] {
        &self.rows
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    pub fn push_error(&mut self, error: impl Into<String>) {
        self.rows.push(DisplayRow::Error(error.into()));
    }

    pub fn push_user(&mut self, message: impl Into<String>) {
        self.rows.push(DisplayRow::User(message.into()));
    }

    pub fn slash_commands(&self) -> &[String] {
        &self.slash_commands
    }

    pub fn take_pending_extension_ui_responses(&mut self) -> Vec<ExtensionUiResponse> {
        std::mem::take(&mut self.pending_extension_ui_responses)
    }

    pub fn apply_json_line(&mut self, line: &str) -> Result<(), serde_json::Error> {
        let message = serde_json::from_str::<RpcEnvelope>(line)?;
        self.apply_message(message);
        Ok(())
    }

    fn apply_message(&mut self, message: RpcEnvelope) {
        match message.message_type.as_str() {
            "response" => self.apply_response(message),
            "extension_ui_request" => self.apply_extension_ui_request(message),
            _ => self.apply_event(message),
        }
    }

    fn apply_response(&mut self, message: RpcEnvelope) {
        if message.command.as_deref() == Some("get_commands") {
            self.slash_commands = extract_commands(&message.payload);
            self.status = format!("{} commands available", self.slash_commands.len());
            return;
        }

        if let Some(error) = message.error {
            self.rows.push(DisplayRow::Error(error));
        }
    }

    fn apply_event(&mut self, message: RpcEnvelope) {
        match message.message_type.as_str() {
            "agent_start" => self.status = "Running".to_owned(),
            "agent_end" => self.status = "Idle".to_owned(),
            "message_update" => {
                let text = message
                    .payload
                    .get("assistantMessageEvent")
                    .and_then(|event| event.get("delta"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if !text.is_empty() {
                    append_assistant_delta(&mut self.rows, text);
                }
            }
            "message_start" => {
                if message
                    .payload
                    .get("message")
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str)
                    == Some("user")
                {
                    let text = extract_message_text(
                        message.payload.get("message").unwrap_or(&Value::Null),
                    );
                    if !text.is_empty() {
                        self.rows.push(DisplayRow::User(text));
                    }
                }
            }
            "tool_execution_start" => {
                let tool_name = message
                    .payload
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                self.rows
                    .push(DisplayRow::Status(format!("Running tool: {tool_name}")));
            }
            "extension_error" => self
                .rows
                .push(DisplayRow::Error(extract_text(&message.payload))),
            other => self
                .rows
                .push(DisplayRow::Status(format!("Unhandled RPC event: {other}"))),
        }
    }

    fn apply_extension_ui_request(&mut self, message: RpcEnvelope) {
        let id = message.id.unwrap_or_default();
        let method = message.method.unwrap_or_else(|| "unknown".to_owned());
        match method.as_str() {
            "notify" => self
                .rows
                .push(DisplayRow::Notification(extract_text(&message.payload))),
            "setStatus" => self.status = extract_status_text(&message.payload),
            "setTitle" => self
                .rows
                .push(DisplayRow::Status(extract_text(&message.payload))),
            "set_editor_text" => self.rows.push(DisplayRow::Status(
                "Extension updated editor text".to_owned(),
            )),
            "setWidget" => self.rows.extend(
                extract_widget_lines(&message.payload)
                    .into_iter()
                    .map(DisplayRow::Status),
            ),
            "confirm" | "input" | "select" | "editor" => self.fail_closed(id, method),
            _ => self.fail_closed(id, method),
        }
    }

    fn fail_closed(&mut self, id: String, method: String) {
        self.rows.push(DisplayRow::Error(format!(
            "Unsupported extension UI request cancelled: {method}"
        )));
        if !id.is_empty() {
            self.pending_extension_ui_responses
                .push(ExtensionUiResponse {
                    message_type: "extension_ui_response",
                    id,
                    cancelled: true,
                });
        }
    }
}

#[derive(Debug, Deserialize)]
struct RpcEnvelope {
    #[serde(rename = "type")]
    message_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(flatten)]
    payload: Value,
}

fn extract_text(payload: &Value) -> String {
    for key in [
        "text",
        "content",
        "message",
        "status",
        "statusText",
        "title",
    ] {
        if let Some(text) = payload.get(key).and_then(Value::as_str) {
            return text.to_owned();
        }
    }
    String::new()
}

fn extract_status_text(payload: &Value) -> String {
    payload
        .get("statusText")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| extract_text(payload))
}

fn extract_widget_lines(payload: &Value) -> Vec<String> {
    payload
        .get("widgetLines")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn extract_commands(payload: &Value) -> Vec<String> {
    payload
        .get("data")
        .and_then(|data| data.get("commands"))
        .or_else(|| payload.get("commands"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|command| {
            command.as_str().map(str::to_owned).or_else(|| {
                command
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
        })
        .map(|command| {
            if command.starts_with('/') {
                command
            } else {
                format!("/{command}")
            }
        })
        .collect()
}

fn extract_message_text(message: &Value) -> String {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn append_assistant_delta(rows: &mut Vec<DisplayRow>, text: String) {
    if let Some(DisplayRow::Assistant(previous)) = rows.last_mut() {
        previous.push_str(&text);
    } else {
        rows.push(DisplayRow::Assistant(text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn streams_assistant_delta_into_single_row() {
        let mut state = ZepiPanelState::new();

        state.apply_json_line(r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"hel"}}"#).unwrap();
        state.apply_json_line(r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"lo"}}"#).unwrap();

        assert_eq!(state.rows(), &[DisplayRow::Assistant("hello".to_owned())]);
    }

    #[test]
    fn records_get_commands_as_slash_completions() {
        let mut state = ZepiPanelState::new();

        state
            .apply_json_line(
                r#"{"type":"response","command":"get_commands","success":true,"data":{"commands":[{"name":"plan"},"/reload","skill"]}}"#,
            )
            .unwrap();

        assert_eq!(state.slash_commands(), &["/plan", "/reload", "/skill"]);
        assert_eq!(state.status(), "3 commands available");
    }

    #[test]
    fn fire_and_forget_extension_ui_requests_do_not_create_responses() {
        let mut state = ZepiPanelState::new();

        state
            .apply_json_line(r#"{"type":"extension_ui_request","id":"status","method":"setStatus","statusText":"Planning"}"#)
            .unwrap();
        state
            .apply_json_line(r#"{"type":"extension_ui_request","id":"widget","method":"setWidget","widgetLines":["A","B"]}"#)
            .unwrap();

        assert_eq!(state.status(), "Planning");
        assert_eq!(state.take_pending_extension_ui_responses(), Vec::new());
        assert_eq!(
            state.rows(),
            &[
                DisplayRow::Status("A".to_owned()),
                DisplayRow::Status("B".to_owned())
            ]
        );
    }

    #[test]
    fn unsupported_blocking_extension_ui_requests_fail_closed() {
        let mut state = ZepiPanelState::new();

        state
            .apply_json_line(r#"{"type":"extension_ui_request","id":"abc","method":"select","message":"Pick one"}"#)
            .unwrap();

        assert_eq!(
            state.take_pending_extension_ui_responses(),
            vec![ExtensionUiResponse {
                message_type: "extension_ui_response",
                id: "abc".to_owned(),
                cancelled: true,
            }]
        );
    }
}
