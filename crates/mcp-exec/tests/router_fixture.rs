use std::path::{Path, PathBuf};
use std::process::Command;

use greentic_mcp_exec::{ExecConfig, ExecRequest, RuntimePolicy, ToolStore, VerifyPolicy};
use serde_json::json;
use wasmtime::component::Linker;
use wasmtime::{Engine, Store};
use wasmtime_wasi::{
    ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView, p2::add_to_linker_sync,
};

mod router_bindings {
    wasmtime::component::bindgen!({
        path: "wit/wasix-mcp-25.6.18",
        world: "mcp-router",
    });
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
        Some(crate_dir.join("target")),
        Some(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target"),
        ),
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
                        .join("..")
                        .join("..")
                        .join("target")
                })
                .join(artifact)
        })
}

fn build_router_echo() -> Option<PathBuf> {
    if !target_installed() {
        eprintln!("Skipping router fixture test; wasm32-wasip2 target not installed");
        return None;
    }

    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/router_echo");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--target", "wasm32-wasip2", "--release"])
        .current_dir(&crate_dir)
        .status();

    match status {
        Ok(status) if status.success() => {
            let path = router_echo_wasm_path(&crate_dir);
            if path.exists() {
                Some(path)
            } else {
                eprintln!(
                    "Skipping router fixture test; built artifact missing: {}",
                    path.display()
                );
                None
            }
        }
        _ => {
            eprintln!("Skipping router fixture test; build failed");
            None
        }
    }
}

#[test]
fn router_executes_echo_tool() {
    let Some(wasm_path) = build_router_echo() else {
        return;
    };

    let dir = wasm_path
        .parent()
        .expect("wasm parent exists")
        .to_path_buf();

    let cfg = ExecConfig {
        store: ToolStore::LocalDir(dir),
        security: VerifyPolicy {
            allow_unverified: true,
            ..Default::default()
        },
        runtime: RuntimePolicy::default(),
        http_enabled: false,
        secrets_store: None,
    };

    let req = ExecRequest {
        component: "router_echo".into(),
        action: "echo".into(),
        args: json!({"text": "hi"}),
        tenant: None,
    };

    let value = greentic_mcp_exec::exec(req, &cfg).expect("router call succeeds");
    assert!(value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
    let text = value
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(text.contains("hi"), "expected echoed text, got {text}");
}

#[test]
fn router_lists_tools() {
    let Some(wasm_path) = build_router_echo() else {
        return;
    };

    let mut config = wasmtime::Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config).expect("engine");
    let component =
        wasmtime::component::Component::from_file(&engine, &wasm_path).expect("component");

    let mut linker: Linker<RouterCtx> = Linker::new(&engine);
    add_to_linker_sync(&mut linker).expect("link wasi");

    let mut store = Store::new(&engine, RouterCtx::new());
    let router =
        router_bindings::McpRouter::instantiate(&mut store, &component, &linker).expect("router");
    let tools = router
        .wasix_mcp_router()
        .call_list_tools(&mut store)
        .expect("list tools");
    let names: Vec<_> = tools.iter().map(|t| t.name.clone()).collect();
    assert!(names.contains(&"echo".to_string()));
}

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
