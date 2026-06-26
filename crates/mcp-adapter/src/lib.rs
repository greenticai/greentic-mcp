#![allow(dead_code)]

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "adapter",
        generate_unused_types: true,
        generate_all,
    });

    // The adapter must use a local `generate!` (it imports the non-greentic
    // `wasix:mcp/router` world), so the secrets-store binding can't come from the
    // canonical greentic-interfaces crate without a duplicate component-type
    // section. Re-export it here via a `self::` path; consumers reference
    // `bindings::host_secrets_store`, keeping the raw locally-generated greentic
    // import path (which the canonical-import lint forbids) out of the rest of
    // the code. Only referenced on wasm (the host call); gated to avoid an
    // unused-import warning on native test builds.
    #[cfg(target_arch = "wasm32")]
    pub use self::greentic::secrets_store::secrets_store as host_secrets_store;
}

use bindings::exports::greentic::component::node::{
    Guest, InvocationEnvelope, InvocationResult, NodeError,
};
use bindings::wasix::mcp::router;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::panic::{AssertUnwindSafe, catch_unwind};
use thiserror::Error;

const PROTOCOL: &str = "25.06.18";
type AdapterResult<T> = Result<T, Box<ErrorEnvelope>>;

// Guest telemetry for the OAuth lifecycle. Flows to the host over the
// `greentic:telemetry/logging` WIT import (stdout fallback when absent), the
// same path the generated router uses. No-ops when the `telemetry` feature is
// off.
mod telemetry {
    #[cfg(feature = "telemetry")]
    use greentic_telemetry::wasm_guest::{Field, Level, log as gt_log};

    #[cfg(feature = "telemetry")]
    fn to_fields<'a>(fields: &'a [(&'a str, &'a str)]) -> Vec<Field<'a>> {
        fields
            .iter()
            .map(|(k, v)| Field { key: k, value: v })
            .collect()
    }

    #[cfg(feature = "telemetry")]
    pub fn info(message: &str, fields: &[(&str, &str)]) {
        gt_log(Level::Info, message, &to_fields(fields));
    }
    #[cfg(feature = "telemetry")]
    pub fn error(message: &str, fields: &[(&str, &str)]) {
        gt_log(Level::Error, message, &to_fields(fields));
    }

    #[cfg(not(feature = "telemetry"))]
    pub fn info(_message: &str, _fields: &[(&str, &str)]) {}
    #[cfg(not(feature = "telemetry"))]
    pub fn error(_message: &str, _fields: &[(&str, &str)]) {}
}

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
        let details = serde_json::to_value(self)
            .ok()
            .and_then(|value| serde_cbor::to_vec(&value).ok());
        NodeError {
            code: self.error.code.to_string(),
            message: self.error.message.clone(),
            retryable,
            backoff_ms: None,
            details,
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
    /// Resolve a persisted secret (e.g. an OAuth access token) by key from the
    /// host secret store. Used by the OAuth self-gate so a token persisted in the
    /// store — not just one passed as a call argument — lets the call proceed.
    /// Returns `None` when absent, denied, or the host provides no secret store.
    fn resolve_secret(&self, _key: &str) -> Option<String> {
        None
    }
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

    fn resolve_secret(&self, key: &str) -> Option<String> {
        read_host_secret(key)
    }
}

