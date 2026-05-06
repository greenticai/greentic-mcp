use anyhow::{Context, Result, anyhow};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

const ADAPTER_COMPONENT: &[u8] = include_bytes!("../assets/mcp_adapter_25_06_18.component.wasm");

pub const ADAPTER_PROTOCOL: &str = "25.06.18";

pub fn compose_router_with_bundled_adapter(
    router: &Path,
    output: &Path,
    wasm_tools: Option<&Path>,
) -> Result<()> {
    if !router.exists() {
        return Err(anyhow!("router component not found: {}", router.display()));
    }

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }

    let wasm_tools = resolve_wasm_tools(wasm_tools)?;
    let adapter_path = write_adapter_component()?;

    let output = output.to_path_buf();
    if try_compose_with_wac(adapter_path.path(), router, &output)? {
        return Ok(());
    }

    let status = Command::new(&wasm_tools)
        .arg("compose")
        .arg(adapter_path.path())
        .arg("-d")
        .arg(router)
        .arg("-o")
        .arg(&output)
        .status()
        .with_context(|| format!("running {}", wasm_tools.display()))?;

    if !status.success() {
        return Err(anyhow!("wasm-tools compose failed with status {status}"));
    }

    Ok(())
}

fn try_compose_with_wac(adapter: &Path, router: &Path, output: &Path) -> Result<bool> {
    let Some(wac) = resolve_wac() else {
        return Ok(false);
    };

    let status = Command::new(&wac)
        .arg("plug")
        .arg(adapter)
        .arg("--plug")
        .arg(router)
        .arg("--output")
        .arg(output)
        .status();

    match status {
        Ok(status) => Ok(status.success()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(anyhow!("running {}: {err}", wac.display())),
    }
}

fn resolve_wasm_tools(wasm_tools: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = wasm_tools {
        return Ok(path.to_path_buf());
    }
    if let Ok(path) = std::env::var("GREENTIC_MCP_WASM_TOOLS")
        && !path.trim().is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    Ok(PathBuf::from("wasm-tools"))
}

fn resolve_wac() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("GREENTIC_MCP_WAC")
        && !path.trim().is_empty()
    {
        return Some(PathBuf::from(path));
    }
    Some(PathBuf::from("wac"))
}

fn write_adapter_component() -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("mcp_adapter_")
        .suffix(".component.wasm")
        .tempfile()
        .context("creating temp adapter component")?;
    std::io::Write::write_all(&mut file, ADAPTER_COMPONENT)
        .context("writing bundled adapter component")?;
    Ok(file)
}
