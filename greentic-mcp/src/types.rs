use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Reference to a tool stored in the [`ToolMapConfig`](ToolMapConfig).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolRef {
    pub name: String,
    pub component: String,
    pub entry: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_retries: Option<u32>,
    #[serde(default)]
    pub retry_backoff_ms: Option<u64>,
}

impl ToolRef {
    /// Resolve the component path to a [`PathBuf`], if it is a filesystem path.
    pub fn component_path(&self) -> PathBuf {
        PathBuf::from(&self.component)
    }

    /// Timeout duration requested for this tool.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout_ms.map(Duration::from_millis)
    }

    /// Maximum retry attempts for this tool.
    pub fn max_retries(&self) -> u32 {
        self.max_retries.unwrap_or(0)
    }

    /// Base retry backoff in milliseconds.
    pub fn retry_backoff(&self) -> Duration {
        Duration::from_millis(self.retry_backoff_ms.unwrap_or(200))
    }
}

/// Tool map configuration file structure.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolMapConfig {
    pub tools: Vec<ToolRef>,
}

/// Input payload for a tool invocation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolInput {
    pub payload: Value,
}

/// Output payload for a tool invocation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolOutput {
    pub payload: Value,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "structuredContent"
    )]
    pub structured_content: Option<Value>,
}

/// Errors surfaced by the MCP executor.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("tool `{0}` not found")]
    ToolNotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
    #[error("tool `{name}` timed out after {timeout:?}")]
    Timeout { name: String, timeout: Duration },
    #[error("transient failure invoking `{0}`: {1}")]
    Transient(String, String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Config(#[from] serde_yaml_bw::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl McpError {
    pub fn tool_not_found(name: impl Into<String>) -> Self {
        McpError::ToolNotFound(name.into())
    }

    pub fn timeout(name: impl Into<String>, timeout: Duration) -> Self {
        McpError::Timeout {
            name: name.into(),
            timeout,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_tool_ref_fields() {
        let tool = ToolRef {
            name: "echo".into(),
            component: "comp.wasm".into(),
            entry: "tool-invoke".into(),
            timeout_ms: Some(250),
            max_retries: Some(3),
            retry_backoff_ms: Some(50),
        };

        assert_eq!(tool.component_path(), PathBuf::from("comp.wasm"));
        assert_eq!(tool.timeout(), Some(Duration::from_millis(250)));
        assert_eq!(tool.max_retries(), 3);
        assert_eq!(tool.retry_backoff(), Duration::from_millis(50));
    }

    #[test]
    fn defaults_apply_for_optional_tool_fields() {
        let tool = ToolRef {
            name: "echo".into(),
            component: "comp.wasm".into(),
            entry: "tool-invoke".into(),
            timeout_ms: None,
            max_retries: None,
            retry_backoff_ms: None,
        };

        assert!(tool.timeout().is_none());
        assert_eq!(tool.max_retries(), 0);
        assert_eq!(tool.retry_backoff(), Duration::from_millis(200));
    }

    #[test]
    fn tool_output_deserialize_structured_content() {
        let value = json!({
            "payload": {"message": "ok"},
            "structuredContent": {"result": "structured"}
        });

        let output: ToolOutput = serde_json::from_value(value).expect("deserialize");
        assert_eq!(
            output.structured_content.as_ref().map(Value::to_string),
            Some("{\"result\":\"structured\"}".to_string())
        );
        assert_eq!(
            output.payload.get("message").and_then(Value::as_str),
            Some("ok")
        );
    }

    #[test]
    fn error_display_rounds_trips() {
        let err = McpError::tool_not_found("echo");
        assert_eq!(err.to_string(), "tool `echo` not found");

        let err = McpError::InvalidInput("invalid input".into());
        assert_eq!(err.to_string(), "invalid input: invalid input");
    }

    #[test]
    fn captures_structured_content_in_output() {
        let value = json!({
            "payload": {"message": "ok"},
            "structuredContent": {"result": "structured"}
        });

        let output: ToolOutput = serde_json::from_value(value).expect("deserialize");
        assert_eq!(
            output
                .structured_content
                .as_ref()
                .and_then(|v| v.get("result"))
                .and_then(Value::as_str),
            Some("structured")
        );
        assert_eq!(
            output.payload.get("message").and_then(Value::as_str),
            Some("ok")
        );
    }
}
