//! The AG-UI event vocabulary (§5.1). Adapters emit these; every layer above
//! transports them unmodified. Types are hand-rolled: the ecosystem SDKs are
//! TS/Python, and the vocabulary is small and stable enough to own. Wire shape
//! follows AG-UI: a SCREAMING_SNAKE `type` tag and camelCase payload fields, so
//! an unmodified AG-UI client parses these directly off the SSE leg.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Namespaced `CUSTOM` event names for Hearth-specific moments.
pub const CUSTOM_PERMISSION_REQUEST: &str = "hearth.permission_request";
pub const CUSTOM_SESSION_NAME: &str = "hearth.session_name";
pub const CUSTOM_STATE: &str = "hearth.state";
pub const CUSTOM_TRUNCATION: &str = "hearth.truncation";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentEvent {
    #[serde(rename = "RUN_STARTED", rename_all = "camelCase")]
    RunStarted { thread_id: String, run_id: String },
    #[serde(rename = "RUN_FINISHED", rename_all = "camelCase")]
    RunFinished {
        thread_id: String,
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    #[serde(rename = "RUN_ERROR", rename_all = "camelCase")]
    RunError {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    #[serde(rename = "STEP_STARTED", rename_all = "camelCase")]
    StepStarted { step_name: String },
    #[serde(rename = "STEP_FINISHED", rename_all = "camelCase")]
    StepFinished { step_name: String },
    #[serde(rename = "TEXT_MESSAGE_START", rename_all = "camelCase")]
    TextMessageStart { message_id: String, role: String },
    #[serde(rename = "TEXT_MESSAGE_CONTENT", rename_all = "camelCase")]
    TextMessageContent { message_id: String, delta: String },
    #[serde(rename = "TEXT_MESSAGE_END", rename_all = "camelCase")]
    TextMessageEnd { message_id: String },
    #[serde(rename = "REASONING_START", rename_all = "camelCase")]
    ReasoningStart { message_id: String },
    #[serde(rename = "REASONING_MESSAGE_START", rename_all = "camelCase")]
    ReasoningMessageStart { message_id: String, role: String },
    #[serde(rename = "REASONING_MESSAGE_CONTENT", rename_all = "camelCase")]
    ReasoningMessageContent { message_id: String, delta: String },
    #[serde(rename = "REASONING_MESSAGE_END", rename_all = "camelCase")]
    ReasoningMessageEnd { message_id: String },
    #[serde(rename = "REASONING_END", rename_all = "camelCase")]
    ReasoningEnd { message_id: String },
    #[serde(rename = "TOOL_CALL_START", rename_all = "camelCase")]
    ToolCallStart {
        tool_call_id: String,
        tool_call_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_message_id: Option<String>,
    },
    #[serde(rename = "TOOL_CALL_ARGS", rename_all = "camelCase")]
    ToolCallArgs { tool_call_id: String, delta: String },
    #[serde(rename = "TOOL_CALL_END", rename_all = "camelCase")]
    ToolCallEnd { tool_call_id: String },
    #[serde(rename = "TOOL_CALL_RESULT", rename_all = "camelCase")]
    ToolCallResult {
        message_id: String,
        tool_call_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
    },
    #[serde(rename = "STATE_SNAPSHOT", rename_all = "camelCase")]
    StateSnapshot { snapshot: Value },
    #[serde(rename = "STATE_DELTA", rename_all = "camelCase")]
    StateDelta { delta: Value },
    #[serde(rename = "MESSAGES_SNAPSHOT", rename_all = "camelCase")]
    MessagesSnapshot { messages: Value },
    #[serde(rename = "RAW", rename_all = "camelCase")]
    Raw {
        event: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    #[serde(rename = "CUSTOM", rename_all = "camelCase")]
    Custom { name: String, value: Value },
}

impl AgentEvent {
    /// The Hearth task-state transition event (`CUSTOM hearth.state`).
    pub fn state_change(state: &crate::task::TaskState, detail: Option<&str>) -> Self {
        AgentEvent::Custom {
            name: CUSTOM_STATE.to_string(),
            value: serde_json::json!({ "state": state, "detail": detail }),
        }
    }

    /// The permission-request event that immediately precedes a run ending
    /// `interrupted` (§3.1).
    pub fn permission_request(prompt: &Value) -> Self {
        AgentEvent::Custom {
            name: CUSTOM_PERMISSION_REQUEST.to_string(),
            value: prompt.clone(),
        }
    }

    /// A durable replacement for the thread's human-readable display name.
    pub fn session_name(name: &str) -> Self {
        AgentEvent::Custom {
            name: CUSTOM_SESSION_NAME.to_string(),
            value: serde_json::json!({ "name": name }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_to_ag_ui_wire_shape() {
        let event = AgentEvent::TextMessageContent {
            message_id: "m1".into(),
            delta: "hi".into(),
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "TEXT_MESSAGE_CONTENT");
        assert_eq!(wire["messageId"], "m1");
        assert_eq!(wire["delta"], "hi");

        let event = AgentEvent::ToolCallStart {
            tool_call_id: "t1".into(),
            tool_call_name: "shell".into(),
            parent_message_id: None,
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "TOOL_CALL_START");
        assert_eq!(wire["toolCallId"], "t1");
        assert_eq!(wire["toolCallName"], "shell");
        assert!(wire.get("parentMessageId").is_none());

        let event = AgentEvent::ReasoningMessageContent {
            message_id: "r1".into(),
            delta: "considering".into(),
        };
        let wire = serde_json::to_value(&event).unwrap();
        assert_eq!(wire["type"], "REASONING_MESSAGE_CONTENT");
        assert_eq!(wire["messageId"], "r1");
        assert_eq!(wire["delta"], "considering");
    }

    #[test]
    fn events_round_trip() {
        let events = vec![
            AgentEvent::RunStarted {
                thread_id: "th".into(),
                run_id: "r1".into(),
            },
            AgentEvent::RunFinished {
                thread_id: "th".into(),
                run_id: "r1".into(),
                result: Some(serde_json::json!({"ok": true})),
            },
            AgentEvent::Custom {
                name: CUSTOM_PERMISSION_REQUEST.into(),
                value: serde_json::json!({"prompt": "rm -rf /tmp/x?"}),
            },
            AgentEvent::session_name("Investigate checkout latency"),
        ];
        for event in events {
            let wire = serde_json::to_string(&event).unwrap();
            let parsed: AgentEvent = serde_json::from_str(&wire).unwrap();
            assert_eq!(parsed, event);
        }
    }
}
