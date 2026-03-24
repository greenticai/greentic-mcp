Clean setup:

```bash
export EXAMPLE_DIR=/tmp/example-com-mcp
rm -rf "$EXAMPLE_DIR" && mkdir -p "$EXAMPLE_DIR" && gtc dev pack new --dir "$EXAMPLE_DIR" example-com-mcp && cp crates/mcp-exec/examples/example-com/example_home.yaml "$EXAMPLE_DIR/example_home.yaml"
```

What this does:

1. Generates an MCP component from `example_home.yaml`.
2. Updates/resolves/builds the pack.
3. Executes the MCP tool call and prints the response.

```bash
set -euo pipefail
gtc dev mcp-gen --spec "$EXAMPLE_DIR/example_home.yaml" --output-dir "$EXAMPLE_DIR/components" --done-dir "$EXAMPLE_DIR/done" --error-dir "$EXAMPLE_DIR/error"
gtc dev pack update --in "$EXAMPLE_DIR"
gtc dev pack resolve --in "$EXAMPLE_DIR"
gtc dev pack build --in "$EXAMPLE_DIR" --allow-pack-schema
gtc dev pack resolve --in "$EXAMPLE_DIR"
gtc dev mcp-exec router --router "$EXAMPLE_DIR/components/example_home.component.wasm" --enable-http --tool get_example_home --input '{}' --pretty
```
