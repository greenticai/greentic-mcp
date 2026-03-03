//! Runtime integration with Wasmtime for invoking the MCP component entrypoint.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Instant;

use greentic_interfaces_wasmtime::host_helpers::v1::{runner_host_http, runner_host_kv};
use greentic_types::TenantCtx;
use serde_json::Value;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{
    ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
    p2::add_to_linker_sync as add_wasi_to_linker,
};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpView};
use wasmtime_wasi_tls::{LinkOptions, WasiTls, WasiTlsCtx, WasiTlsCtxBuilder};

use crate::ExecRequest;
use crate::config::{DynSecretsStore, RuntimePolicy};
use crate::error::RunnerError;
use crate::router::try_call_tool_router;
use crate::verify::VerifiedArtifact;

const LEGACY_EXEC_INTERFACE: &str = "legacy:exec/exec";
type LegacyExecFunc = wasmtime::component::TypedFunc<(String, String), (String,)>;
pub struct ExecutionContext<'a> {
    pub runtime: &'a RuntimePolicy,
    pub http_enabled: bool,
    pub secrets_store: Option<DynSecretsStore>,
}

pub trait Runner: Send + Sync {
    fn run(
        &self,
        request: &ExecRequest,
        artifact: &VerifiedArtifact,
        ctx: ExecutionContext<'_>,
    ) -> Result<Value, RunnerError>;
}

pub struct DefaultRunner {
    engine: Engine,
}

impl DefaultRunner {
    pub fn new(runtime: &RuntimePolicy) -> Result<Self, RunnerError> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        // Epoch interruption lets us wire wallclock enforcement without embedding async support.
        config.epoch_interruption(true);
        if runtime.fuel.is_some() {
            config.consume_fuel(true);
        }
        let engine = Engine::new(&config)?;
        Ok(Self { engine })
    }
}

impl Runner for DefaultRunner {
    fn run(
        &self,
        request: &ExecRequest,
        artifact: &VerifiedArtifact,
        ctx: ExecutionContext<'_>,
    ) -> Result<Value, RunnerError> {
        let engine = self.engine.clone();
        let request = request.clone();
        let artifact = artifact.clone();
        let runtime = ctx.runtime.clone();
        let http_enabled = ctx.http_enabled;
        let secrets_store = ctx.secrets_store.clone();
        let timeout_duration = runtime.per_call_timeout;

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let res = run_sync(
                engine,
                request,
                artifact,
                runtime,
                http_enabled,
                secrets_store,
            );
            let _ = tx.send(res);
        });

        match rx.recv_timeout(timeout_duration) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => Err(RunnerError::Timeout {
                elapsed: timeout_duration,
            }),
            Err(RecvTimeoutError::Disconnected) => {
                Err(RunnerError::Internal("blocking runner task failed".into()))
            }
        }
    }
}

fn run_sync(
    engine: Engine,
    request: ExecRequest,
    artifact: VerifiedArtifact,
    runtime: RuntimePolicy,
    http_enabled: bool,
    secrets_store: Option<DynSecretsStore>,
) -> Result<Value, RunnerError> {
    let component = match Component::from_binary(&engine, artifact.resolved.bytes.as_ref()) {
        Ok(component) => component,
        Err(err) => {
            if let Some(result) = try_mock_json(artifact.resolved.bytes.as_ref(), &request.action) {
                return result;
            }
            return Err(err.into());
        }
    };

    let mut linker = Linker::new(&engine);
    linker.allow_shadowing(true);
    add_wasi_to_linker(&mut linker).map_err(|err| RunnerError::Internal(err.to_string()))?;

    // Add wasi-tls types and turn on the feature in linker
    let mut opts = LinkOptions::default();
    opts.tls(true);
    wasmtime_wasi_tls::add_to_linker(&mut linker, &mut opts, |h: &mut StoreState| h.wasi_tls())?;

    // Add wasi-http types and turn on the feature in linker
    wasmtime_wasi_http::add_only_http_to_linker_sync(&mut linker)?;

    runner_host_http::add_runner_host_http_to_linker(&mut linker, |state: &mut StoreState| state)
        .map_err(|err| RunnerError::Internal(err.to_string()))?;
    runner_host_kv::add_runner_host_kv_to_linker(&mut linker, |state: &mut StoreState| state)
        .map_err(|err| RunnerError::Internal(err.to_string()))?;
    add_secrets_to_linker(&mut linker)?;

    let mut store = Store::new(
        &engine,
        StoreState::new(http_enabled, secrets_store, request.tenant.clone()),
    );
    // Epoch interruption requires an explicit deadline; set a far future deadline
    // until a caller opts into tighter wallclock control.
    store.set_epoch_deadline(u64::MAX / 2);

    let args_json = serde_json::to_string(&request.args)?;
    if let Some(value) = try_call_tool_router(
        &component,
        &mut linker,
        &mut store,
        &request.action,
        &args_json,
    )
    .map_err(|e| RunnerError::Internal(e.to_string()))?
    {
        return Ok(value);
    }

    let instance = linker.instantiate(&mut store, &component)?;
    let exec = if let Some(func) = legacy_exec_func(&instance, &mut store)? {
        func
    } else {
        instance.get_typed_func::<(String, String), (String,)>(&mut store, "exec")?
    };

    let started = Instant::now();
    let (raw_response,) = match exec.call(&mut store, (request.action.clone(), args_json)) {
        Ok(result) => result,
        Err(trap) => {
            let msg = trap.to_string();
            if msg.contains("transient.") {
                return Err(RunnerError::ToolTransient {
                    component: request.component.clone(),
                    message: msg,
                });
            }
            return Err(RunnerError::Internal(msg));
        }
    };

    if started.elapsed() > runtime.wallclock_timeout {
        return Err(RunnerError::Timeout {
            elapsed: started.elapsed(),
        });
    }

    let value: Value = serde_json::from_str(&raw_response)?;
    Ok(value)
}

