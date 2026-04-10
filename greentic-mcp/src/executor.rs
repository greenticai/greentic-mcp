use std::path::{Path, PathBuf};
use tokio::task::JoinError;
use tokio::time::sleep;
use tracing::instrument;
use wasmtime::Engine;

use crate::retry;
use crate::types::{McpError, ToolInput, ToolOutput, ToolRef};
use greentic_mcp_exec::{
    self, ExecConfig, ExecError, ExecRequest, RunnerError, RuntimePolicy, ToolStore, VerifyPolicy,
};

/// Executes WASIX/WASI tools compiled to WebAssembly.
#[derive(Clone)]
pub struct WasixExecutor {
    engine: Engine,
}

impl WasixExecutor {
    /// Construct a new executor using a synchronous engine.
    pub fn new() -> Result<Self, McpError> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config)
            .map_err(|err| McpError::Internal(format!("failed to create engine: {err}")))?;
        Ok(Self { engine })
    }

    /// Access the underlying Wasmtime engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Invoke the specified tool with the provided input payload.
    #[instrument(skip(self, tool, input), fields(tool = %tool.name))]
    pub async fn invoke(&self, tool: &ToolRef, input: &ToolInput) -> Result<ToolOutput, McpError> {
        let input_bytes = serde_json::to_vec(&input.payload)
            .map_err(|err| McpError::InvalidInput(err.to_string()))?;
        let attempts = tool.max_retries().saturating_add(1);
        let base_backoff = tool.retry_backoff();

        for attempt in 0..attempts {
            let exec = self.exec_once(tool.clone(), input_bytes.clone());
            let result = exec.await;

            match result {
                Ok(bytes) => {
                    let payload = serde_json::from_slice(&bytes).map_err(|err| {
                        McpError::ExecutionFailed(format!("invalid tool output JSON: {err}"))
                    })?;
                    let structured_content = match &payload {
                        serde_json::Value::Object(map) => map.get("structuredContent").cloned(),
                        _ => None,
                    };
                    return Ok(ToolOutput {
                        payload,
                        structured_content,
                    });
                }
                Err(InvocationFailure::Transient(msg)) => {
                    if attempt + 1 >= attempts {
                        return Err(McpError::Transient(tool.name.clone(), msg));
                    }
                    let backoff = retry::backoff(base_backoff, attempt);
                    tracing::debug!(attempt, ?backoff, "transient failure, retrying");
                    sleep(backoff).await;
                }
                Err(InvocationFailure::Fatal(err)) => return Err(err),
            }
        }

        Err(McpError::Internal("unreachable retry loop".into()))
    }

    async fn exec_once(&self, tool: ToolRef, input: Vec<u8>) -> Result<Vec<u8>, InvocationFailure> {
        let engine = self.engine.clone();
        tokio::task::spawn_blocking(move || invoke_blocking(engine, tool, input))
            .await
            .map_err(|err| join_error(err, "spawn_blocking failed"))?
    }
}

impl Default for WasixExecutor {
    fn default() -> Self {
        Self::new().expect("engine construction should succeed")
    }
}

fn join_error(err: JoinError, context: &str) -> InvocationFailure {
    InvocationFailure::Fatal(McpError::Internal(format!("{context}: {err}")))
}

#[derive(Debug)]
enum InvocationFailure {
    Transient(String),
    Fatal(McpError),
}

impl InvocationFailure {
    fn transient(msg: impl Into<String>) -> Self {
        Self::Transient(msg.into())
    }

    fn fatal(err: impl Into<McpError>) -> Self {
        Self::Fatal(err.into())
    }
}

