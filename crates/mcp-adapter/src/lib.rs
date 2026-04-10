#![allow(dead_code)]

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "adapter",
        generate_unused_types: true,
        generate_all,
    });
}

use bindings::exports::greentic::component::node::{
    ExecCtx, Guest, InvokeResult, LifecycleStatus, NodeError, StreamEvent,
};
use bindings::wasix::mcp::router;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::panic::{AssertUnwindSafe, catch_unwind};
use thiserror::Error;

const PROTOCOL: &str = "25.06.18";
type AdapterResult<T> = Result<T, Box<ErrorEnvelope>>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdapterRequest {
    operation: Option<String>,
    tool: Option<String>,
    #[serde(default = "default_arguments")]
    arguments: Value,
}

#[derive(Debug)]
enum Operation {
    List,
    Call,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
    status: u16,
    tool: Option<String>,
    protocol: &'static str,
    details: Value,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    ok: bool,
    error: ErrorBody,
}

impl ErrorEnvelope {
    fn node_error(&self) -> NodeError {
        let retryable = self.error.status >= 500;
        let details = serde_json::to_string(self).unwrap_or_else(|_| self.error.message.clone());
        NodeError {
            code: self.error.code.to_string(),
            message: self.error.message.clone(),
            retryable,
            backoff_ms: None,
            details: Some(details),
        }
    }
}

#[derive(Debug, Error)]
enum RouterError {
    #[error("{0}")]
    Transport(String),
}

#[derive(Debug, Error)]
enum CallFailure {
    #[error("tool")]
    Tool(router::ToolError),
    #[error("{0}")]
    Transport(String),
}

trait McpRouter {
    fn list_tools(&self) -> Result<Vec<router::Tool>, RouterError>;
    fn call_tool(&self, tool: &str, arguments: &Value) -> Result<router::Response, CallFailure>;
}

struct WitRouter;

impl McpRouter for WitRouter {
    fn list_tools(&self) -> Result<Vec<router::Tool>, RouterError> {
        catch_unwind(router::list_tools)
            .map_err(|_| RouterError::Transport("router panicked".into()))
    }

    fn call_tool(&self, tool: &str, arguments: &Value) -> Result<router::Response, CallFailure> {
        let args_json = serde_json::to_string(arguments)
            .map_err(|err| CallFailure::Transport(err.to_string()))?;

        let call = catch_unwind(AssertUnwindSafe(|| router::call_tool(tool, &args_json)));

        match call {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(err)) => Err(CallFailure::Tool(err)),
            Err(_) => Err(CallFailure::Transport("router panicked".into())),
        }
    }
}

struct Adapter;

impl Guest for Adapter {
    fn get_manifest() -> String {
        serde_json::to_string(&json!({
            "name": "greentic-mcp-adapter",
            "version": env!("CARGO_PKG_VERSION"),
            "protocol": PROTOCOL,
            "operations": ["list", "call"],
            "description": "MCP adapter template exporting greentic:component/node@0.5.0 and importing wasix:mcp@25.06.18.",
        }))
        .unwrap_or_else(|_| "{}".into())
    }

    fn on_start(_ctx: ExecCtx) -> Result<LifecycleStatus, String> {
        Ok(LifecycleStatus::Ok)
    }

    fn on_stop(_ctx: ExecCtx, _reason: String) -> Result<LifecycleStatus, String> {
        Ok(LifecycleStatus::Ok)
    }

    fn invoke(_ctx: ExecCtx, op: String, input: String) -> InvokeResult {
        match handle_invoke(&WitRouter, &op, &input) {
            Ok(value) => {
                let rendered =
                    serde_json::to_string(&value).unwrap_or_else(|_| "{\"ok\":true}".into());
                InvokeResult::Ok(rendered)
            }
            Err(err) => InvokeResult::Err(err.node_error()),
        }
    }