fn legacy_exec_func(
    instance: &wasmtime::component::Instance,
    store: &mut Store<StoreState>,
) -> Result<Option<LegacyExecFunc>, RunnerError> {
    let Some(interface_index) = instance.get_export_index(&mut *store, None, LEGACY_EXEC_INTERFACE)
    else {
        return Ok(None);
    };
    let Some(func_index) = instance.get_export_index(&mut *store, Some(&interface_index), "exec")
    else {
        return Ok(None);
    };
    let func = instance.get_typed_func::<(String, String), (String,)>(&mut *store, &func_index)?;
    Ok(Some(func))
}

pub struct StoreState {
    http_enabled: bool,
    http_client: Option<reqwest::blocking::Client>,
    secrets_store: Option<DynSecretsStore>,
    tenant: Option<TenantCtx>,
    table: ResourceTable,
    wasi_ctx: WasiCtx,
    wasi_tls_ctx: WasiTlsCtx,
    wasi_http_ctx: WasiHttpCtx,
}

// The Wasmtime store is confined to a single worker thread for each execution.
unsafe impl Send for StoreState {}
unsafe impl Sync for StoreState {}

impl StoreState {
    pub fn new(
        http_enabled: bool,
        secrets_store: Option<DynSecretsStore>,
        tenant: Option<greentic_types::TenantCtx>,
    ) -> Self {
        let mut builder = WasiCtxBuilder::new();
        builder.inherit_stdio().inherit_env();
        if http_enabled {
            builder.inherit_network().allow_ip_name_lookup(true);
        }
        let wasi_ctx = builder.build();
        let wasi_tls_ctx = WasiTlsCtxBuilder::new().build();
        let wasi_http_ctx = WasiHttpCtx::new();
        Self {
            http_enabled,
            http_client: None,
            secrets_store,
            tenant,
            table: ResourceTable::new(),
            wasi_ctx,
            wasi_tls_ctx,
            wasi_http_ctx,
        }
    }

    pub fn table_mut(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    pub fn wasi_tls(&mut self) -> WasiTls<'_> {
        WasiTls::new(&self.wasi_tls_ctx, &mut self.table)
    }

    pub fn wasi_http_ctx_mut(&mut self) -> &mut WasiHttpCtx {
        &mut self.wasi_http_ctx
    }

    fn http_client(&mut self) -> Result<&reqwest::blocking::Client, String> {
        if !self.http_enabled {
            return Err("http-disabled".into());
        }

        if self.http_client.is_none() {
            // Lazily construct a blocking client so hosts that never expose
            // outbound HTTP do not pay the initialization cost.
            let client = reqwest::blocking::Client::builder()
                .use_rustls_tls()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|err| format!("http-client: {err}"))?;
            self.http_client = Some(client);
        }