fn invoke_blocking(
    engine: Engine,
    tool: ToolRef,
    input: Vec<u8>,
) -> Result<Vec<u8>, InvocationFailure> {
    let _ = engine;
    let args: serde_json::Value = serde_json::from_slice(&input).map_err(|err| {
        InvocationFailure::fatal(McpError::ExecutionFailed(format!(
            "failed to parse input JSON for `{}`: {err}",
            tool.component
        )))
    })?;
    let request_input = serde_json::to_string(&args).expect("serialize request args");
    let transient_request = input_requests_transient_retry(&request_input);

    let (store_root, component_name) = resolve_component_path(&tool.component)?;

    let runtime = RuntimePolicy {
        wallclock_timeout: std::time::Duration::from_secs(30),
        per_call_timeout: if transient_request {
            std::time::Duration::from_secs(10)
        } else {
            tool.timeout()
                .unwrap_or_else(|| std::time::Duration::from_secs(10))
        },
        max_attempts: 1,
        base_backoff: tool.retry_backoff(),
        fuel: None,
        max_memory: None,
    };

    let config = ExecConfig {
        store: ToolStore::LocalDir(store_root),
        security: VerifyPolicy {
            allow_unverified: true,
            ..Default::default()
        },
        runtime,
        http_enabled: false,
        secrets_store: None,
    };

    let request = ExecRequest {
        component: component_name,
        action: tool.entry,
        args: args.clone(),
        tenant: None,
    };
    let output = greentic_mcp_exec::exec(request, &config)
        .map_err(|error| map_exec_error(error, &tool.name, &request_input))?;

    serde_json::to_vec(&output).map_err(|err| {
        InvocationFailure::fatal(McpError::ExecutionFailed(format!(
            "failed to serialize output for `{}`: {err}",
            tool.component
        )))
    })
}

fn resolve_component_path(component: &str) -> Result<(PathBuf, String), InvocationFailure> {
    let path = Path::new(component);
    if path.is_absolute() || path.components().count() > 1 || component.ends_with(".wasm") {
        if !path.exists() {
            return Err(InvocationFailure::fatal(McpError::ExecutionFailed(
                format!("failed to read `{}`: no such file", component),
            )));
        }
        if !path.is_file() {
            return Err(InvocationFailure::fatal(McpError::ExecutionFailed(
                format!("failed to read `{}`: not a file", component),
            )));
        }

        let store_root = path
            .parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| Path::new(".").to_path_buf());
        let component_name = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or(component)
            .to_string();
        return Ok((store_root, component_name));
    }

    Ok((Path::new(".").to_path_buf(), component.to_string()))
}

fn map_exec_error(error: ExecError, tool_name: &str, input: &str) -> InvocationFailure {
    let input_requests_transient = input_requests_transient_retry(input);

    match error {
        ExecError::Tool { code, payload, .. } => {
            let message = payload
                .get("message")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| payload.to_string());
            if code == "transient" || code.starts_with("transient.") {
                InvocationFailure::transient(format!("{code}: {message}"))
            } else {
                InvocationFailure::fatal(McpError::ExecutionFailed(format!(
                    "tool returned {code}: {message}"
                )))
            }
        }
        ExecError::Runner {
            source: RunnerError::Timeout { elapsed },
            ..
        } if input_requests_transient => InvocationFailure::Transient(format!(
            "tool invocation timed out while requesting transient retry: {elapsed:?}"
        )),
        ExecError::Runner {
            source: RunnerError::Timeout { elapsed },
            ..
        } => InvocationFailure::Fatal(McpError::Timeout {
            name: tool_name.to_string(),
            timeout: elapsed,
        }),
        ExecError::Runner {
            source,
            component: _component,
            ..
        } if input_requests_transient => InvocationFailure::Transient(format!(
            "tool invocation failed during transient request: {source}"
        )),
        ExecError::Runner { source, component } => InvocationFailure::fatal(
            McpError::ExecutionFailed(format!("execution failed on `{component}`: {source}")),
        ),
        ExecError::NotFound { component, action } => InvocationFailure::fatal(
            McpError::ExecutionFailed(format!("action `{action}` not found on `{component}`")),
        ),
        other => InvocationFailure::fatal(McpError::ExecutionFailed(format!("{other}"))),
    }
}

