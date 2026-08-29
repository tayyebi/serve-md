# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] — unreleased

The agent release. A folder of Markdown becomes something an AI agent can
search and read, not just a website.

### Added

- **`webmcp` plugin** (`--plugin webmcp`), gating everything below. Without it
  the server is byte-for-byte what it was in 0.4.0.
- **`POST /mcp`** — a Model Context Protocol endpoint over Streamable HTTP,
  implementing revision 2026-07-28 (stateless: no handshake, no session id) and
  answering the older `initialize`-based revisions for clients still on them.
- **Four tools**: `search_docs`, `read_doc`, `list_docs`, `get_outline`.
- **Resources** — every document published as `serve-md:///<path>`, so it can
  be attached directly in an MCP client.
- **Full-text search**, delegated to `rg`, then `ag`, then `grep`. Every hit is
  filtered against the served file list before it is returned.
- **WebMCP in the browser** — rendered pages register the same tools via
  `document.modelContext.registerTool()`, so an in-browser agent needs no
  configuration. Each tool forwards to `/mcp`; nothing is reimplemented.
- **`GET /llms.txt` and `GET /llms-full.txt`**, generated from the tree per the
  llms.txt v2 format. A local file of either name is always served instead.
- **`GET /.well-known/mcp.json`** — a server card for endpoint discovery.
- **`--fresh` and `--fresh-interval <MS>`** — watch the directory and pick up
  changes while running. The website, the MCP tools and `llms.txt` all read
  from one shared file list.
- **Heading `id` attributes** on rendered pages under `webmcp`, so the
  `#anchor` links the tools hand out actually resolve.
- `LICENSE`, `CONTRIBUTING.md`, this changelog, issue and PR templates, a
  Dockerfile, a Homebrew formula, and `cargo binstall` metadata.

### Changed

- The file list moved from a startup-only `Vec` into a shared catalog.
- `POST` and `OPTIONS` are accepted, on the MCP endpoint only. Every other path
  still answers `405 Method Not Allowed` with `Allow: GET, HEAD`.
- The README no longer mirrors `--help` verbatim, which had been drifting.

### Fixed

- YAML front matter is stripped rather than rendered as prose. comrak had no
  delimiter configured, so a leading `---` block parsed as a thematic break
  followed by a setext heading, and a generator-written document opened with
  its own metadata as its title — which would then have leaked into `llms.txt`
  and the MCP tool output. Set in `Set::options()`, so the browser, the
  terminal renderer and `docmeta`'s title extraction all agree.
- A directory without an index served the whole tree rather than its own
  contents. `Resolved::Listing` now carries the directory's path and the
  listing is scoped one level deep. The root listing is unchanged: it is the
  site index and what `curl <base>/` is documented to return.

### Security

- Request bodies are bounded (1 MiB) and read only for `POST`. `Transfer-Encoding:
  chunked` is refused rather than parsed.
- Search never goes through a shell; the query is passed after `-e`/`--` and
  matched literally unless `regex` is requested. `grep` is invoked with `-r`,
  not `-R`, so it does not follow symlinks out of the tree.

## [0.4.0]

### Added

- Canonical URLs: slash runs collapsed, trailing slashes dropped, redundant
  `index.*` segments suppressed.
- Hidden and VCS path refusal shared by the scanner and the router.
- Fully static release binaries, asserted in CI.
- Render plugins: `math` (MathML) and `mermaid` (server-side SVG).