        Ok(self.http_client.as_ref().expect("client initialized"))
    }

    fn secrets_read(&self, name: String) -> Result<Vec<u8>, String> {
        let store = self
            .secrets_store
            .as_ref()
            .ok_or_else(|| HostError::unavailable("no secrets store configured").to_wire_error())?;
        let tenant = self
            .tenant
            .as_ref()
            .ok_or_else(|| HostError::missing_ctx().to_wire_error())?;
        store
            .read(tenant, &name)
            .map_err(HostError::from)
            .map_err(|err| err.to_wire_error())
    }

    fn secrets_write(&self, name: String, bytes: Vec<u8>) -> Result<(), String> {
        let store = self
            .secrets_store
            .as_ref()
            .ok_or_else(|| HostError::unavailable("no secrets store configured").to_wire_error())?;
        let tenant = self
            .tenant
            .as_ref()
            .ok_or_else(|| HostError::missing_ctx().to_wire_error())?;
        store
            .write(tenant, &name, &bytes)
            .map_err(HostError::from)
            .map_err(|err| err.to_wire_error())
    }

    fn secrets_delete(&self, name: String) -> Result<(), String> {
        let store = self
            .secrets_store
            .as_ref()
            .ok_or_else(|| HostError::unavailable("no secrets store configured").to_wire_error())?;
        let tenant = self
            .tenant
            .as_ref()
            .ok_or_else(|| HostError::missing_ctx().to_wire_error())?;
        store
            .delete(tenant, &name)
            .map_err(HostError::from)
            .map_err(|err| err.to_wire_error())
    }
}

impl StoreState {
    fn http_request(
        &mut self,
        method: String,
        url: String,
        headers: Vec<String>,
        body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, String> {
        if !self.http_enabled {
            return Err("http-disabled".into());
        }

        use reqwest::Method;

        let client = self.http_client()?;
        let method =
            Method::from_bytes(method.as_bytes()).map_err(|_| "invalid-method".to_string())?;

        let builder = client.request(method, &url);
        let mut builder = apply_headers(builder, &headers)?;

        if let Some(body) = body {
            builder = builder.body(body);
        }

        let response = builder.send().map_err(|err| format!("request: {err}"))?;

        if !response.status().is_success() {
            return Err(format!("status-{}", response.status().as_u16()));
        }

        response
            .bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(|err| format!("body: {err}"))
    }

    fn kv_get(&mut self, _ns: String, _key: String) -> Option<String> {
        None
    }

    fn kv_put(&mut self, _ns: String, _key: String, _val: String) {}
}

impl runner_host_http::RunnerHostHttp for StoreState {
    fn request(
        &mut self,
        method: wasmtime::component::__internal::String,
        url: wasmtime::component::__internal::String,
        headers: wasmtime::component::__internal::Vec<wasmtime::component::__internal::String>,
        body: Option<wasmtime::component::__internal::Vec<u8>>,
    ) -> std::result::Result<
        wasmtime::component::__internal::Vec<u8>,
        wasmtime::component::__internal::String,
    > {
        let headers = headers.into_iter().collect();
        self.http_request(method, url, headers, body)
    }
}

impl runner_host_kv::RunnerHostKv for StoreState {
    fn get(
        &mut self,
        ns: wasmtime::component::__internal::String,
        key: wasmtime::component::__internal::String,
    ) -> Option<wasmtime::component::__internal::String> {
        self.kv_get(ns, key)
    }

    fn put(
        &mut self,
        ns: wasmtime::component::__internal::String,
        key: wasmtime::component::__internal::String,
        val: wasmtime::component::__internal::String,
    ) {
        self.kv_put(ns.to_string(), key.to_string(), val.to_string());
    }
}

impl WasiView for StoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for StoreState {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.wasi_http_ctx
    }

    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

fn apply_headers(
    mut builder: reqwest::blocking::RequestBuilder,
    headers: &[String],
) -> Result<reqwest::blocking::RequestBuilder, String> {
    use reqwest::header::{HeaderName, HeaderValue};

    for header in headers {
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| format!("invalid-header:{header}"))?;
        let header_name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| format!("invalid-header-name:{}", name.trim()))?;
        let header_value = HeaderValue::from_str(value.trim())
            .map_err(|_| format!("invalid-header-value:{header}"))?;
        builder = builder.header(header_name, header_value);
    }

    Ok(builder)
}

pub fn add_secrets_to_linker(linker: &mut Linker<StoreState>) -> wasmtime::Result<()> {
    let mut secrets = linker.instance("greentic:secrets/secret-store@1.0.0")?;
    secrets.func_wrap(
        "read",
        |mut caller: wasmtime::StoreContextMut<'_, StoreState>, (name,): (String,)| {
            Ok((caller.data_mut().secrets_read(name),))
        },
    )?;
    secrets.func_wrap(
        "write",
        |mut caller: wasmtime::StoreContextMut<'_, StoreState>,
         (name, bytes): (String, Vec<u8>)| {
            Ok((caller.data_mut().secrets_write(name, bytes),))
        },
    )?;
    secrets.func_wrap(
        "delete",
        |mut caller: wasmtime::StoreContextMut<'_, StoreState>, (name,): (String,)| {
            Ok((caller.data_mut().secrets_delete(name),))
        },
    )?;
    Ok(())
}

