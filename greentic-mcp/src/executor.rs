use std::fs;

use tokio::task::JoinError;
use tokio::time::{sleep, timeout};
use tracing::instrument;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store, Trap};
use wasmtime_wasi::p2;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpView};
use wasmtime_wasi_tls::{LinkOptions, WasiTls, WasiTlsCtx, WasiTlsCtxBuilder};

use crate::retry;
use crate::types::{McpError, ToolInput, ToolOutput, ToolRef};

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
        let timeout_duration = tool.timeout();
        let base_backoff = tool.retry_backoff();

        for attempt in 0..attempts {
            let exec = self.exec_once(tool.clone(), input_bytes.clone());
            let result = if let Some(duration) = timeout_duration {
                match timeout(duration, exec).await {
                    Ok(res) => res,
                    Err(_) => return Err(McpError::timeout(&tool.name, duration)),
                }
            } else {
                exec.await
            };

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
    let component_bytes = fs::read(tool.component_path()).map_err(|err| {
        InvocationFailure::fatal(McpError::ExecutionFailed(format!(
            "failed to read `{}`: {err}",
            tool.component
        )))
    })?;
    let component = Component::from_binary(&engine, &component_bytes).map_err(|err| {
        InvocationFailure::fatal(McpError::ExecutionFailed(format!(
            "failed to compile `{}`: {err}",
            tool.component
        )))
    })?;

    let mut linker = Linker::new(&engine);
    p2::add_to_linker_sync(&mut linker).map_err(|err| {
        InvocationFailure::fatal(McpError::Internal(format!(
            "failed to link WASI imports: {err}"
        )))
    })?;

    // Add wasi-tls types and turn on the feature in linker
    let mut opts = LinkOptions::default();
    opts.tls(true);
    wasmtime_wasi_tls::add_to_linker(&mut linker, &mut opts, |h: &mut WasiState| {
        WasiTls::new(&h.wasi_tls_ctx, &mut h.table)
    })
    .map_err(|err| {
        InvocationFailure::fatal(McpError::Internal(format!(
            "failed to link TLS helper: {err}"
        )))
    })?;

    // Add wasi-http types and turn on the feature in linker
    wasmtime_wasi_http::add_only_http_to_linker_sync(&mut linker).map_err(|err| {
        InvocationFailure::fatal(McpError::Internal(format!(
            "failed to link HTTP helper: {err}"
        )))
    })?;

    let pre = linker.instantiate_pre(&component).map_err(|err| {
        InvocationFailure::fatal(McpError::ExecutionFailed(format!(
            "failed to prepare `{}`: {err}",
            tool.component
        )))
    })?;

    let mut store = Store::new(&engine, WasiState::new());
    let instance = pre
        .instantiate(&mut store)
        .map_err(|err| classify(err, &tool))?;

    let func = instance
        .get_typed_func::<(String,), (String,)>(&mut store, &tool.entry)
        .map_err(|err| {
            InvocationFailure::fatal(McpError::ExecutionFailed(format!(
                "missing entry `{}`: {err}",
                tool.entry
            )))
        })?;

    let input_str = String::from_utf8(input).map_err(|err| {
        InvocationFailure::fatal(McpError::InvalidInput(format!(
            "input is not valid UTF-8: {err}"
        )))
    })?;

    let (output,) = func
        .call(&mut store, (input_str,))
        .map_err(|err| classify(err, &tool))?;

    Ok(output.into_bytes())
}

fn classify(err: wasmtime::Error, tool: &ToolRef) -> InvocationFailure {
    if err.downcast_ref::<Trap>().is_some() {
        InvocationFailure::transient(err.to_string())
    } else {
        InvocationFailure::fatal(McpError::ExecutionFailed(format!(
            "tool `{}` failed: {err}",
            tool.name
        )))
    }
}

struct WasiState {
    ctx: WasiCtx,
    wasi_tls_ctx: WasiTlsCtx,
    wasi_http_ctx: WasiHttpCtx,
    table: ResourceTable,
}

impl WasiState {
    fn new() -> Self {
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stdio();
        builder.inherit_env();
        builder.allow_blocking_current_thread(true);
        Self {
            ctx: builder.build(),
            wasi_tls_ctx: WasiTlsCtxBuilder::new().build(),
            wasi_http_ctx: WasiHttpCtx::new(),
            table: ResourceTable::new(),
        }
    }
}

impl WasiView for WasiState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for WasiState {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.wasi_http_ctx
    }

    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}
