//! Executor library for loading and running `wasix:mcp` compatible Wasm components.
//! Users supply an [`ExecConfig`] describing how to resolve artifacts and what
//! runtime constraints to enforce, then call [`exec`] with a structured request.

mod config;
pub mod describe;
mod error;
mod path_safety;
mod resolve;
pub mod router;
pub mod runner;
mod store;
#[cfg(test)]
mod test_support;
mod verify;

pub use config::{DynSecretsStore, ExecConfig, RuntimePolicy, SecretsStore, VerifyPolicy};
pub use error::{ExecError, RunnerError};
pub use store::{ToolInfo, ToolStore};

use greentic_types::TenantCtx;
use serde_json::{Value, json};

use crate::runner::Runner;

/// One tool advertised by a `wasix:mcp/router` component.
#[derive(Clone, Debug)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// The tool's declared output schema, or `None` when it declares none.
    ///
    /// `None` (undeclared) and `Some(json!({}))` (declared, but empty) are
    /// deliberately distinct: a caller asking "what fields does this tool
    /// return?" needs those two answers to differ, so an absent schema is
    /// never defaulted to an empty object.
    ///
    /// The value is passed through as the tool declared it and is *not*
    /// normalised or validated here — a declaration that is not valid JSON
    /// surfaces verbatim as [`Value::String`], leaving a consumer that cares
    /// free to detect it and warn.
    pub output_schema: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct ExecRequest {
    pub component: String,
    pub action: String,
    pub args: Value,
    pub tenant: Option<TenantCtx>,
}

/// Execute a single action exported by an MCP component.
///
/// Resolution, verification, and runtime enforcement are performed in sequence,
/// with detailed errors surfaced through [`ExecError`].
pub fn exec(req: ExecRequest, cfg: &ExecConfig) -> Result<Value, ExecError> {
    let resolved = resolve::resolve(&req.component, &cfg.store)
        .map_err(|err| ExecError::resolve(&req.component, err))?;

    let verified = verify::verify(&req.component, resolved, &cfg.security)
        .map_err(|err| ExecError::verification(&req.component, err))?;

    let runner = runner::DefaultRunner::new(&cfg.runtime)
        .map_err(|err| ExecError::runner(&req.component, err))?;

    let result = runner.run(
        &req,
        &verified,
        runner::ExecutionContext {
            runtime: &cfg.runtime,
            http_enabled: cfg.http_enabled,
            secrets_store: cfg.secrets_store.clone(),
        },
    );

    let value = match result {
        Ok(v) => v,
        Err(RunnerError::ActionNotFound { .. }) => {
            return Err(ExecError::not_found(
                req.component.clone(),
                req.action.clone(),
            ));
        }
        Err(RunnerError::ToolTransient { component, message }) => {
            return Err(ExecError::tool_error(
                component,
                req.action.clone(),
                "transient",
                json!({ "message": message }),
            ));
        }
        Err(RunnerError::Internal(message)) => {
            return Err(ExecError::runner(
                &req.component,
                RunnerError::Internal(message),
            ));
        }
        Err(err) => return Err(ExecError::runner(&req.component, err)),
    };

    if let Some(code) = value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        if code == "iface-error.not-found" {
            return Err(ExecError::not_found(req.component, req.action));
        } else {
            return Err(ExecError::tool_error(
                req.component,
                req.action,
                code,
                value,
            ));
        }
    }

    Ok(value)
}

/// List the tools a local `wasix:mcp` component exports (the `tools/list`
/// equivalent), resolving and verifying via the same pipeline as [`exec`].
///
/// Synchronous (blocking Wasmtime) — callers on an async runtime must wrap this
/// in `spawn_blocking`.
pub fn list_tools(component: &str, cfg: &ExecConfig) -> Result<Vec<ToolDef>, ExecError> {
    let resolved = resolve::resolve(component, &cfg.store)
        .map_err(|err| ExecError::resolve(component, err))?;
    let verified = verify::verify(component, resolved, &cfg.security)
        .map_err(|err| ExecError::verification(component, err))?;

    let runner = runner::DefaultRunner::new(&cfg.runtime)
        .map_err(|err| ExecError::runner(component, err))?;

    let tools = runner
        .list_tools_router(
            &verified,
            runner::ExecutionContext {
                runtime: &cfg.runtime,
                http_enabled: cfg.http_enabled,
                secrets_store: cfg.secrets_store.clone(),
            },
        )
        .map_err(|err| ExecError::runner(component, err))?
        .unwrap_or_default();

    Ok(tools.into_iter().map(tool_def_from_router).collect())
}

