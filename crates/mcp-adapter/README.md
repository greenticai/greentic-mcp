# Greentic MCP Adapter (wasix:mcp@25.06.18)

Guest component template that exports `greentic:component/node@0.6.0` and imports `wasix:mcp@25.06.18`. Intended to be composed with an MCP router component at pack-build time to produce the final MCP component artifact.

## Input contract

The adapter accepts either:

1) Explicit MCP adapter shape:

```json
{
  "operation": "list" | "call",
  "tool": "tool_name_if_call",
  "arguments": { }
}
```

2) Component-exec shorthand:

```json
{
  "key": "...",
  "q": "Nairobi",
  "aqi": "no"
}
```

When shorthand is used, the runtime operation id (for example `get_weather`) is
treated as the MCP tool name and the full JSON object is passed as
`arguments`.

Defaults / resolution:
- If `operation` is missing and `tool` is present → treat as `call`.
- If `operation` and `tool` are missing → treat as `list`.
- If `operation` is an unknown value and `tool` is missing, the operation value
  is treated as the tool name (compatibility path).
- `arguments` defaults to `{}`; must be an object if provided.

## Behavior

- `list` → invokes `list-tools` on the router; returns `{ok: true, result: { tools, protocol }}`.
- `call` → invokes `call-tool(tool, arguments)`; returns:
  - Success: `{ok: true, result { content, structured_content?, progress?, meta?, is_error?, annotations? }, messages: [...] , protocol}`.
  - Elicitation: `{ok: true, elicitation { ... }, messages: [...], protocol}`.

Content mapping:
- Text/image/audio/resource/resource-link are emitted both in `result.content` (full detail) and `messages` (simple cards).
- `structured_content` is parsed from the router’s JSON string for machine consumption.
- `annotations` and `meta` are passed through in `payload_json`.

## Errors

All errors use `{ ok: false, error { code, message, status, tool, protocol, details } }`:
- `MCP_TOOL_ERROR` for router `tool-error` variants (400/404/422/500 as appropriate).
- `MCP_ROUTER_ERROR` for transport/panic failures talking to the router (502).
- `MCP_CONFIG_ERROR` for invalid adapter inputs (400).

## Composition

- Build the adapter as a guest component targeting `wasm32-wasip2`.
- Compose it with an MCP router component exporting `wasix:mcp@25.06.18` via `wasm-tools component new` / `component-link`.
- The merged component is what flows should reference (`component_ref` in packs).

## Development

- Targets `greentic-interfaces-guest` with `guest` feature to generate bindings.
- Tests include list/call mapping, structured-content + resource-link handling, and error envelope mapping (using a mock router).
