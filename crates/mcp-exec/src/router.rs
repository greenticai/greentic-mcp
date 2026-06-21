use base64::Engine;
use serde_json::Value;
use wasmtime::component::Linker;

use crate::runner::StoreState;

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit/wasix-mcp-25.6.18",
        world: "mcp-router",
    });
}

pub use bindings::McpRouter;
pub use bindings::exports::wasix::mcp::router::{
    ContentBlock, Response, Tool, ToolError, ToolResult,
};

pub(crate) fn try_call_tool_router(
    component: &wasmtime::component::Component,
    linker: &mut Linker<StoreState>,
    store: &mut wasmtime::Store<StoreState>,
    tool: &str,
    arguments_json: &String,
) -> anyhow::Result<Option<Value>> {
    let router = match McpRouter::instantiate(&mut *store, component, linker) {
        Ok(router) => router,
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("unknown export")
                || msg.contains("No such export")
                || msg.contains("no exported instance named")
            {
                return Ok(None);
            }
            return Err(anyhow::anyhow!(err.to_string()));
        }
    };

    let response = match router
        .wasix_mcp_router()
        .call_call_tool(&mut *store, tool, arguments_json)
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(err)) => return Ok(Some(tool_error_to_value(tool, err))),
        Err(err) => return Err(anyhow::anyhow!(err.to_string())),
    };

    Ok(Some(render_response(&response)))
}

#[allow(dead_code)]
pub(crate) fn try_list_tools_router(
    component: &wasmtime::component::Component,
    linker: &mut Linker<StoreState>,
    store: &mut wasmtime::Store<StoreState>,
) -> anyhow::Result<Option<Vec<Tool>>> {
    let router = match McpRouter::instantiate(&mut *store, component, linker) {
        Ok(router) => router,
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("unknown export")
                || msg.contains("No such export")
                || msg.contains("no exported instance named")
            {
                return Ok(None);
            }
            return Err(anyhow::anyhow!(err.to_string()));
        }
    };

    let tools = router
        .wasix_mcp_router()
        .call_list_tools(&mut *store)
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    Ok(Some(tools))
}

pub fn render_response(response: &Response) -> Value {
    match response {
        Response::Completed(result) => render_tool_result(result),
        Response::Elicit(req) => serde_json::json!({
            "ok": true,
            "elicitation": {
                "title": req.title.clone(),
                "message": req.message.clone(),
                "schema": req.schema.clone(),
            }
        }),
    }
}

fn render_tool_result(result: &ToolResult) -> Value {
    let (content, _structured_content) = result.content.iter().map(render_content_block).fold(
        (Vec::new(), Vec::new()),
        |mut acc, (c, s)| {
            acc.0.push(c);
            if let Some(s) = s {
                acc.1.push(s);
            }
            acc
        },
    );

    serde_json::json!({
        "ok": true,
        "result": {
            "content": content,
            "structured_content": result
                .structured_content
                .as_ref()
                .map(|raw| serde_json::from_str::<serde_json::Value>(raw).unwrap_or_else(|_| raw.clone().into())),
        }
    })
}

fn render_content_block(block: &ContentBlock) -> (Value, Option<Value>) {
    match block {
        ContentBlock::Text(text) => (serde_json::json!({"type": "text", "text": text.text}), None),
        ContentBlock::Image(img) => (
            serde_json::json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(&img.data), "mime_type": img.mime_type}),
            None,
        ),
        ContentBlock::ResourceLink(link) => (
            serde_json::json!({"type": "resource", "uri": link.uri.clone()}),
            None,
        ),
        ContentBlock::EmbeddedResource(res) => (
            serde_json::json!({"type": "resource-embed", "uri": res.uri.clone(), "data": base64::engine::general_purpose::STANDARD.encode(&res.data)}),
            None,
        ),
        ContentBlock::Audio(audio) => (
            serde_json::json!({"type": "audio", "data": base64::engine::general_purpose::STANDARD.encode(&audio.data), "mime_type": audio.mime_type}),
            None,
        ),
    }
}

