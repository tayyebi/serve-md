# serve-md

serve-md turns a folder of documents into an AI-ready website.

You point it at your documentation, and with one command it creates a clean website that people can read, while simultaneously giving AI agents a structured way to search and read those same documents.

There is no database, cloud service, or complicated setup. It is a single lightweight application that runs wherever your documents are.

Point it at a folder. You get a website, a Model Context Protocol server, and a generated `llms.txt`, from one command and one static binary with no runtime dependencies.

```
$ serve-md --plugin webmcp --dir ./docs

serve-md 0.5.0
serving: ./docs
plugins: webmcp
  http://127.0.0.1:8080/
  http://127.0.0.1:8080/mcp          Model Context Protocol
  http://127.0.0.1:8080/llms.txt     index for language models
search: rg
```

<!-- TODO: record and commit docs/demo.gif, then uncomment.
![serve-md turning a folder of Markdown into a searchable MCP server](docs/demo.gif)
-->

It builds from a single crate (`comrak`). The HTTP server, JSON parser,
template engine, HTML tokenizer, base64 and percent-encoding are all
hand-rolled, and the release binaries are fully static.

---

## Quick start

```sh
serve-md                      # serve the current directory, open a browser
serve-md --dir ./docs         # serve somewhere else
serve-md --plugin webmcp      # ...and let AI agents use it
serve-md --plugin webmcp --fresh   # ...and pick up edits while running
```

## Contents

