# Greentic MCP

Executor and component tooling for the Greentic platform targeting the
`wasix:mcp` interface. The workspace currently provides a reusable Rust
library (`greentic-mcp-exec`) that can load Wasm components, verify their provenance,
wire in the Greentic host imports, and execute the exported MCP entrypoint.

## Workspace layout

```
greentic-mcp/
├─ crates/
│  ├─ mcp-adapter/      # MCP adapter component template (wasix:mcp@25.06.18)
│  └─ mcp-exec/         # executor library (package: greentic-mcp-exec)
└─ Cargo.toml           # workspace manifest
```

### `greentic-mcp-exec`

Public API:

```rust
use greentic_types::{EnvId, TenantCtx, TenantId};
use greentic_mcp_exec::{ExecConfig, ExecRequest, RuntimePolicy, ToolStore, VerifyPolicy};
use serde_json::json;
use std::path::PathBuf;

let tenant = TenantCtx {
    env: EnvId("dev".into()),
    tenant: TenantId("acme".into()),
    team: None,
    user: None,
    trace_id: Some("trace-123".into()),
    correlation_id: None,
    deadline: None,
    attempt: 0,
    idempotency_key: None,
};

let cfg = ExecConfig {
    store: ToolStore::LocalDir(PathBuf::from("./tools")),
    security: VerifyPolicy::default(),
    runtime: RuntimePolicy::default(),
    http_enabled: false,
    secrets_store: None,
};

let result = greentic_mcp_exec::exec(
    ExecRequest {
        component: "weather_api".into(),
        action: "forecast_weather".into(),
        args: json!({"location": "AMS"}),
        tenant: Some(tenant),
    },
    &cfg,
)?;
```

Key features:

- **Resolver** – Reads Wasm bytes from local directories or single-file HTTP sources (with caching).
- **Verifier** – Checks digest/signature policy before execution.
- **Describe bridge** – Calls the `greentic:component/component@1.0.0` describe world when tools implement it, surfacing schema/default metadata directly from each component.
- **Runner** – Spins up a Wasmtime component environment, registers the `runner-host-v1` imports from `greentic-interfaces`, and calls the tool's MCP `exec` export.
- **Errors** – Structured error types map resolution, verification, and runtime failures to caller-friendly variants.

### `mcp-adapter`

Guest component template that:

- Exports `greentic:component/node@0.5.0` and imports `wasix:mcp@25.06.18`.
- Accepts JSON payloads of `{operation?, tool?, arguments?}`:
  - Defaults to `list` when no tool is provided; defaults to `call` when a tool is present.
  - Maps `list` → `list-tools`, `call` → `call-tool(tool, arguments)`.
- Returns `{ok: true, result: ...}` envelopes with content/structured-content/meta and lightweight cards/messages for text/image/audio/resource(-link) blocks; elicitations surface as `{ok: true, elicitation: ...}`.
- Errors use `{ok: false, error { code, message, status, tool, protocol, details }}` with codes `MCP_TOOL_ERROR`, `MCP_ROUTER_ERROR`, or `MCP_CONFIG_ERROR`.
- Designed to be composed at pack-build time with a router component; the final merged artifact is the component flows should reference.
- See `crates/mcp-adapter/README.md` for the detailed payload/response contract and composition notes.

### `greentic-types`

Pulled from crates.io; provides `TenantCtx`, identifiers, and supporting types for multi-tenant flows.

### Schema ownership

MCP node configuration schemas ship with the Wasm component that implements the tool. Each component is responsible for returning a `describe-json` payload (matching [`greentic:component@1.0.0`](https://docs.rs/greentic-interfaces)) that includes its schema/defaults. This repository only bridges the runtime protocol and does not try to duplicate per-tool JSON schemas.

## Development

```bash
rustup target add wasm32-wasip2
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
RUN_ONLINE_TESTS=1 cargo test -p greentic-mcp-exec --test online_weather
```

The online weather integration test is skipped unless `RUN_ONLINE_TESTS=1` is set.

For faster local MCP runs, prefer the release build and enable the Wasmtime cache:

```bash
cargo run -p greentic-mcp-exec --release -- router --router /path/to/component.wasm --list-tools

cat > /tmp/wasmtime-cache.toml <<'EOF'
[cache]
enabled = true
directory = "/tmp/wasmtime-cache"
EOF

WASMTIME_CACHE_CONFIG=/tmp/wasmtime-cache.toml \
cargo run -p greentic-mcp-exec --release -- router --router /path/to/component.wasm --list-tools
```

## Local checks

Run `ci/local_check.sh` before pushing to mirror the CI matrix locally. Helpful
toggles:

- `LOCAL_CHECK_ONLINE=1` – enable the networked weather test.
- `LOCAL_CHECK_STRICT=1` – treat skipped optional steps/tools as failures.
- `LOCAL_CHECK_VERBOSE=1` – echo each command.

The script will install a lightweight `pre-push` hook (if one is not already
present) so future pushes automatically run the same checks.

## Protocol revisions

The client-side protocol helpers understand multiple MCP protocol revisions.
`ProtocolRevision` defaults to `2025-06-18`; configs may optionally set
`protocol_revision` (e.g., `"2025-03-26"`) per server. When unspecified, the
latest revision is used while keeping message shapes compatible with older
servers.

Structured tool output is carried through as-is: `outputSchema` is preserved on
tools, and tool call results may include `structuredContent` alongside plain
content for richer responses.

**Design note:** MCP transport (HTTP/JSON-RPC, protocol headers, batching,
OAuth token injection) is handled in other Greentic components. This crate only
models MCP message shapes and executes tools locally via WIT/wasm host calls.

## Releases & Publishing

- Versions are taken directly from each crate's `Cargo.toml`.
- When a commit lands on `master`, any crate whose manifest version changed gets a Git tag `<crate>-v<version>` pushed automatically.
- The publish workflow then runs, linting and testing before calling `katyo/publish-crates@v2` to publish updated crates to crates.io.
- Publishing is idempotent; if the specified version already exists, the workflow exits successfully without pushing anything new.

### MCP adapter publishing

- greentic-mcp builds and publishes the MCP adapter for `wasix:mcp@25.06.18` to GHCR:
  - `ghcr.io/greenticai/greentic-mcp-adapter:25.06.18-v<adapter_version>`
  - `ghcr.io/greenticai/greentic-mcp-adapter:25.06.18-stable` (moving pointer)
- The pushed artifact is `mcp_adapter_25_06_18.component.wasm`, implementing `greentic:component/node@0.5.0` and importing `wasix:mcp@25.06.18`.
- See `.github/workflows/publish-mcp-adapter.yml` and `scripts/build_adapter.sh` for the build/publish steps.

## Roadmap

- Implement OCI and Warg resolvers, including signature verification.
- Publish spec docs and add end-to-end examples powered by real tool WASMs.

## License

Dual-licensed under either MIT or Apache-2.0. See `LICENSE-MIT` and
`LICENSE-APACHE` once added to the repository.