/// Read a secret from the host secret store over the `greentic:secrets-store`
/// import. Compiled to the real WIT call on wasm; a no-op off-target so native
/// unit/integration tests link without a host (they exercise `resolve_secret`
/// via mock routers instead).
#[cfg(target_arch = "wasm32")]
fn read_host_secret(key: &str) -> Option<String> {
    // Same `greentic:secrets-store` import the generated router uses to inject
    // access_token; re-exported from `mod bindings` (see note there).
    use bindings::host_secrets_store as secrets_store;
    match secrets_store::get(key) {
        Ok(Some(bytes)) => String::from_utf8(bytes)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        _ => None,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn read_host_secret(_key: &str) -> Option<String> {
    None
}

struct Adapter;

impl Guest for Adapter {
    fn invoke(op: String, envelope: InvocationEnvelope) -> Result<InvocationResult, NodeError> {
        let input_json = decode_payload_json(&envelope).map_err(|err| err.node_error())?;
        let value = handle_invoke(&WitRouter, &op, &input_json).map_err(|err| err.node_error())?;
        let output_cbor = serde_cbor::to_vec(&value).map_err(|err| {
            config_error(
                format!("failed to encode output-cbor: {err}"),
                None,
                Value::Null,
            )
            .node_error()
        })?;

        Ok(InvocationResult {
            ok: true,
            output_cbor,
            output_metadata_cbor: None,
        })
    }
}

#[cfg(target_arch = "wasm32")]
bindings::exports::greentic::component::node::__export_greentic_component_node_0_6_0_cabi!(
    Adapter with_types_in bindings::exports::greentic::component::node
);

fn decode_payload_json(envelope: &InvocationEnvelope) -> AdapterResult<String> {
    let payload = if envelope.payload_cbor.is_empty() {
        json!({})
    } else {
        serde_cbor::from_slice::<Value>(&envelope.payload_cbor).map_err(|err| {
            Box::new(config_error(
                format!("invalid payload-cbor: {err}"),
                None,
                Value::Null,
            ))
        })?
    };

    serde_json::to_string(&payload).map_err(|err| {
        Box::new(config_error(
            format!("failed to encode payload as JSON: {err}"),
            None,
            Value::Null,
        ))
    })
}

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
            // OAuth bake-in: if the tool declares OAuth (meta["oauth"]) and no
            // token is supplied, return the sign-in card instead of calling the
            // API. The card rides the node output's `renderedCard`.
            if let Some(card) = oauth_sign_in_card(router, &tool_name, &request.arguments)? {
                return Ok(card);
            }
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

/// OAuth bake-in self-gate: if the named tool carries an `oauth` declaration in
/// its meta and the call has no usable token, render the sign-in card (via the
/// export-free `oauth-card-core`) and return it as the node output. Returns
/// `Ok(None)` when there's no OAuth requirement or a token is already present,
/// so the normal API call proceeds.
fn oauth_sign_in_card<R: McpRouter>(
    router: &R,
    tool_name: &str,
    arguments: &Value,
) -> AdapterResult<Option<Value>> {
    let tools = router
        .list_tools()
        .map_err(|err| Box::new(transport_error(err, Some(tool_name.to_string()))))?;
    let Some(tool) = tools.iter().find(|t| t.name == tool_name) else {
        return Ok(None);
    };
    let Some(meta) = meta_to_value(tool.meta.as_ref()) else {
        return Ok(None);
    };
    let Some(oauth) = meta.get("oauth").filter(|v| v.is_object()) else {
        return Ok(None);
    };

    let token_input = oauth
        .get("token_input")
        .and_then(Value::as_str)
        .unwrap_or("access_token");
    let has_token = arguments
        .get(token_input)
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let provider_id = oauth
        .get("provider_id")
        .and_then(Value::as_str)
        .unwrap_or("oauth-provider");

    if has_token {
        telemetry::info(
            "oauth.token_present",
            &[
                ("tool", tool_name),
                ("provider", provider_id),
                ("source", "argument"),
            ],
        );
        return Ok(None);
    }
    // Not passed as a call argument — check the persisted secret store using the
    // same key the router injects from (its `secret_requirements`). A token there
    // means the router can authenticate the call, so proceed instead of gating.
    if let Some(key) = oauth_secret_key(&meta)
        && router.resolve_secret(&key).is_some()
    {
        telemetry::info(
            "oauth.token_present",
            &[
                ("tool", tool_name),
                ("provider", provider_id),
                ("source", "secret-store"),
            ],
        );
        return Ok(None);
    }
    telemetry::info(
        "oauth.sign_in_required",
        &[("tool", tool_name), ("provider", provider_id)],
    );

    let subject = arguments
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("user");
    let scopes = oauth.get("scopes").cloned().unwrap_or_else(|| json!([]));

    // With a public client_id available (e.g. injected by admin config), drive
    // start-sign-in so the card renders a Connect button; otherwise fall back to
    // a status-card sign-in prompt (no consent URL needed).
    let auth_url = oauth.get("auth_url").and_then(Value::as_str);
    // The OAuth app client_id is provided by the bundle AUTHOR at setup time and
    // stored under the declaration's `setup_fields` client_id key (pack-scoped).
    // Read it from the secret store so the card can render a real Connect button.
    // The client_secret never leaves the host (used only by /oauth/callback).
    let client_id_key = oauth
        .get("setup_fields")
        .and_then(Value::as_array)
        .and_then(|fields| {
            fields.iter().find_map(|f| {
                f.get("key")
                    .and_then(Value::as_str)
                    .filter(|k| k.ends_with("client_id"))
                    .map(str::to_string)
            })
        });
    let client_id = client_id_key
        .as_deref()
        .and_then(|k| router.resolve_secret(k));
    // OpenAPI scheme name (e.g. `githubOAuth`) from the client_id key — carried in
    // `state` so /oauth/callback can resolve the pack-scoped client_secret + token.
    let scheme = client_id_key
        .as_deref()
        .and_then(|k| {
            k.strip_prefix("auth.oauth2.")
                .and_then(|r| r.strip_suffix(".client_id"))
        })
        .unwrap_or(provider_id);
    let mut payload = json!({
        "provider_id": provider_id,
        "subject": subject,
        "scopes": scopes,
    });
    if let (Some(auth_url), Some(client_id)) = (auth_url, client_id.as_deref()) {
        payload["mode"] = json!("start-sign-in");
        payload["auth_url"] = json!(auth_url);
        payload["client_id"] = json!(client_id);
        // Encode the scheme as the OAuth `state` so the host callback can resolve
        // the right pack-scoped secrets. No redirect_uri → the provider uses the
        // OAuth App's registered callback (`/oauth/callback/<provider>`).
        payload["state_id"] = json!(format!("{scheme}|{provider_id}"));
    } else {
        payload["mode"] = json!("status-card");
    }

    let mode = payload
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("status-card");
    let mut card = oauth_card_core::handle_message_json(&payload).map_err(|err| {
        let detail = err.to_string();
        telemetry::error(
            "oauth.card_render_failed",
            &[
                ("tool", tool_name),
                ("provider", provider_id),
                ("error", &detail),
            ],
        );
        Box::new(config_error(
            format!("oauth card render failed: {detail}"),
            Some(tool_name.to_string()),
            Value::Null,
        ))
    })?;
    let status = card
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    telemetry::info(
        "oauth.card_rendered",
        &[
            ("tool", tool_name),
            ("provider", provider_id),
            ("mode", mode),
            ("status", status.as_str()),
        ],
    );
    if let Value::Object(map) = &mut card {
        map.insert("ok".to_string(), Value::Bool(true));
        map.insert("needs_auth".to_string(), Value::Bool(true));
        map.insert("protocol".to_string(), Value::String(PROTOCOL.to_string()));
    }
    Ok(Some(card))
}

/// Pick the secret-store key the OAuth token is persisted under, from the tool's
/// `secret_requirements` meta (emitted by the generator, e.g.
/// `auth.oauth2.<scheme>.access_token`). Prefers a required oauth2/access_token
/// entry; falls back to the first required key.
///
/// TODO(credential-modes): support both ownership models (post-broker). Read
/// `oauth.credential_mode` (default "user"); for "shared" use this base key
/// (one credential per tenant/team), for "user" append the envelope `subject`
/// (per-user key, e.g. `<base>::<subject>`) so each user signs in with their own.
fn oauth_secret_key(meta: &Value) -> Option<String> {
    let reqs = meta.get("secret_requirements")?.as_array()?;
    let is_required = |r: &&Value| r.get("required").and_then(Value::as_bool).unwrap_or(false);
    let key_of = |r: &Value| r.get("key").and_then(Value::as_str).map(str::to_string);
    reqs.iter()
        .filter(is_required)
        .find(|r| {
            r.get("key")
                .and_then(Value::as_str)
                .map(|k| k.contains("oauth2") || k.ends_with("access_token"))
                .unwrap_or(false)
        })
        .and_then(key_of)
        .or_else(|| reqs.iter().find(is_required).and_then(key_of))
}

fn parse_request(op: &str, input: &str) -> AdapterResult<ParsedRequest> {
    let raw_value: Value = serde_json::from_str(input).map_err(|err| {
        Box::new(config_error(
            format!("invalid request payload: {err}"),
            None,
            json!({"raw": input}),
        ))
    })?;
    let mut parsed: AdapterRequest = serde_json::from_value(raw_value.clone()).map_err(|err| {
        Box::new(config_error(
            format!("invalid request payload: {err}"),
            None,
            json!({"raw": input}),
        ))
    })?;

    if should_use_shorthand_payload(&raw_value) {
        // Compatibility shorthand for component.exec:
        // - runner op carries tool id (e.g. "get_weather")
        // - payload is the raw arguments object
        // This lets flows call MCP tools without wrapping every input as
        // {"operation":"call","tool":"...","arguments":{...}}.
        parsed.arguments = raw_value;
    }
    if parsed.tool.is_none() {
        parsed.tool = unknown_operation_as_tool(op);
    }

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

fn should_use_shorthand_payload(value: &Value) -> bool {
    let Value::Object(map) = value else {
        return false;
    };
    !map.contains_key("operation") && !map.contains_key("tool") && !map.contains_key("arguments")
}

fn unknown_operation_as_tool(op: &str) -> Option<String> {
    let trimmed = op.trim();
    if trimmed.is_empty() || parse_operation(trimmed).is_some() {
        return None;
    }
    Some(trimmed.to_string())
}

#[derive(Debug)]
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
    let meta = meta_to_value(tool.meta.as_ref());
    // OAuth bake-in: surface the tool's `oauth` declaration as a first-class field
    // (hoisted from meta) so consumers — designer palette, admin creds wiring —
    // can discover it without parsing the meta blob. Absent for non-OAuth tools.
    let oauth = meta
        .as_ref()
        .and_then(|m| m.get("oauth"))
        .filter(|v| v.is_object())
        .cloned();
    json!({
        "name": tool.name,
        "title": tool.title,
        "description": tool.description,
        "input_schema": parse_json_string(&tool.input_schema),
        "output_schema": tool.output_schema.as_ref().map(|s| parse_json_string(s)),
        "annotations": tool.annotations.as_ref().map(render_tool_annotations),
        "oauth": oauth,
        "meta": meta,
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

    fn oauth_tool(name: &str) -> router::Tool {
        router::Tool {
            name: name.into(),
            title: None,
            description: "needs oauth".into(),
            input_schema: "{}".into(),
            output_schema: None,
            annotations: None,
            meta: Some(vec![
                router::MetaEntry {
                    key: "secret_requirements".into(),
                    value: r#"[{"key":"auth.oauth2.githubOAuth.access_token","required":true,"format":"text"}]"#.into(),
                },
                router::MetaEntry {
                    key: "oauth".into(),
                    value: r#"{"provider_id":"github","auth_url":"https://github.com/login/oauth/authorize","token_url":"https://github.com/login/oauth/access_token","scopes":["repo"],"token_input":"access_token"}"#.into(),
                },
            ]),
        }
    }

    #[test]
    fn oauth_tool_without_token_returns_sign_in_card() {
        let router = MockRouter {
            tools: vec![oauth_tool("list_repos")],
            response: None,
        };
        let result = handle_invoke(
            &router,
            "",
            r#"{"operation":"call","tool":"list_repos","arguments":{}}"#,
        )
        .expect("self-gate should render");
        assert_eq!(result.get("needs_auth"), Some(&Value::Bool(true)));
        assert_eq!(
            result.get("status"),
            Some(&Value::String("needs-sign-in".into()))
        );
        assert_eq!(
            result.pointer("/renderedCard/type"),
            Some(&Value::String("AdaptiveCard".into()))
        );
    }

    #[test]
    fn oauth_tool_with_token_proceeds_to_call() {
        let router = MockRouter {
            tools: vec![oauth_tool("list_repos")],
            response: Some(router::Response::Completed(router::ToolResult {
                content: vec![router::ContentBlock::Text(router::TextContent {
                    text: "repos".into(),
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
            r#"{"operation":"call","tool":"list_repos","arguments":{"access_token":"gho_live"}}"#,
        )
        .expect("call should proceed");
        assert_eq!(result.get("ok"), Some(&Value::Bool(true)));
        assert!(result.get("renderedCard").is_none());
    }

    // Router that holds a persisted token in its secret store but no token in the
    // call arguments — exercises the secret-store branch of the self-gate.
    struct SecretRouter {
        tools: Vec<router::Tool>,
        response: Option<router::Response>,
        secret_key: String,
        secret_value: Option<String>,
    }

    impl McpRouter for SecretRouter {
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
        fn resolve_secret(&self, key: &str) -> Option<String> {
            if key == self.secret_key {
                self.secret_value.clone()
            } else {
                None
            }
        }
    }

    #[test]
    fn oauth_tool_with_persisted_secret_proceeds_to_call() {
        let router = SecretRouter {
            tools: vec![oauth_tool("list_repos")],
            response: Some(router::Response::Completed(router::ToolResult {
                content: vec![router::ContentBlock::Text(router::TextContent {
                    text: "repos".into(),
                    annotations: None,
                })],
                structured_content: None,
                progress: None,
                meta: None,
                is_error: None,
            })),
            secret_key: "auth.oauth2.githubOAuth.access_token".into(),
            secret_value: Some("gho_persisted".into()),
        };
        // No access_token in arguments, but a token is persisted in the store →
        // the gate resolves it and the call proceeds (no sign-in card).
        let result = handle_invoke(
            &router,
            "",
            r#"{"operation":"call","tool":"list_repos","arguments":{}}"#,
        )
        .expect("call should proceed via persisted token");
        assert_eq!(result.get("ok"), Some(&Value::Bool(true)));
        assert!(result.get("renderedCard").is_none());
        assert!(result.get("needs_auth").is_none());
    }

    #[test]
    fn oauth_tool_without_persisted_secret_returns_card() {
        // Same tool, but the store has no token under the key → sign-in card.
        let router = SecretRouter {
            tools: vec![oauth_tool("list_repos")],
            response: None,
            secret_key: "auth.oauth2.githubOAuth.access_token".into(),
            secret_value: None,
        };
        let result = handle_invoke(
            &router,
            "",
            r#"{"operation":"call","tool":"list_repos","arguments":{}}"#,
        )
        .expect("self-gate should render");
        assert_eq!(result.get("needs_auth"), Some(&Value::Bool(true)));
    }

    #[test]
    fn oauth_declaration_is_first_class_in_tool_list() {
        let router = MockRouter {
            tools: vec![oauth_tool("list_repos")],
            response: None,
        };
        let result =
            handle_invoke(&router, "", r#"{"operation":"list"}"#).expect("list should render");
        let tool = result
            .pointer("/result/tools/0")
            .expect("one tool rendered");
        assert_eq!(
            tool.pointer("/oauth/provider_id"),
            Some(&Value::String("github".into()))
        );
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
    fn shorthand_tool_operation_from_runner_op() {
        struct AssertOpAndArgsRouter {
            expected_tool: String,
            expected_args: Value,
        }

        impl McpRouter for AssertOpAndArgsRouter {
            fn list_tools(&self) -> Result<Vec<router::Tool>, RouterError> {
                Ok(vec![])
            }

            fn call_tool(
                &self,
                tool: &str,
                arguments: &Value,
            ) -> Result<router::Response, CallFailure> {
                if tool != self.expected_tool {
                    return Err(CallFailure::Transport(format!("unexpected tool: {tool}")));
                }
                if arguments != &self.expected_args {
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

        let router = AssertOpAndArgsRouter {
            expected_tool: "get_weather".to_string(),
            expected_args: json!({
                "key": "demo",
                "q": "Nairobi",
                "aqi": "no",
            }),
        };

        let result = handle_invoke(
            &router,
            "get_weather",
            r#"{"key":"demo","q":"Nairobi","aqi":"no"}"#,
        )
        .expect("shorthand call should succeed");

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

    #[test]
    fn parse_request_defaults_operation_from_tool() {
        let request =
            parse_request("", r#"{"tool":"demo","arguments":{"v":1}}"#).expect("valid request");
        assert!(matches!(request.operation, Operation::Call));
        assert_eq!(request.tool.as_deref(), Some("demo"));
        assert_eq!(request.arguments, json!({"v":1}));
    }

    #[test]
    fn parse_request_defaults_list_without_tool() {
        let request = parse_request("", r#"{"arguments":null}"#).expect("valid request");
        assert!(matches!(request.operation, Operation::List));
        assert_eq!(request.arguments, Value::Object(Default::default()));
    }

    #[test]
    fn parse_request_rejects_invalid_arguments_payload() {
        let err = parse_request("call", r#"{"tool":"demo","arguments":5}"#)
            .expect_err("invalid arguments");
        assert!(err.error.message.contains("arguments must be an object"));
        assert_eq!(err.error.status, 400);
    }

    #[test]
    fn parse_request_rejects_call_without_tool() {
        let err = parse_request("call", r#"{"arguments":{}}"#).expect_err("missing tool");
        assert_eq!(err.error.code, "MCP_CONFIG_ERROR");
        assert!(err.error.message.contains("tool is required"));
    }

    #[test]
    fn resolve_operation_prefers_payload_operation() {
        let op = resolve_operation(Some("call"), "list", Some("tool")).expect("from payload");
        assert!(matches!(op, Operation::Call));
    }

    #[test]
    fn resolve_operation_defaults_by_tool_presence() {
        let op = resolve_operation(None, "", Some("tool")).expect("tool implies call");
        assert!(matches!(op, Operation::Call));

        let op = resolve_operation(None, "", None).expect("no tool implies list");
        assert!(matches!(op, Operation::List));
    }

    #[test]
    fn resolve_operation_rejects_invalid_payload() {
        let err = resolve_operation(Some("invalid"), "list", None).expect_err("invalid operation");
        assert!(err.error.message.contains("unsupported operation value"));
    }

    #[test]
    fn transport_and_tool_errors_are_mapped() {
        let transport = map_call_error(CallFailure::Transport("down".into()), "demo");
        assert_eq!(transport.error.code, "MCP_ROUTER_ERROR");
        assert_eq!(transport.error.status, 502);

        let tool = map_call_error(
            CallFailure::Tool(router::ToolError::NotFound("missing".into())),
            "demo",
        );
        assert_eq!(tool.error.code, "MCP_TOOL_ERROR");
        assert_eq!(tool.error.status, 404);
    }

    #[test]
    fn tool_result_renders_all_content_blocks() {
        let router = MockRouter {
            tools: vec![],
            response: Some(router::Response::Completed(router::ToolResult {
                content: vec![
                    router::ContentBlock::Text(router::TextContent {
                        text: "txt".into(),
                        annotations: None,
                    }),
                    router::ContentBlock::Image(router::ImageContent {
                        data: "AA==".into(),
                        mime_type: "image/png".into(),
                        annotations: None,
                    }),
                    router::ContentBlock::Audio(router::AudioContent {
                        data: "AQI=".into(),
                        mime_type: "audio/wav".into(),
                        annotations: None,
                    }),
                    router::ContentBlock::ResourceLink(router::ResourceLinkContent {
                        uri: "https://example.com".into(),
                        title: Some("title".into()),
                        description: Some("desc".into()),
                        mime_type: Some("text/plain".into()),
                        annotations: None,
                    }),
                ],
                structured_content: Some(r#"{"ok":true}"#.into()),
                progress: Some(vec![router::ProgressNotification {
                    progress: Some(1.0),
                    message: Some("done".into()),
                    annotations: None,
                }]),
                meta: Some(vec![router::MetaEntry {
                    key: "trace".into(),
                    value: r#""value""#.into(),
                }]),
                is_error: Some(false),
            })),
        };

        let result = handle_invoke(
            &router,
            "",
            r#"{"operation":"call","tool":"demo","arguments":{}}"#,
        )
        .expect("call should succeed");

        let content = result
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(Value::as_array)
            .expect("content");
        assert_eq!(content.len(), 4);

        assert_eq!(
            result
                .get("result")
                .and_then(|r| r.get("structured_content"))
                .and_then(|s| s.get("ok")),
            Some(&Value::Bool(true))
        );

        let messages = result
            .get("messages")
            .and_then(Value::as_array)
            .expect("messages");
        assert_eq!(messages.len(), 4);
    }

    #[test]
    fn guest_invoke_returns_node_error_on_malformed_payload() {
        let envelope = InvocationEnvelope {
            ctx: bindings::exports::greentic::component::node::TenantCtx {
                tenant_id: "tenant".into(),
                team_id: None,
                user_id: None,
                env_id: "env".into(),
                trace_id: "trace".into(),
                correlation_id: "corr".into(),
                deadline_ms: 0,
                attempt: 0,
                idempotency_key: None,
                i18n_id: "en".into(),
            },
            flow_id: "flow".into(),
            step_id: "step".into(),
            component_id: "component".into(),
            attempt: 0,
            payload_cbor: b"not-cbor".to_vec(),
            metadata_cbor: None,
        };

        let err = Adapter::invoke("call".into(), envelope).expect_err("malformed payload");
        assert_eq!(err.code, "MCP_CONFIG_ERROR");
    }

    #[test]
    fn error_envelope_and_tool_error_mapping_is_complete() {
        let transport = map_call_error(CallFailure::Transport("downstream".into()), "demo");
        assert_eq!(transport.error.code, "MCP_ROUTER_ERROR");
        assert_eq!(transport.error.status, 502);

        let invalid = map_call_error(
            CallFailure::Tool(router::ToolError::InvalidParameters("bad".into())),
            "demo",
        );
        assert_eq!(invalid.error.status, 400);
        let execution = map_call_error(
            CallFailure::Tool(router::ToolError::ExecutionError("oops".into())),
            "demo",
        );
        assert_eq!(execution.error.status, 500);
        let schema = map_call_error(
            CallFailure::Tool(router::ToolError::SchemaError("bad schema".into())),
            "demo",
        );
        assert_eq!(schema.error.status, 422);
        let missing = map_call_error(
            CallFailure::Tool(router::ToolError::NotFound("tool".into())),
            "demo",
        );
        assert_eq!(missing.error.status, 404);

        let malformed: ErrorEnvelope = tool_error(400, "oops".into(), "demo");
        let node_error = malformed.node_error();
        assert_eq!(node_error.code, "MCP_TOOL_ERROR");
        assert!(!node_error.retryable);
    }

    #[test]
    fn render_elicitation_and_embedded_resource_are_rendered() {
        let router = MockRouter {
            tools: vec![],
            response: Some(router::Response::Elicit(router::ElicitationRequest {
                title: Some("title".into()),
                message: "message".into(),
                schema: r#"{"type":"object"}"#.into(),
                annotations: Some(router::Annotations {
                    audience: Some(vec![router::Role::User, router::Role::Assistant]),
                    priority: Some(2.0),
                    timestamp: Some("ts".into()),
                }),
                meta: Some(vec![router::MetaEntry {
                    key: "k".into(),
                    value: r#""v""#.into(),
                }]),
            })),
        };
        let value = handle_invoke(
            &router,
            "",
            r#"{"operation":"call","tool":"demo","arguments":{}}"#,
        )
        .expect("elicit");

        let messages = value
            .get("messages")
            .and_then(Value::as_array)
            .expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].get("type"), Some(&json!("text")));
        assert!(
            value
                .get("elicitation")
                .and_then(Value::as_object)
                .is_some()
        );
        let content = [router::ContentBlock::EmbeddedResource(
            router::EmbeddedResource {
                uri: "urn://test".into(),
                title: Some("resource".into()),
                description: Some("desc".into()),
                mime_type: Some("text/plain".into()),
                data: "AA==".into(),
                annotations: Some(router::Annotations {
                    audience: Some(vec![router::Role::Assistant]),
                    priority: None,
                    timestamp: None,
                }),
            },
        )];
        let rendered = render_content_block(&content[0]);
        assert_eq!(rendered.0.get("type"), Some(&json!("resource")));
        assert!(rendered.1.is_some());
    }

    #[test]
    fn export_binding_mapping_functions_are_exercised() {
        let annotations = Some(router_exports::Annotations {
            audience: Some(vec![
                router_exports::Role::User,
                router_exports::Role::Assistant,
            ]),
            priority: Some(3.0),
            timestamp: Some("ts".into()),
        });
        let mapped_annotations = map_annotations(annotations).expect("annotations");
        assert_eq!(
            mapped_annotations.audience,
            Some(vec![router::Role::User, router::Role::Assistant])
        );
        assert_eq!(mapped_annotations.priority, Some(3.0));

        let tool_ann = map_tool_annotations(Some(router_exports::ToolAnnotations {
            read_only: Some(false),
            destructive: Some(false),
            streaming: Some(true),
            experimental: Some(false),
        }));
        assert!(tool_ann.is_some());
        assert_eq!(tool_ann.unwrap().streaming, Some(true));

        let meta = map_meta(Some(vec![router_exports::MetaEntry {
            key: "k".into(),
            value: r#""v""#.into(),
        }]))
        .expect("meta");
        assert_eq!(meta[0].key, "k");

        let tool = map_tool(router_exports::Tool {
            name: "tool".into(),
            title: Some("Tool".into()),
            description: "desc".into(),
            input_schema: "{}".into(),
            output_schema: None,
            annotations: None,
            meta: None,
        });
        assert_eq!(tool.name, "tool");

        let progress = map_progress(Some(vec![router_exports::ProgressNotification {
            progress: Some(0.25),
            message: Some("half".into()),
            annotations: Some(router_exports::Annotations {
                audience: None,
                priority: None,
                timestamp: None,
            }),
        }]))
        .expect("progress");
        assert_eq!(progress[0].message, Some("half".into()));

        let mapped_tool_result = map_tool_result(router_exports::ToolResult {
            content: vec![router_exports::ContentBlock::Text(
                router_exports::TextContent {
                    text: "hello".into(),
                    annotations: None,
                },
            )],
            structured_content: Some(r#"{"x":1}"#.into()),
            progress: None,
            meta: None,
            is_error: Some(false),
        });
        assert_eq!(mapped_tool_result.content.len(), 1);
        assert_eq!(mapped_tool_result.is_error, Some(false));

        let mapped_elicitation = map_elicitation(router_exports::ElicitationRequest {
            title: Some("title".into()),
            message: "message".into(),
            schema: r#"{"type":"object"}"#.into(),
            annotations: None,
            meta: None,
        });
        assert_eq!(mapped_elicitation.title, Some("title".into()));

        assert!(matches!(
            map_response(router_exports::Response::Completed(
                router_exports::ToolResult {
                    content: vec![],
                    structured_content: None,
                    progress: None,
                    meta: None,
                    is_error: None,
                }
            )),
            router::Response::Completed(_)
        ));
        assert!(matches!(
            map_response(router_exports::Response::Elicit(
                router_exports::ElicitationRequest {
                    title: Some("title".into()),
                    message: "message".into(),
                    schema: r#"{}"#.into(),
                    annotations: None,
                    meta: None,
                }
            )),
            router::Response::Elicit(_)
        ));

        assert_eq!(
            map_tool_error(router_exports::ToolError::InvalidParameters("x".into())).to_string(),
            map_tool_error(router_exports::ToolError::InvalidParameters("x".into())).to_string()
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