pub fn tool_error_to_value(tool: &str, err: ToolError) -> Value {
    let (code, status, message) = match err {
        ToolError::InvalidParameters(msg) => ("MCP_TOOL_ERROR", 400, msg),
        ToolError::ExecutionError(msg) => ("MCP_TOOL_ERROR", 500, msg),
        ToolError::SchemaError(msg) => ("MCP_TOOL_ERROR", 422, msg),
        ToolError::NotFound(msg) => ("MCP_TOOL_ERROR", 404, msg),
    };

    serde_json::json!({
        "ok": false,
        "error": {
            "code": code,
            "message": message,
            "status": status,
            "tool": tool,
            "protocol": "25.06.18",
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::bindings::exports::wasix::mcp::router as generated_router_types;
    use crate::runner::StoreState;
    use wasmtime::Config;
    use wasmtime::Engine;
    use wasmtime::Store;
    use wasmtime::component::Component;
    use wasmtime::component::Linker;
    use wat::parse_str;

    fn empty_router_component() -> (Engine, Component) {
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config).expect("engine");
        Component::from_binary(&engine, &parse_str("(component)").expect("wat"))
            .map(|component| (engine, component))
            .expect("component")
    }

    #[test]
    fn render_response_handles_completed_result() {
        let value = render_response(&Response::Completed(ToolResult {
            content: vec![
                ContentBlock::Text(generated_router_types::TextContent {
                    text: "hello".into(),
                    annotations: Some(generated_router_types::Annotations {
                        audience: Some(vec![generated_router_types::Role::Assistant]),
                        priority: Some(1.0),
                        timestamp: Some("ts".into()),
                    }),
                }),
                ContentBlock::Image(generated_router_types::ImageContent {
                    data: "aGVsbG8=".into(),
                    mime_type: "image/png".into(),
                    annotations: None,
                }),
            ],
            structured_content: Some(r#"{"a":1}"#.into()),
            progress: Some(vec![generated_router_types::ProgressNotification {
                progress: Some(0.5),
                message: Some("half".into()),
                annotations: Some(generated_router_types::Annotations {
                    audience: None,
                    priority: None,
                    timestamp: None,
                }),
            }]),
            meta: Some(vec![generated_router_types::MetaEntry {
                key: "source".into(),
                value: "\"router\"".into(),
            }]),
            is_error: Some(false),
        }));

        assert_eq!(value["ok"], serde_json::json!(true));
        assert_eq!(
            value["result"]["content"]
                .as_array()
                .expect("content")
                .len(),
            2
        );
        assert_eq!(
            value["result"]["structured_content"]["a"],
            serde_json::json!(1)
        );
    }

    #[test]
    fn render_content_maps_all_variants() {
        let image =
            render_content_block(&ContentBlock::Image(generated_router_types::ImageContent {
                data: "AA==".into(),
                mime_type: "image/png".into(),
                annotations: None,
            }));
        assert_eq!(image.0["type"], serde_json::json!("image"));
        assert_eq!(image.1, None);

        let resource = render_content_block(&ContentBlock::ResourceLink(
            generated_router_types::ResourceLinkContent {
                uri: "uri".into(),
                title: None,
                description: None,
                mime_type: Some("text/plain".into()),
                annotations: None,
            },
        ));
        assert_eq!(resource.0["type"], serde_json::json!("resource"));

        let embedded = render_content_block(&ContentBlock::EmbeddedResource(
            generated_router_types::EmbeddedResource {
                uri: "uri".into(),
                title: None,
                description: None,
                mime_type: Some("text/plain".into()),
                data: "".into(),
                annotations: None,
            },
        ));
        assert_eq!(embedded.0["type"], serde_json::json!("resource-embed"));

        let audio =
            render_content_block(&ContentBlock::Audio(generated_router_types::AudioContent {
                data: "AA==".into(),
                mime_type: "audio/wav".into(),
                annotations: None,
            }));
        assert_eq!(audio.0["type"], serde_json::json!("audio"));
    }

    #[test]
    fn maps_tool_errors_to_expected_status_codes() {
        assert_eq!(
            tool_error_to_value("tool", ToolError::InvalidParameters("bad".into()))["error"]["status"],
            400
        );
        assert_eq!(
            tool_error_to_value("tool", ToolError::ExecutionError("oops".into()))["error"]["status"],
            500
        );
        assert_eq!(
            tool_error_to_value("tool", ToolError::SchemaError("bad".into()))["error"]["status"],
            422
        );
        assert_eq!(
            tool_error_to_value("tool", ToolError::NotFound("missing".into()))["error"]["status"],
            404
        );
    }

    #[test]
    fn list_tooling_router_returns_none_when_export_missing() {
        let (engine, component) = empty_router_component();
        let mut linker = Linker::new(&engine);
        let mut store = Store::new(&engine, StoreState::new(false, None, None));
        let result = try_list_tools_router(&component, &mut linker, &mut store).expect("no export");
        assert!(result.is_none());
    }

    #[test]
    fn call_router_returns_none_when_export_missing() {
        let (engine, component) = empty_router_component();
        let mut linker = Linker::new(&engine);
        let mut store = Store::new(&engine, StoreState::new(false, None, None));
        let result = try_call_tool_router(
            &component,
            &mut linker,
            &mut store,
            "tool",
            &"{}".to_string(),
        )
        .expect("no export");
        assert!(result.is_none());
    }
}