#[derive(Clone, Debug)]
struct HostError {
    code: String,
    message: String,
}

impl HostError {
    fn unavailable(message: &str) -> Self {
        Self {
            code: "secrets-unavailable".into(),
            message: message.to_string(),
        }
    }

    fn missing_ctx() -> Self {
        Self {
            code: "missing-tenant-ctx".into(),
            message: "tenant context is required to access secrets".into(),
        }
    }
}

impl From<String> for HostError {
    fn from(message: String) -> Self {
        Self {
            code: "secrets-error".into(),
            message,
        }
    }
}

impl HostError {
    fn to_wire_error(&self) -> String {
        format!("{}:{}", self.code, self.message)
    }
}

fn try_mock_json(bytes: &[u8], action: &str) -> Option<Result<Value, RunnerError>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let root: Value = serde_json::from_str(text).ok()?;

    if !root
        .get("_mock_mcp_exec")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    let responses = root.get("responses")?.as_object()?;
    match responses.get(action) {
        Some(value) => Some(Ok(value.clone())),
        None => Some(Err(RunnerError::ActionNotFound {
            action: action.to_string(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RuntimePolicy, SecretsStore};
    use greentic_types::{EnvId, TenantCtx, TenantId};
    use std::sync::{Arc, Mutex};
    use wasmtime::component::Component;

    #[derive(Default)]
    struct MockSecretsStore {
        last: Mutex<Option<(String, String)>>,
    }

    impl SecretsStore for MockSecretsStore {
        fn read(&self, scope: &TenantCtx, name: &str) -> Result<Vec<u8>, String> {
            self.last
                .lock()
                .unwrap()
                .replace((scope.env.as_str().to_string(), name.to_string()));
            Ok(b"ok".to_vec())
        }
    }

    #[test]
    fn http_request_requires_flag() {
        let mut state = StoreState::new(false, None, None);
        let result =
            state.http_request("GET".into(), "https://example.com".into(), Vec::new(), None);
        assert!(matches!(result, Err(err) if err == "http-disabled"));
    }

    #[test]
    fn http_request_rejects_invalid_method() {
        let mut state = StoreState::new(true, None, None);
        let result =
            state.http_request("???".into(), "https://example.com".into(), Vec::new(), None);
        assert!(matches!(result, Err(err) if err == "invalid-method"));
    }

    #[test]
    fn secrets_read_fails_without_store() {
        let tenant = TenantCtx::new(EnvId("dev".into()), TenantId("acme".into()));
        let state = StoreState::new(true, None, Some(tenant));
        let err = state
            .secrets_read("api-key".into())
            .expect_err("should fail");
        assert!(
            err.starts_with("secrets-unavailable"),
            "expected code in error string, got {err}"
        );
    }

    #[test]
    fn secrets_read_uses_scope() {
        let store = Arc::new(MockSecretsStore::default());
        let tenant = TenantCtx::new(EnvId("dev".into()), TenantId("acme".into()));
        let state = StoreState::new(true, Some(store.clone()), Some(tenant));
        let bytes = state.secrets_read("api-key".into()).expect("read ok");
        assert_eq!(bytes, b"ok");
        let last = store.last.lock().unwrap().clone().expect("called");
        assert_eq!(last.0, "dev");
        assert_eq!(last.1, "api-key");
    }

    #[test]
    fn links_preview2_wasi_imports() {
        let wasm = wat::parse_str(
            r#"(component
                (import "wasi:clocks/monotonic-clock@0.2.0" (instance
                  (export "now" (func (result u64)))
                )))"#,
        )
        .expect("wat should parse");

        let runner = DefaultRunner::new(&RuntimePolicy::default()).expect("runner config");
        let engine = runner.engine.clone();
        let component = Component::from_binary(&engine, &wasm).expect("component should compile");

        let mut linker = Linker::new(&engine);
        linker.allow_shadowing(true);
        add_wasi_to_linker(&mut linker).expect("add preview2 imports");
        runner_host_http::add_runner_host_http_to_linker(&mut linker, |state: &mut StoreState| {
            state
        })
        .expect("runner host http linking");
        runner_host_kv::add_runner_host_kv_to_linker(&mut linker, |state: &mut StoreState| state)
            .expect("runner host kv linking");
        add_secrets_to_linker(&mut linker).expect("secrets linking");

        let mut store = Store::new(&engine, StoreState::new(false, None, None));
        linker
            .instantiate(&mut store, &component)
            .expect("instantiate with preview2 imports");
    }
}
