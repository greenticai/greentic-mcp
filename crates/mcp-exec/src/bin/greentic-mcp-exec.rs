use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use greentic_interfaces_wasmtime::host_helpers::v1::{runner_host_http, runner_host_kv};
use greentic_mcp_exec::router;
use greentic_mcp_exec::runner::{StoreState, add_secrets_to_linker};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::add_to_linker_sync as add_wasi_to_linker;
use wasmtime_wasi_http::p2::add_only_http_to_linker_sync as add_wasi_http_to_linker;
use wasmtime_wasi_tls::LinkOptions;

#[derive(Parser)]
#[command(
    name = "greentic-mcp-exec",
    version,
    about = "Execute wasix:mcp/router components locally"
)]
struct Cli {
    /// Increase diagnostic output.
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Invoke a router component export (wasix:mcp/router@25.6.18).
    Router(RouterCommand),
}

#[derive(Parser)]
struct RouterCommand {
    /// Path to the router component (.wasm).
    #[arg(long, value_name = "PATH")]
    router: PathBuf,
    /// Router tool name (alias: --operation).
    #[arg(long, alias = "operation", value_name = "NAME")]
    tool: Option<String>,
    /// List tools instead of calling one.
    #[arg(long)]
    list_tools: bool,
    /// Allow router HTTP calls (default off).
    #[arg(long)]
    enable_http: bool,
    /// Optional timeout in milliseconds for the router call/list.
    #[arg(long, value_name = "MILLIS")]
    timeout_ms: Option<u64>,
    /// Inline JSON arguments to pass to call-tool.
    #[arg(long, value_name = "JSON")]
    input: Option<String>,
    /// Read JSON arguments from file.
    #[arg(long, value_name = "FILE")]
    input_file: Option<PathBuf>,
    /// Pretty-print the response.
    #[arg(long)]
    pretty: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Router(cmd) => run_router(cmd, cli.verbose),
    }
}

fn run_router(cmd: RouterCommand, verbose: bool) -> Result<()> {
    if verbose {
        eprintln!(
            "router CLI starting (list_tools={}, enable_http={})",
            cmd.list_tools, cmd.enable_http
        );
    }
    // Avoid blocking on stdin when we're only listing tools.
    let args_json = if cmd.list_tools {
        "{}".to_string()
    } else {
        load_input(cmd.input.clone(), cmd.input_file.clone())?
    };
    if verbose {
        eprintln!("creating wasmtime engine");
    }
    let engine = build_engine()?;
    if verbose {
        eprintln!("loading component {}", cmd.router.display());
    }
    let component = Component::from_file(&engine, &cmd.router)
        .map_err(|err| anyhow!("loading component {}: {}", cmd.router.display(), err))?;
    if verbose {
        eprintln!("component loaded");
    }

    // Offload instantiation/invocation to a worker so we can enforce a wallclock timeout.
    let timeout = cmd.timeout_ms.map(std::time::Duration::from_millis);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let res = invoke_router(cmd, args_json, engine, component, verbose);
        let _ = tx.send(res);
    });

    match timeout {
        Some(dur) => match rx.recv_timeout(dur) {
            Ok(res) => res,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err(anyhow!("router call timed out after {:?}", dur))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow!("router call worker failed"))
            }
        },
        None => rx
            .recv()
            .map_err(|_| anyhow!("router call worker failed"))?,
    }
}

fn invoke_router(
    cmd: RouterCommand,
    args_json: String,
    engine: Engine,
    component: Component,
    verbose: bool,
) -> Result<()> {
    if verbose {
        eprintln!("creating linker and wiring wasi/hosts");
    }
    let mut linker = Linker::new(&engine);
    linker.allow_shadowing(true);
    add_wasi_to_linker(&mut linker)
        .map_err(|err| anyhow!("linking wasi preview2 imports: {}", err))?;

    // Mirror runtime linker setup so router components importing wasi:http/types
    // and wasi:tls types can instantiate in this direct CLI path.
    let mut opts = LinkOptions::default();
    opts.tls(true);
    wasmtime_wasi_tls::add_to_linker(&mut linker, &mut opts, |h: &mut StoreState| h.wasi_tls())
        .map_err(|err| anyhow!("linking wasi tls imports: {}", err))?;
    add_wasi_http_to_linker(&mut linker)
        .map_err(|err| anyhow!("linking wasi http imports: {}", err))?;

    runner_host_http::add_runner_host_http_to_linker(&mut linker, |state: &mut StoreState| state)
        .map_err(|err| anyhow!("linking runner host http: {}", err))?;
    runner_host_kv::add_runner_host_kv_to_linker(&mut linker, |state: &mut StoreState| state)
        .map_err(|err| anyhow!("linking runner host kv: {}", err))?;
    add_secrets_to_linker(&mut linker).map_err(|err| anyhow!("linking secrets host: {}", err))?;

    let http_enabled = cmd.enable_http && !cmd.list_tools;
    if verbose {
        eprintln!("building store (http_enabled={})", http_enabled);
    }
    let mut store = Store::new(&engine, StoreState::new(http_enabled, None, None));

    if verbose {
        eprintln!("instantiating router component {}", cmd.router.display());
    }
    let router = router::McpRouter::instantiate(&mut store, &component, &linker)
        .map_err(|err| anyhow!("component missing wasix:mcp/router@25.6.18 exports: {err}"))?;

    if verbose {
        let tool = cmd.tool.as_deref().unwrap_or("<list-tools>");
        eprintln!(
            "executing router `{}` via tool `{}`",
            cmd.router.display(),
            tool
        );
    }

    let router_iface = router.wasix_mcp_router();

    if cmd.list_tools {
        if verbose {
            eprintln!("calling list-tools");
        }
        let tools = router_iface
            .call_list_tools(&mut store)
            .map_err(|err| anyhow!(err.to_string()))?;
        if verbose {
            eprintln!("list-tools returned {} entries", tools.len());
        }
        let names: Vec<_> = tools.into_iter().map(|t| t.name).collect();
        if cmd.pretty {
            println!("{}", serde_json::to_string_pretty(&names)?);
        } else {
            println!("{}", serde_json::to_string(&names)?);
        }
        return Ok(());
    }

    let tool = cmd
        .tool
        .as_deref()
        .ok_or_else(|| anyhow!("--tool/--operation is required unless --list-tools is set"))?;

    let result = router_iface
        .call_call_tool(&mut store, tool, &args_json)
        .map_err(|err| anyhow!(err.to_string()))?;

    let json = match result {
        Ok(resp) => router::render_response(&resp),
        Err(err) => router::tool_error_to_value(tool, err),
    };

    if cmd.pretty {
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        println!("{}", serde_json::to_string(&json)?);
    }

    Ok(())
}

fn load_input(inline: Option<String>, file: Option<PathBuf>) -> Result<String> {
    if let Some(path) = file {
        let contents =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        return Ok(contents);
    }

    if let Some(inline) = inline {
        return Ok(inline);
    }

    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .context("reading stdin")?;
    if buf.trim().is_empty() {
        return Err(anyhow!(
            "no input provided (use --input, --input-file, or stdin)"
        ));
    }
    Ok(buf)
}

fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    // Epoch interruption is disabled here; caller-driven timeouts are enforced by a worker thread.
    config.epoch_interruption(false);
    Engine::new(&config).map_err(|err| anyhow!("initializing wasmtime engine: {}", err))
}