- [For humans](#for-humans) — rendering, the terminal, plugins
- [For agents](#for-agents) — MCP, WebMCP, `llms.txt`
- [Options](#options)
- [Security](#security)
- [Install](#install) · [Build](#build)

---

## For humans

### Rendering

Markdown and HTML files are rendered as semantic HTML5 with no CSS and no
JavaScript — your browser's defaults, and nothing else. There is no banner, no
footer and no breadcrumb: the page is the document. Directories resolve to
`index.html` then `index.md`; a directory without either shows a file listing.

Every document has exactly one URL. Slash runs collapse, trailing slashes drop,
and a trailing `index.html`/`index.md` is suppressed when the shorter path
serves the identical file. Anything else redirects, once, with `301`.

### The terminal is a first-class client

`curl` and `wget` get plain text, not markup:

```sh
curl localhost:8080/                          # the listing, as a table
curl localhost:8080/guides/start.md           # rendered to 80-column text
curl -H 'Accept: text/markdown' localhost:8080/guides/start.md   # the source
curl -H 'Accept: text/plain'    localhost:8080/page.html         # HTML → text
```

| Client asks for | Markdown file | HTML file |
|---|---|---|
| `Accept: text/markdown` | the source, unchanged | converted to Markdown |
| `Accept: text/plain`, or a `curl`/`wget` user-agent | rendered to 80-column text | converted to text |
| anything else | rendered HTML | served as-is |

### Render plugins

None are enabled by default; pass `--plugin <NAME>` (repeatable, or
comma-separated).

| Plugin | Effect |
|---|---|
| `math` | `$…$` and `$$…$$` become MathML, rendered by the browser natively — no KaTeX, no web fonts, no JavaScript |
| `mermaid` | ` ```mermaid ` flowcharts become SVG, laid out server-side with a Sugiyama algorithm — no Mermaid.js |
| `webmcp` | the agent surface: `/mcp`, `/llms.txt`, and in-browser WebMCP |

Both `math` and `mermaid` render **server-side**, so pages stay script-free and
work with JavaScript disabled.

---

## For agents

Everything in this section requires `--plugin webmcp`. Without it, serve-md is
exactly the file server described above and none of these routes exist.

### The MCP endpoint

`POST /mcp` is a [Model Context Protocol][mcp] server over Streamable HTTP.

It implements revision **2026-07-28**, whose stateless core — no `initialize`
handshake, no session id, no SSE stream required — is a natural fit for a
server that keeps no per-client state. It also answers the older
`initialize`-based revisions (2025-03-26 through 2025-11-25), because most
clients in the field still open with those.

**Claude Desktop / Claude Code**

```jsonc
// claude_desktop_config.json
{
  "mcpServers": {
    "my-docs": {
      "type": "http",
      "url": "http://localhost:8080/mcp"
    }
  }
}
```

**Cursor** — `.cursor/mcp.json`, same shape. **VS Code** — `.vscode/mcp.json`:

```jsonc
{
  "servers": {
    "my-docs": { "type": "http", "url": "http://localhost:8080/mcp" }
  }
}
```

Or from the shell:

```sh
curl -s localhost:8080/mcp \
  -H 'Content-Type: application/json' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: tools/list' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

#### Tools

| Tool | Arguments | Returns |
|---|---|---|
| `search_docs` | `query`, `limit?`, `regex?` | Matching lines with file, line number, and the heading each match sits under |
| `read_doc` | `path`, `format?` | One document as `markdown` (default), `text`, or `html` |
| `list_docs` | — | Every document with title, size and mtime |
| `get_outline` | `path` | The heading tree, with anchors that resolve on the page |

Search delegates to whichever tool the host already has, trying `rg`, then
`ag`, then `grep`. If none is on `PATH`, search reports that and the other
tools carry on working. Install [ripgrep][rg] for the best results.

#### Resources

Every document is also published as an MCP resource
(`serve-md:///guides/start.md`), so a person can attach one directly in their
client rather than hoping the model calls a tool for it.

#### Discovery

`GET /.well-known/mcp.json` returns a server card naming the endpoint, the
protocol revisions supported, and the tools available.

### WebMCP, in the browser

Rendered pages register the same four tools with the browser's own agent
through [WebMCP][webmcp] (`document.modelContext.registerTool()`, shipped in
Chrome 146 and Edge 147). A visitor's in-browser agent can search your
documentation with **no configuration at all** — no URL to paste, no
credentials to hand over.

The injected script implements nothing itself; each tool forwards to `/mcp`. So
there is one implementation of every tool, and the browser surface cannot drift
from the server's.

This is the only JavaScript serve-md ever emits, and only under
`--plugin webmcp`.

### llms.txt

`GET /llms.txt` returns an [llms.txt v2][llmstxt] index generated from the
tree — titles and one-line summaries read from each document's own first
heading and first sentence — and `GET /llms-full.txt` returns every document
concatenated.

**If you have written your own `llms.txt`, yours is served.** Generation only
fills the gap when there is no file.

---

## Options

| Flag | Default | |
|---|---|---|
| `--host <HOST>` | `127.0.0.1` | |
| `--port <PORT>` | `8080` | |
| `--dir <DIR>` | `.` | Directory to serve |
| `--plugin <NAME>` | none | Repeatable or comma-separated |
| `--fresh` | off | Watch for changes instead of scanning once at startup |
| `--fresh-interval <MS>` | `1000` | How often `--fresh` re-scans |
| `--user <USER>` | none | Require Basic auth |
| `--pass <PASS>` | none | Or set `SERVE_MD_PASSWORD` |
| `--no-open` | off | Do not open a browser on startup |
| `--verbose` | off | Log each request |

Run `serve-md --help` for the authoritative list.

### Freshness

By default the file list is read **once, at startup** — so a document added
while the server runs is not listed until you restart. `--fresh` starts a
watcher that re-walks the tree and picks changes up, and the website, the MCP
tools and `llms.txt` all read from that one shared list.

It watches by polling rather than by native filesystem events, which would
require FFI and end the single-dependency, pure-Rust static build.

---

## Security

- **Hidden and VCS paths are never served.** Any dot-prefixed name, plus
  `.git`, `.hg`, `.svn`, `target` and `node_modules`, is refused by both the
  listing and the router — the two share one rule, so a name the listing hides
  cannot be reached by typing its path. `.well-known` is the single exception.
- **Path traversal is refused twice**: once as a filter on the request string
  (`..`, `.`, backslashes, control bytes, drive letters, UNC prefixes), and
  again after resolution, so a symlink cannot smuggle a path back out. Every
  refusal is an indistinguishable `404`.
- **Search cannot escape the tree.** Every hit from `rg`/`ag`/`grep` is checked
  against the served file list before it is returned, so a search for
  `password` can never surface a line of `.git/config` or `.env`. The search
  tool is invoked with `Command::new`, never a shell, and the query is passed
  after `-e`/`--` as a literal string by default.
- **The MCP endpoint exposes nothing extra.** It reads the same files the
  website serves, through the same path resolution, behind the same
  `--user`/`--pass` if you set it.
- **Bounded by design**: a connection cap, a header size and count cap, a
  request body cap, path length and depth caps, a search timeout, and read,
  write and header deadlines. Static files stream rather than buffer.

Serving publicly? Set `--user`/`--pass`, or put it behind a reverse proxy that
terminates TLS. serve-md speaks plain HTTP only.

---

## Install

Download a binary from [releases](https://github.com/tayyebi/serve-md/releases):

| Platform | Asset |
|---|---|
| Linux x86-64 | `serve-md-linux-x86_64` |
| Linux ARM64 | `serve-md-linux-aarch64` |
| Windows x86-64 | `serve-md-windows-x86_64.exe` |

Or:

```sh
curl -fsSLO https://github.com/tayyebi/serve-md/releases/latest/download/serve-md-linux-x86_64
chmod +x serve-md-linux-x86_64 && sudo mv serve-md-linux-x86_64 /usr/local/bin/serve-md
```

Or build it yourself — the repo is the only prerequisite:

```sh
cargo install --git https://github.com/tayyebi/serve-md
```

A `Dockerfile` is included if you would rather run it in a container:

```sh
docker build -t serve-md . && docker run --rm -p 8080:8080 -v "$PWD:/docs" serve-md
```

The Linux binaries are **fully static** — no libc, no interpreter, no shared
objects. CI asserts this on every release. They run on Alpine, on distroless,
and in `FROM scratch`.

> One caveat for minimal images: `search_docs` needs `rg`, `ag` or `grep` on
> `PATH`. A `scratch` image has none. The published image is Alpine with
> ripgrep for exactly this reason.

## Build

```sh
cargo build --release
cargo test
```

No C toolchain is needed even for the musl targets — the dependency tree is
pure Rust.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Adding a plugin is one file in
`src/plugin/` and one line in `REGISTRY`.

## License

MIT — see [LICENSE](LICENSE).

[mcp]: https://modelcontextprotocol.io/specification/2026-07-28
[webmcp]: https://github.com/webmachinelearning/webmcp
[llmstxt]: https://llmstxt.org/
[rg]: https://github.com/BurntSushi/ripgrep
