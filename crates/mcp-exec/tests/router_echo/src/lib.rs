mod bindings {
    wit_bindgen::generate!({
        path: "wit/wasix-mcp-25.6.18",
        world: "mcp-router",
        generate_all,
        generate_unused_types: true,
    });
}

use bindings::exports::wasix::mcp::router::{
    CompletionError, CompletionRequest, CompletionResponse, ContentBlock, GetPromptResult,
    McpResource, MetaEntry, Prompt, PromptError, ReadResourceResult, ResourceError, Response,
    ServerCapabilities, ServerDescription, Tool, ToolAnnotations, ToolError, ToolResult,
    ToolsCapability,
};
use bindings::exports::wasix::mcp::router::{Guest, TextContent};

#[allow(dead_code)]
struct Router;

impl Guest for Router {
    fn name() -> String {
        "router-echo".into()
    }

    fn title() -> Option<String> {
        Some("Router Echo".into())
    }

    fn instructions() -> String {
        "Echo arguments".into()
    }

    fn describe_server() -> ServerDescription {
        ServerDescription {
            name: "router-echo".into(),
            title: Some("router test fixture".into()),
            capabilities: ServerCapabilities {
                prompts: None,
                resources: None,
                tools: Some(ToolsCapability {
                    list_changed: Some(false),
                }),
                completions: None,
            },
            resources: None,
            resource_metadata: None,
            meta: None,
        }
    }

    fn list_tools() -> Vec<Tool> {
        vec![
            // Declares no output schema — pins that the absent case survives
            // the round trip as absent.
            Tool {
                name: "echo".into(),
                title: Some("Echo".into()),
                description: "echo args".into(),
                input_schema: r#"{"type":"object"}"#.into(),
                output_schema: None,
                annotations: Some(ToolAnnotations {
                    read_only: Some(true),
                    destructive: Some(false),
                    streaming: Some(false),
                    experimental: None,
                }),
                meta: None,
            },
            // Declares an output schema — pins that a tool's real field names
            // survive the WIT boundary and reach the host's `ToolDef`.
            Tool {
                name: "quote".into(),
                title: Some("Quote".into()),
                description: "quote a policy".into(),
                input_schema: r#"{"type":"object"}"#.into(),
                output_schema: Some(
                    r#"{"type":"object","properties":{"annual_premium":{"type":"number"}}}"#.into(),
                ),
                annotations: None,
                meta: None,
            },
        ]
    }

    fn call_tool(tool_name: String, arguments: String) -> Result<Response, ToolError> {
        if tool_name != "echo" {
            return Err(ToolError::NotFound(tool_name));
        }

        let block = ContentBlock::Text(TextContent {
            text: arguments.clone(),
            annotations: None,
        });

        Ok(Response::Completed(ToolResult {
            content: vec![block],
            structured_content: None,
            progress: None,
            meta: Some(vec![MetaEntry {
                key: "echo".into(),
                value: "\"ok\"".into(),
            }]),
            is_error: None,
        }))
    }

    fn list_resources() -> Vec<McpResource> {
        Vec::new()
    }

    fn read_resource(_uri: String) -> Result<ReadResourceResult, ResourceError> {
        Err(ResourceError::NotFound("missing".into()))
    }

    fn list_prompts() -> Vec<Prompt> {
        Vec::new()
    }

    fn get_prompt(_prompt_name: String) -> Result<GetPromptResult, PromptError> {
        Err(PromptError::NotFound("missing".into()))
    }

    fn complete(_request: CompletionRequest) -> Result<CompletionResponse, CompletionError> {
        Err(CompletionError::NotFound("disabled".into()))
    }
}

#[cfg(target_arch = "wasm32")]
bindings::export!(Router with_types_in bindings);