/// Parse a WIT `json` payload (which is just a string) into a [`Value`].
///
/// A payload that is not valid JSON is passed through verbatim as
/// [`Value::String`] rather than being discarded or replaced with a
/// placeholder, so the caller can still see exactly what the tool declared.
/// Mirrors `parse_json_string` in the sibling `greentic-mcp-adapter` crate,
/// keeping the local-wasm and HTTP paths in agreement.
fn parse_json_payload(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Map one `wasix:mcp/router` tool record onto a [`ToolDef`].
///
/// `output-schema` is `option<json>` in the WIT, and that optionality is
/// carried through unchanged: an undeclared schema stays `None` instead of
/// collapsing into an empty object.
fn tool_def_from_router(t: router::Tool) -> ToolDef {
    ToolDef {
        name: t.name,
        description: t.description,
        // Pre-existing behaviour, deliberately left alone: a malformed *input*
        // schema still degrades to `{}`. Changing that is a separate call.
        input_schema: serde_json::from_str(&t.input_schema)
            .unwrap_or_else(|_| serde_json::json!({})),
        output_schema: t.output_schema.as_deref().map(parse_json_payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RuntimePolicy, VerifyPolicy};
    use crate::error::RunnerError;
    use crate::store::ToolStore;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::verify::VerifiedArtifact;

    #[derive(Default)]
    struct MockRunner;

    impl runner::Runner for MockRunner {
        fn run(
            &self,
            request: &ExecRequest,
            artifact: &VerifiedArtifact,
            _ctx: runner::ExecutionContext<'_>,
        ) -> Result<Value, RunnerError> {
            let mut payload = request.args.clone();
            if let Value::Object(map) = &mut payload {
                map.insert(
                    "component_digest".to_string(),
                    Value::String(artifact.resolved.digest.clone()),
                );
            }
            Ok(payload)
        }
    }

    #[test]
    fn local_resolve_and_verify_success() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let wasm_path = tempdir.path().join("echo.component.wasm");
        std::fs::write(&wasm_path, b"fake wasm contents").expect("write");

        let digest = crate::resolve::resolve(
            "echo.component",
            &ToolStore::LocalDir(PathBuf::from(tempdir.path())),
        )
        .expect("resolve")
        .digest;

        let mut required = HashMap::new();
        required.insert("echo.component".to_string(), digest.clone());

        let cfg = ExecConfig {
            store: ToolStore::LocalDir(PathBuf::from(tempdir.path())),
            security: VerifyPolicy {
                allow_unverified: false,
                required_digests: required,
                trusted_signers: Vec::new(),
            },
            runtime: RuntimePolicy::default(),
            http_enabled: false,
            secrets_store: None,
        };

        let req = ExecRequest {
            component: "echo.component".into(),
            action: "noop".into(),
            args: json!({"message": "hello"}),
            tenant: None,
        };

        // Inject our mock runner to exercise pipeline without executing wasm.
        let resolved =
            crate::resolve::resolve(&req.component, &cfg.store).expect("resolve second time");
        let verified =
            crate::verify::verify(&req.component, resolved, &cfg.security).expect("verify");
        let result = MockRunner
            .run(
                &req,
                &verified,
                runner::ExecutionContext {
                    runtime: &cfg.runtime,
                    http_enabled: cfg.http_enabled,
                    secrets_store: cfg.secrets_store.clone(),
                },
            )
            .expect("run");

        assert_eq!(
            result.get("component_digest").and_then(Value::as_str),
            Some(digest.as_str())
        );
    }

    /// Build a router tool record with the given `output-schema` declaration.
    fn router_tool(output_schema: Option<&str>) -> router::Tool {
        router::Tool {
            name: "quote".into(),
            title: None,
            description: "quote a policy".into(),
            input_schema: r#"{"type":"object"}"#.into(),
            output_schema: output_schema.map(str::to_string),
            annotations: None,
            meta: None,
        }
    }

    #[test]
    fn list_tools_mapping_carries_a_declared_output_schema() {
        let def = tool_def_from_router(router_tool(Some(
            r#"{"type":"object","properties":{"annual_premium":{"type":"number"}}}"#,
        )));

        // The whole point of the change: a caller can now discover that this
        // tool returns an `annual_premium` field.
        assert_eq!(
            def.output_schema
                .as_ref()
                .and_then(|s| s.get("properties"))
                .and_then(|p| p.get("annual_premium"))
                .and_then(|f| f.get("type"))
                .and_then(Value::as_str),
            Some("number"),
            "declared output schema reached the ToolDef: {:?}",
            def.output_schema
        );
    }

    #[test]
    fn an_undeclared_output_schema_stays_absent() {
        let def = tool_def_from_router(router_tool(None));
        assert!(
            def.output_schema.is_none(),
            "a tool declaring no output schema must stay None, got {:?}",
            def.output_schema
        );
    }

    #[test]
    fn undeclared_and_declared_empty_output_schemas_are_distinguishable() {
        let undeclared = tool_def_from_router(router_tool(None)).output_schema;
        let declared_empty = tool_def_from_router(router_tool(Some("{}"))).output_schema;

        assert_eq!(undeclared, None);
        assert_eq!(declared_empty, Some(json!({})));
        assert_ne!(
            undeclared, declared_empty,
            "\"declares nothing\" and \"declares an empty schema\" must not collapse \
             into the same answer"
        );
    }

    #[test]
    fn a_malformed_output_schema_is_passed_through_not_normalised() {
        let def = tool_def_from_router(router_tool(Some("not json at all")));

        // Not silently dropped to None (which would read as "declares
        // nothing") and not normalised to {} — the consumer sees the raw
        // declaration and can warn about it.
        assert_eq!(
            def.output_schema,
            Some(Value::String("not json at all".into())),
            "malformed declaration passed through verbatim"
        );
    }

    #[test]
    fn a_declared_output_schema_does_not_disturb_the_input_schema() {
        let def = tool_def_from_router(router_tool(Some(r#"{"type":"object"}"#)));
        assert_eq!(
            def.input_schema,
            json!({"type": "object"}),
            "input schema mapping is unchanged"
        );
        assert_eq!(def.name, "quote");
        assert_eq!(def.description, "quote a policy");
    }
}
