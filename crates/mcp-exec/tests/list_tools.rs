//! Public `list_tools` over a local `wasix:mcp/router` component (no HTTP).
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use greentic_mcp_exec::ToolStore;
use greentic_mcp_exec::{ExecConfig, RuntimePolicy, VerifyPolicy, list_tools};

fn target_installed() -> bool {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|list| list.lines().any(|l| l.trim() == "wasm32-wasip2"))
        .unwrap_or(false)
}

/// Build the router_echo fixture and return the release output directory,
/// or return None and print a skip message if building is not possible.
fn build_router_echo() -> Option<PathBuf> {
    if !target_installed() {
        eprintln!("skipping: wasm32-wasip2 target not installed");
        return None;
    }

    // The fixture crate lives under tests/router_echo but shares the workspace
    // target directory (two levels up from the crate root).
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/router_echo");
    let workspace_target =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm32-wasip2/release");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--target", "wasm32-wasip2", "--release"])
        .current_dir(&crate_dir)
        .status();

    match status {
        Ok(status) if status.success() => {
            if workspace_target.join("router_echo.wasm").exists() {
                Some(workspace_target)
            } else {
                eprintln!("skipping: router_echo.wasm not found after build");
                None
            }
        }
        _ => {
            eprintln!("skipping: router_echo fixture build failed");
            None
        }
    }
}

#[test]
fn list_tools_returns_router_echo_tools() {
    let Some(dir) = build_router_echo() else {
        return;
    };

    let cfg = ExecConfig {
        store: ToolStore::LocalDir(dir),
        security: VerifyPolicy {
            allow_unverified: true,
            required_digests: HashMap::new(),
            trusted_signers: Vec::new(),
        },
        runtime: RuntimePolicy::default(),
        http_enabled: false,
        secrets_store: None,
    };

    let tools = list_tools("router_echo", &cfg).expect("list_tools ok");
    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "expected an `echo` tool, got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    let echo = tools.iter().find(|t| t.name == "echo").unwrap();
    assert!(
        echo.input_schema.is_object(),
        "schema parsed to JSON object"
    );
}
