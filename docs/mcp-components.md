# MCP adapter and component overview

## mcp-adapter

`crates/mcp-adapter` is a guest component template that bridges JSON payloads into
`wasix:mcp@25.06.18` router calls. It exports `greentic:component/node@0.6.0` so the
component can be described and controlled by the Greentic platform, and it
imports `wasix:mcp@25.06.18` so it can invoke router exports.

What it does:

- Accepts `{operation?, tool?, arguments?}` JSON requests and applies defaults
  (`list` when no tool is provided, `call` when a tool is present).
- Supports shorthand calls where runtime `operation` is used as the tool name
  and the payload object is treated as `arguments`.
- Maps `list` to `list-tools` and `call` to `call-tool(tool, arguments)`.
- Returns MCP envelopes with `content`, optional `structured_content`, and
  lightweight `messages` cards; elicitations are surfaced as
  `{ok: true, elicitation: ...}`.
- Standardizes errors into `{ok: false, error { code, message, status, tool,
  protocol, details }}` with `MCP_TOOL_ERROR`, `MCP_ROUTER_ERROR`, or
  `MCP_CONFIG_ERROR`.

How it is used:

- Built as a `wasm32-wasip2` guest component.
- Composed at pack-build time with a router component to produce the final
  artifact that flows reference.
- Detailed payload/response behavior lives in `crates/mcp-adapter/README.md`.

The `greentic-mcp` CLI can perform this composition with the bundled adapter:

```bash
greentic-mcp compose ./router.component.wasm -o ./merged.component.wasm
```