    fn invoke_stream(ctx: ExecCtx, op: String, input: String) -> Vec<StreamEvent> {
        match Self::invoke(ctx, op, input) {
            InvokeResult::Ok(body) => vec![StreamEvent::Data(body), StreamEvent::Done],
            InvokeResult::Err(err) => {
                let payload = err.details.clone().unwrap_or_else(|| err.message.clone());
                vec![StreamEvent::Error(payload)]
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
bindings::exports::greentic::component::node::__export_greentic_component_node_0_5_0_cabi!(
    Adapter with_types_in bindings::exports::greentic::component::node
);

fn handle_invoke<R: McpRouter>(router: &R, op: &str, input: &str) -> AdapterResult<Value> {
    let request = parse_request(op, input)?;

    match request.operation {
        Operation::List => {
            let tools = router
                .list_tools()
                .map_err(|err| Box::new(transport_error(err, None)))?;
            Ok(render_tool_list(&tools))
        }
        Operation::Call => {
            let tool_name = request.tool.clone().unwrap_or_default();
            let response = router
                .call_tool(&tool_name, &request.arguments)
                .map_err(|err| Box::new(map_call_error(err, &tool_name)))?;

            match response {
                router::Response::Completed(result) => Ok(render_tool_result(&result)),
                router::Response::Elicit(req) => Ok(render_elicitation(&req)),
            }
        }
    }
}

fn parse_request(op: &str, input: &str) -> AdapterResult<ParsedRequest> {
    let parsed: AdapterRequest = serde_json::from_str(input).map_err(|err| {
        Box::new(config_error(
            format!("invalid request payload: {err}"),
            None,
            json!({"raw": input}),
        ))
    })?;

    let operation = resolve_operation(parsed.operation.as_deref(), op, parsed.tool.as_deref())?;
    let arguments_value = parsed.arguments.clone();
    let arguments = ensure_object(parsed.arguments).map_err(|err| {
        Box::new(config_error(
            err,
            parsed.tool.clone(),
            json!({"arguments": arguments_value}),
        ))
    })?;

    Ok(ParsedRequest {
        operation,
        tool: parsed.tool,
        arguments,
    })
}

struct ParsedRequest {
    operation: Operation,
    tool: Option<String>,
    arguments: Value,
}

fn ensure_object(value: Value) -> Result<Value, String> {
    match value {
        Value::Null => Ok(json!({})),
        Value::Object(_) => Ok(value),
        other => Err(format!("arguments must be an object, got {other:?}")),
    }
}

fn resolve_operation(
    from_payload: Option<&str>,
    from_op: &str,
    tool: Option<&str>,
) -> AdapterResult<Operation> {
    let parsed_payload = match from_payload {
        Some(raw) => {
            let op = parse_operation(raw).ok_or_else(|| {
                Box::new(config_error(
                    format!("unsupported operation value: {raw}"),
                    tool.map(|t| t.to_string()),
                    Value::Null,
                ))
            })?;
            Some(op)
        }
        None => None,
    };
    let parsed_op = parse_operation(from_op);

    let op = parsed_payload.or(parsed_op).unwrap_or_else(|| {
        if tool.is_some() {
            Operation::Call
        } else {
            Operation::List
        }
    });

    if matches!(op, Operation::Call) && tool.is_none() {
        return Err(Box::new(config_error(
            "tool is required for operation=call".into(),
            None,
            Value::Null,
        )));
    }

    Ok(op)
}

fn parse_operation(raw: &str) -> Option<Operation> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "list" => Some(Operation::List),
        "call" => Some(Operation::Call),
        _ => None,
    }
}

fn render_tool_list(tools: &[router::Tool]) -> Value {
    let rendered_tools: Vec<Value> = tools.iter().map(render_tool).collect();
    json!({
        "ok": true,
        "result": {
            "tools": rendered_tools,
            "protocol": PROTOCOL,
        }
    })
}

fn render_tool(tool: &router::Tool) -> Value {
    json!({
        "name": tool.name,
        "title": tool.title,
        "description": tool.description,
        "input_schema": parse_json_string(&tool.input_schema),
        "output_schema": tool.output_schema.as_ref().map(|s| parse_json_string(s)),
        "annotations": tool.annotations.as_ref().map(render_tool_annotations),
        "meta": meta_to_value(tool.meta.as_ref()),
    })
}

fn render_tool_annotations(ann: &router::ToolAnnotations) -> Value {
    json!({
        "read_only": ann.read_only,
        "destructive": ann.destructive,
        "streaming": ann.streaming,
        "experimental": ann.experimental,
    })
}

fn render_tool_result(result: &router::ToolResult) -> Value {
    let mut messages = Vec::new();
    let mut result_annotations: Option<Value> = None;
    let content: Vec<Value> = result
        .content
        .iter()
        .map(|block| {
            let (payload, message, annotations) = render_content_block(block);
            if let Some(message) = message {
                messages.push(message);
            }
            if result_annotations.is_none() {
                result_annotations = annotations;
            }
            payload
        })
        .collect();

    let payload = json!({
        "ok": true,
        "result": {
            "content": content,
            "structured_content": result.structured_content.as_ref().map(|s| parse_json_string(s)),
            "progress": result.progress.as_deref().map(render_progress),
            "meta": meta_to_value(result.meta.as_ref()),
            "is_error": result.is_error,
            "annotations": result_annotations,
        },
        "messages": Value::Array(messages),
        "protocol": PROTOCOL,
    });

    payload
}

fn render_progress(progress: &[router::ProgressNotification]) -> Value {
    Value::Array(
        progress
            .iter()
            .map(|p| {
                json!({
                    "progress": p.progress,
                    "message": p.message,
                    "annotations": p.annotations.as_ref().map(render_annotations),
                })
            })
            .collect(),
    )
}

fn render_elicitation(req: &router::ElicitationRequest) -> Value {
    json!({
        "ok": true,
        "elicitation": {
            "title": req.title,
            "message": req.message,
            "schema": parse_json_string(&req.schema),
            "annotations": req.annotations.as_ref().map(render_annotations),
            "meta": meta_to_value(req.meta.as_ref()),
        },
        "messages": [{
            "type": "text",
            "text": req.message,
        }],
        "protocol": PROTOCOL,
    })
}

fn render_content_block(block: &router::ContentBlock) -> (Value, Option<Value>, Option<Value>) {
    match block {
        router::ContentBlock::Text(text) => {
            let payload = json!({
                "type": "text",
                "text": text.text,
                "annotations": text.annotations.as_ref().map(render_annotations),
            });
            let message = json!({
                "type": "text",
                "text": text.text,
            });
            (
                payload,
                Some(message),
                text.annotations.as_ref().map(render_annotations),
            )
        }
        router::ContentBlock::Image(image) => {
            let payload = json!({
                "type": "image",
                "data": image.data,
                "mime_type": image.mime_type,
                "annotations": image.annotations.as_ref().map(render_annotations),
            });
            let message = json!({
                "type": "image",
                "mime_type": image.mime_type,
                "data": image.data,
            });
            (
                payload,
                Some(message),
                image.annotations.as_ref().map(render_annotations),
            )
        }
        router::ContentBlock::Audio(audio) => {
            let payload = json!({
                "type": "audio",
                "data": audio.data,
                "mime_type": audio.mime_type,
                "annotations": audio.annotations.as_ref().map(render_annotations),
            });
            let message = json!({
                "type": "audio",
                "mime_type": audio.mime_type,
                "data": audio.data,
            });
            (
                payload,
                Some(message),
                audio.annotations.as_ref().map(render_annotations),
            )
        }
        router::ContentBlock::ResourceLink(link) => {
            let payload = json!({
                "type": "resource_link",
                "uri": link.uri,
                "title": link.title,
                "description": link.description,
                "mime_type": link.mime_type,
                "annotations": link.annotations.as_ref().map(render_annotations),
            });
            let message = json!({
                "type": "resource_link",
                "uri": link.uri,
                "title": link.title,
                "description": link.description,
            });
            (
                payload,
                Some(message),
                link.annotations.as_ref().map(render_annotations),
            )
        }
        router::ContentBlock::EmbeddedResource(res) => {
            let payload = json!({
                "type": "resource",
                "uri": res.uri,
                "title": res.title,
                "description": res.description,
                "mime_type": res.mime_type,
                "data": res.data,
                "annotations": res.annotations.as_ref().map(render_annotations),
            });
            let message = json!({
                "type": "resource",
                "uri": res.uri,
                "title": res.title,
                "description": res.description,
                "mime_type": res.mime_type,
            });
            (
                payload,
                Some(message),
                res.annotations.as_ref().map(render_annotations),
            )
        }
    }
}

fn render_annotations(ann: &router::Annotations) -> Value {
    json!({
        "audience": ann.audience.as_ref().map(|roles| {
            roles.iter().map(|role| match role {
                router::Role::User => "user",
                router::Role::Assistant => "assistant",
            }).collect::<Vec<_>>()
        }),
        "priority": ann.priority,
        "timestamp": ann.timestamp,
    })
}

fn meta_to_value(meta: Option<&Vec<router::MetaEntry>>) -> Option<Value> {
    meta.map(|entries| {
        let mut map = Map::new();
        for entry in entries {
            map.insert(entry.key.clone(), parse_json_string(&entry.value));
        }
        Value::Object(map)
    })
}

fn parse_json_string(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn transport_error(err: RouterError, tool: Option<String>) -> ErrorEnvelope {
    ErrorEnvelope {
        ok: false,
        error: ErrorBody {
            code: "MCP_ROUTER_ERROR",
            message: err.to_string(),
            status: 502,
            tool,
            protocol: PROTOCOL,
            details: Value::Null,
        },
    }
}

fn map_call_error(err: CallFailure, tool: &str) -> ErrorEnvelope {
    match err {
        CallFailure::Tool(tool_err) => match tool_err {
            router::ToolError::InvalidParameters(msg) => tool_error(400, msg, tool),
            router::ToolError::ExecutionError(msg) => tool_error(500, msg, tool),
            router::ToolError::SchemaError(msg) => tool_error(422, msg, tool),
            router::ToolError::NotFound(msg) => tool_error(404, msg, tool),
        },
        CallFailure::Transport(msg) => {
            transport_error(RouterError::Transport(msg), Some(tool.to_string()))
        }
    }
}

fn tool_error(status: u16, message: String, tool: &str) -> ErrorEnvelope {
    ErrorEnvelope {
        ok: false,
        error: ErrorBody {
            code: "MCP_TOOL_ERROR",
            message,
            status,
            tool: Some(tool.to_string()),
            protocol: PROTOCOL,
            details: Value::Null,
        },
    }
}

fn config_error(message: String, tool: Option<String>, details: Value) -> ErrorEnvelope {
    ErrorEnvelope {
        ok: false,
        error: ErrorBody {
            code: "MCP_CONFIG_ERROR",
            message,
            status: 400,
            tool,
            protocol: PROTOCOL,
            details,
        },
    }
}

fn default_arguments() -> Value {
    json!({})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use wasmtime::component::Linker;
    use wasmtime::{Engine, Store};
    use wasmtime_wasi::{
        ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView, p2::add_to_linker_sync,
    };

    struct MockRouter {
        tools: Vec<router::Tool>,
        response: Option<router::Response>,
    }

    impl McpRouter for MockRouter {
        fn list_tools(&self) -> Result<Vec<router::Tool>, RouterError> {
            Ok(self.tools.clone())
        }

        fn call_tool(
            &self,
            _tool: &str,
            _arguments: &Value,
        ) -> Result<router::Response, CallFailure> {
            self.response
                .clone()
                .ok_or_else(|| CallFailure::Transport("no response".into()))
        }
    }

    fn sample_tool() -> router::Tool {
        router::Tool {
            name: "demo".into(),
            title: Some("Demo".into()),
            description: "Example".into(),
            input_schema: r#"{\"type\":\"object\"}"#.into(),
            output_schema: Some(
                r#"{"type":"object","properties":{"result":{"type":"string"}}}"#.into(),
            ),
            annotations: None,
            meta: None,
        }
    }

    #[test]
    fn list_operation_defaults_without_tool() {
        let router = MockRouter {
            tools: vec![sample_tool()],
            response: None,
        };

        let result =
            handle_invoke(&router, "", r#"{"arguments": {}}"#).expect("list should succeed");

        assert_eq!(result.get("ok"), Some(&Value::Bool(true)));
        let tools = result
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(tools.len(), 1);
    }

    #[test]
    fn call_operation_routes_arguments() {
        let router = MockRouter {
            tools: vec![],
            response: Some(router::Response::Completed(router::ToolResult {
                content: vec![router::ContentBlock::Text(router::TextContent {
                    text: "hi".into(),
                    annotations: None,
                })],
                structured_content: None,
                progress: None,
                meta: None,
                is_error: None,
            })),
        };

        let result = handle_invoke(
            &router,
            "",
            r#"{"operation":"call","tool":"demo","arguments":{"foo":"bar"}}"#,
        )
        .expect("call should succeed");

        assert_eq!(result.get("ok"), Some(&Value::Bool(true)));
        let messages = result
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn call_operation_preserves_typed_arguments() {
        struct AssertArgsRouter {
            expected: Value,
        }

        impl McpRouter for AssertArgsRouter {
            fn list_tools(&self) -> Result<Vec<router::Tool>, RouterError> {
                Ok(vec![])
            }

            fn call_tool(
                &self,
                _tool: &str,
                arguments: &Value,
            ) -> Result<router::Response, CallFailure> {
                if arguments != &self.expected {
                    return Err(CallFailure::Transport(format!(
                        "unexpected arguments: {arguments}"
                    )));
                }

                Ok(router::Response::Completed(router::ToolResult {
                    content: vec![],
                    structured_content: None,
                    progress: None,
                    meta: None,
                    is_error: None,
                }))
            }
        }

        let router = AssertArgsRouter {
            expected: json!({
                "count": 3,
                "active": true,
                "items": ["a", "b"],
                "meta": {"score": 9.5},
            }),
        };

        let result = handle_invoke(
            &router,
            "",
            r#"{"operation":"call","tool":"demo","arguments":{"count":3,"active":true,"items":["a","b"],"meta":{"score":9.5}}}"#,
        )
        .expect("call should succeed");

        assert_eq!(result.get("ok"), Some(&Value::Bool(true)));
    }

    #[test]
    fn tool_error_maps_to_envelope() {
        let _router = MockRouter {
            tools: vec![],
            response: Some(router::Response::Completed(router::ToolResult {
                content: vec![],
                structured_content: None,
                progress: None,
                meta: None,
                is_error: Some(true),
            })),
        };

        let err = map_call_error(
            CallFailure::Tool(router::ToolError::InvalidParameters("bad".into())),
            "demo",
        );
        assert_eq!(err.error.code, "MCP_TOOL_ERROR");
        assert_eq!(err.error.status, 400);
    }

    #[test]
    fn structured_content_and_resource_link_round_trip() {
        let router = MockRouter {
            tools: vec![],
            response: Some(router::Response::Completed(router::ToolResult {
                content: vec![router::ContentBlock::ResourceLink(
                    router::ResourceLinkContent {
                        uri: "https://example.com/doc".into(),
                        title: Some("Doc".into()),
                        description: Some("desc".into()),
                        mime_type: Some("text/html".into()),
                        annotations: None,
                    },
                )],
                structured_content: Some(r#"{"result":42}"#.into()),
                progress: None,
                meta: Some(vec![router::MetaEntry {
                    key: "source".into(),
                    value: r#""demo-router""#.into(),
                }]),
                is_error: None,
            })),
        };

        let result = handle_invoke(
            &router,
            "",
            r#"{"operation":"call","tool":"demo","arguments":{"foo":"bar"}}"#,
        )
        .expect("call should succeed");

        assert_eq!(result.get("ok"), Some(&Value::Bool(true)));
        let structured = result
            .pointer("/result/structured_content/result")
            .cloned()
            .unwrap();
        assert_eq!(structured, json!(42));

        let content = result
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(content.len(), 1);
        assert_eq!(
            content[0].get("type"),
            Some(&Value::String("resource_link".into()))
        );

        let messages = result
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            messages.first().and_then(|m| m.get("type")),
            Some(&Value::String("resource_link".into()))
        );
    }

    mod router_bindings {
        wasmtime::component::bindgen!({
            path: "wit/deps/wasix-mcp-25.6.18",
            world: "mcp-router",
        });
    }
    use router_bindings::exports::wasix::mcp::router as router_exports;

    struct RouterCtx {
        table: ResourceTable,
        ctx: WasiCtx,
    }

    impl RouterCtx {
        fn new() -> Self {
            let mut builder = WasiCtxBuilder::new();
            builder.inherit_stdio();
            builder.inherit_env();
            builder.allow_blocking_current_thread(true);
            Self {
                table: ResourceTable::new(),
                ctx: builder.build(),
            }
        }
    }

    impl WasiView for RouterCtx {
        fn ctx(&mut self) -> WasiCtxView<'_> {
            WasiCtxView {
                ctx: &mut self.ctx,
                table: &mut self.table,
            }
        }
    }

    fn target_installed() -> bool {
        Command::new("rustup")
            .args(["target", "list", "--installed"])
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|list| list.lines().any(|l| l.trim() == "wasm32-wasip2"))
            .unwrap_or(false)
    }

    fn router_echo_wasm_path(crate_dir: &Path) -> PathBuf {
        let artifact = PathBuf::from("wasm32-wasip2/release/router_echo.wasm");

        let target_roots = [
            std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from),
            Some(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join("target"),
            ),
            Some(crate_dir.join("target")),
        ];

        target_roots
            .into_iter()
            .flatten()
            .map(|root| root.join(&artifact))
            .find(|path| path.exists())
            .unwrap_or_else(|| {
                std::env::var_os("CARGO_TARGET_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("../..")
                            .join("target")
                    })
                    .join(artifact)
            })
    }

    fn build_router_echo() -> Option<PathBuf> {
        if !target_installed() {
            eprintln!(
                "Skipping adapter/router composition test; wasm32-wasip2 target not installed"
            );
            return None;
        }

        let crate_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../mcp-exec/tests/router_echo");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "--target", "wasm32-wasip2", "--release"])
            .current_dir(&crate_dir)
            .status();

        match status {
            Ok(status) if status.success() => Some(router_echo_wasm_path(&crate_dir)),
            _ => {
                eprintln!("Skipping adapter/router composition test; router build failed");
                None
            }
        }
    }

    fn map_annotations(ann: Option<router_exports::Annotations>) -> Option<router::Annotations> {
        ann.map(|ann| router::Annotations {
            audience: ann.audience.map(|roles| {
                roles
                    .into_iter()
                    .map(|role| match role {
                        router_exports::Role::User => router::Role::User,
                        router_exports::Role::Assistant => router::Role::Assistant,
                    })
                    .collect()
            }),
            priority: ann.priority,
            timestamp: ann.timestamp,
        })
    }

    fn map_tool_annotations(
        ann: Option<router_exports::ToolAnnotations>,
    ) -> Option<router::ToolAnnotations> {
        ann.map(|ann| router::ToolAnnotations {
            read_only: ann.read_only,
            destructive: ann.destructive,
            streaming: ann.streaming,
            experimental: ann.experimental,
        })
    }

    fn map_meta(entries: Option<Vec<router_exports::MetaEntry>>) -> Option<Vec<router::MetaEntry>> {
        entries.map(|entries| {
            entries
                .into_iter()
                .map(|entry| router::MetaEntry {
                    key: entry.key,
                    value: entry.value,
                })
                .collect()
        })
    }

    fn map_tool(tool: router_exports::Tool) -> router::Tool {
        router::Tool {
            name: tool.name,
            title: tool.title,
            description: tool.description,
            input_schema: tool.input_schema,
            output_schema: tool.output_schema,
            annotations: map_tool_annotations(tool.annotations),
            meta: map_meta(tool.meta),
        }
    }

    fn map_progress(
        items: Option<Vec<router_exports::ProgressNotification>>,
    ) -> Option<Vec<router::ProgressNotification>> {
        items.map(|items| {
            items
                .into_iter()
                .map(|item| router::ProgressNotification {
                    progress: item.progress,
                    message: item.message,
                    annotations: map_annotations(item.annotations),
                })
                .collect()
        })
    }

    fn map_content_block(block: router_exports::ContentBlock) -> router::ContentBlock {
        match block {
            router_exports::ContentBlock::Text(text) => {
                router::ContentBlock::Text(router::TextContent {
                    text: text.text,
                    annotations: map_annotations(text.annotations),
                })
            }
            router_exports::ContentBlock::Image(image) => {
                router::ContentBlock::Image(router::ImageContent {
                    data: image.data,
                    mime_type: image.mime_type,
                    annotations: map_annotations(image.annotations),
                })
            }
            router_exports::ContentBlock::Audio(audio) => {
                router::ContentBlock::Audio(router::AudioContent {
                    data: audio.data,
                    mime_type: audio.mime_type,
                    annotations: map_annotations(audio.annotations),
                })
            }
            router_exports::ContentBlock::ResourceLink(link) => {
                router::ContentBlock::ResourceLink(router::ResourceLinkContent {
                    uri: link.uri,
                    title: link.title,
                    description: link.description,
                    mime_type: link.mime_type,
                    annotations: map_annotations(link.annotations),
                })
            }
            router_exports::ContentBlock::EmbeddedResource(resource) => {
                router::ContentBlock::EmbeddedResource(router::EmbeddedResource {
                    uri: resource.uri,
                    title: resource.title,
                    description: resource.description,
                    mime_type: resource.mime_type,
                    data: resource.data,
                    annotations: map_annotations(resource.annotations),
                })
            }
        }
    }

    fn map_tool_result(result: router_exports::ToolResult) -> router::ToolResult {
        router::ToolResult {
            content: result.content.into_iter().map(map_content_block).collect(),
            structured_content: result.structured_content,
            progress: map_progress(result.progress),
            meta: map_meta(result.meta),
            is_error: result.is_error,
        }
    }

    fn map_elicitation(req: router_exports::ElicitationRequest) -> router::ElicitationRequest {
        router::ElicitationRequest {
            title: req.title,
            message: req.message,
            schema: req.schema,
            annotations: map_annotations(req.annotations),
            meta: map_meta(req.meta),
        }
    }

    fn map_response(response: router_exports::Response) -> router::Response {
        match response {
            router_exports::Response::Completed(result) => {
                router::Response::Completed(map_tool_result(result))
            }
            router_exports::Response::Elicit(req) => router::Response::Elicit(map_elicitation(req)),
        }
    }

    fn map_tool_error(err: router_exports::ToolError) -> router::ToolError {
        match err {
            router_exports::ToolError::InvalidParameters(msg) => {
                router::ToolError::InvalidParameters(msg)
            }
            router_exports::ToolError::ExecutionError(msg) => {
                router::ToolError::ExecutionError(msg)
            }
            router_exports::ToolError::SchemaError(msg) => router::ToolError::SchemaError(msg),
            router_exports::ToolError::NotFound(msg) => router::ToolError::NotFound(msg),
        }
    }

    struct ComponentRouter {
        router: router_bindings::McpRouter,
        store: RefCell<Store<RouterCtx>>,
    }

    impl ComponentRouter {
        fn new(wasm_path: &PathBuf) -> Result<Self, String> {
            let mut config = wasmtime::Config::new();
            config.wasm_component_model(true);
            let engine = Engine::new(&config).map_err(|err| err.to_string())?;
            let component = wasmtime::component::Component::from_file(&engine, wasm_path)
                .map_err(|err| err.to_string())?;

            let mut linker: Linker<RouterCtx> = Linker::new(&engine);
            add_to_linker_sync(&mut linker).map_err(|err| err.to_string())?;

            let mut store = Store::new(&engine, RouterCtx::new());
            let router = router_bindings::McpRouter::instantiate(&mut store, &component, &linker)
                .map_err(|err| err.to_string())?;

            Ok(Self {
                router,
                store: RefCell::new(store),
            })
        }
    }

    impl McpRouter for ComponentRouter {
        fn list_tools(&self) -> Result<Vec<router::Tool>, RouterError> {
            let mut store = self.store.borrow_mut();
            let tools = self
                .router
                .wasix_mcp_router()
                .call_list_tools(&mut *store)
                .map_err(|err| RouterError::Transport(err.to_string()))?;
            Ok(tools.into_iter().map(map_tool).collect())
        }

        fn call_tool(
            &self,
            tool: &str,
            arguments: &Value,
        ) -> Result<router::Response, CallFailure> {
            let mut store = self.store.borrow_mut();
            let args_json = serde_json::to_string(arguments)
                .map_err(|err| CallFailure::Transport(err.to_string()))?;
            let response = self
                .router
                .wasix_mcp_router()
                .call_call_tool(&mut *store, tool, &args_json)
                .map_err(|err| CallFailure::Transport(err.to_string()))?;
            let response = response
                .map_err(map_tool_error)
                .map_err(CallFailure::Tool)?;
            Ok(map_response(response))
        }
    }

    #[test]
    fn adapter_handles_router_echo_component() {
        let Some(wasm_path) = build_router_echo() else {
            return;
        };

        let router = ComponentRouter::new(&wasm_path).expect("router component");

        let list = handle_invoke(&router, "", r#"{"arguments": {}}"#).expect("list should succeed");
        let tools = list
            .pointer("/result/tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(tools.len(), 1);

        let call = handle_invoke(
            &router,
            "",
            r#"{"operation":"call","tool":"echo","arguments":{"hello":"world"}}"#,
        )
        .expect("call should succeed");
        let echoed = call
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(echoed.contains("\"hello\":\"world\""));
    }
}
