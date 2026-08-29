# Demo script

Target: under 15 seconds. Type at a natural pace; do not pause to explain.

```sh
# 1. An ordinary folder of Markdown.
ls docs/

# 2. One command.
serve-md --plugin webmcp --dir ./docs --no-open

# 3. In a second pane: an agent's view of it.
curl -s localhost:8080/mcp \
  -H 'Content-Type: application/json' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: tools/call' \
  -H 'Mcp-Name: search_docs' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"search_docs","arguments":{"query":"authentication"}}}'

# 4. And the same thing a human reads.
curl -s localhost:8080/llms.txt | head -20
```

Then cut to a browser on `http://localhost:8080/` for two seconds. End.