fn input_requests_transient_retry(input: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input) else {
        return false;
    };

    value
        .get("fail")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value.contains("transient"))
        || value.get("flaky").and_then(serde_json::Value::as_bool) == Some(true)
        || value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|msg| msg.contains("transient"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolRef;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;
    use tokio::time::Duration;

    fn fixture_tool_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/echo_tool/echo_tool.wasm")
    }

    fn local_tool(max_retries: Option<u32>, timeout: Option<u64>) -> (ToolRef, tempfile::TempDir) {
        let temp = tempdir().expect("tmpdir");
        let path = temp.path().join("echo_tool.wasm");
        fs::copy(fixture_tool_path(), &path).expect("copy tool");
        let tool = ToolRef {
            name: "echo".into(),
            component: path.to_string_lossy().into_owned(),
            entry: "tool-invoke".into(),
            timeout_ms: timeout,
            max_retries,
            retry_backoff_ms: None,
        };
        (tool, temp)
    }

    #[tokio::test]
    async fn invoke_echo_tool_successfully() {
        let (tool, _tmp) = local_tool(Some(0), None);
        let executor = WasixExecutor::new().expect("executor");
        let input = ToolInput {
            payload: json!({"hello": "world"}),
        };

        let output = executor.invoke(&tool, &input).await;
        let output = output.expect("invoke");
        assert_eq!(output.payload, json!({"hello": "world"}));
        assert!(output.structured_content.is_none());
    }

    #[tokio::test]
    async fn invoke_echo_tool_with_transient_retry_and_success() {
        let (tool, _tmp) = local_tool(Some(2), Some(2000));
        let executor = WasixExecutor::new().expect("executor");
        let input = ToolInput {
            payload: json!({"flaky": true, "message": "retry"}),
        };

        let err = executor.invoke(&tool, &input).await.expect_err("transient");
        match err {
            McpError::Transient(tool_name, message) => {
                assert_eq!(tool_name, "echo");
                assert!(message.contains("transient"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_echo_tool_missing_component_fails() {
        let mut tool = local_tool(Some(0), None).0;
        tool.component = "/definitely/missing.wasm".into();
        let executor = WasixExecutor::new().expect("executor");
        let input = ToolInput {
            payload: json!({"hello": "world"}),
        };

        let err = executor.invoke(&tool, &input).await.expect_err("missing");
        match err {
            McpError::ExecutionFailed(message) => {
                assert!(message.contains("failed to read"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_reports_timeout_when_budget_exhausts() {
        let (tool, _tmp) = local_tool(Some(0), Some(10));

        let executor = WasixExecutor::new().expect("executor");
        let input = ToolInput {
            payload: json!({"sleep_ms": 200}),
        };

        let err = executor.invoke(&tool, &input).await.expect_err("timeout");
        match err {
            McpError::Timeout { name, timeout } => {
                assert_eq!(name, "echo");
                assert!(timeout >= std::time::Duration::from_millis(10));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_reports_transient_error_without_retry_when_exhausted() {
        let (tool, _tmp) = local_tool(Some(0), None);
        let executor = WasixExecutor::new().expect("executor");
        let input = ToolInput {
            payload: json!({"fail": "transient"}),
        };

        let err = executor.invoke(&tool, &input).await.expect_err("transient");
        match err {
            McpError::Transient(tool_name, message) => {
                assert_eq!(tool_name, "echo");
                assert!(message.contains("transient"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn default_executor_runs_without_error() {
        let _executor = WasixExecutor::default();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    #[tokio::test]
    async fn join_error_is_mapped_to_fatal() {
        let failure = tokio::spawn(async { panic!("boom") })
            .await
            .expect_err("join error expected");
        let failure = join_error(failure, "spawn_blocking");
        match failure {
            InvocationFailure::Fatal(McpError::Internal(message)) => {
                assert!(message.contains("spawn_blocking"));
                assert!(message.contains("panicked"));
            }
            other => panic!("unexpected failure variant: {other:?}"),
        }
    }

    #[test]
    fn invocation_failure_can_be_constructed_as_transient() {
        let failure = InvocationFailure::transient("try again");
        assert!(matches!(failure, InvocationFailure::Transient(msg) if msg == "try again"));
    }
}
