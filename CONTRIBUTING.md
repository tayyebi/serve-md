# Contributing

Thanks for taking a look.

## Before you start

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

CI runs both, and clippy is a hard gate. Tests run single-threaded
(`--test-threads=1`) because several bind real sockets.

## The constraints that shape this codebase

These are deliberate. A change that breaks one needs a good argument.

1. **One dependency.** `comrak`, and nothing else. The HTTP server, JSON
   parser, template engine, HTML tokenizer, base64 and percent-encoding are all
   hand-rolled because of it. A new crate costs the fully static, pure-Rust
   build — the release workflow needs no C toolchain, and that holds only while
   the tree stays pure Rust.
2. **Nothing is on by default.** Plugins are opt-in, and so is every route they
   bring. A serve-md started with no flags must be exactly the server it was
   before your change.
3. **Rendered pages carry no CSS and no JavaScript**, except the WebMCP script
   under `--plugin webmcp`.
4. **Tests live beside the code**, in an inline `#[cfg(test)] mod tests`. There
   is no `tests/` directory.
5. **Comments explain why, not what** — and where a standard governs the code,
   cite it. Most modules open with a `# References` section linking the spec
   they implement.

## Security-sensitive areas

Changes here want particular care and a test that would fail without the fix:

- `http::safe_join` / `http::resolve` — path traversal and symlink containment.
- `scanner::is_forbidden_segment` — the one rule the listing and the router
  share. A name the listing hides must not be reachable by typing its path.
- `search::keep_served_paths` — the check that stops an external search tool
  reporting a file the site does not serve. The engine's own exclusions are a
  second line of defence, not the guarantee.
- `http::read_body` — request bounds.

## Adding a plugin

1. Write `src/plugin/your_plugin.rs` implementing the `Plugin` trait.
2. Add one line to `REGISTRY` in `src/plugin/mod.rs`.

`--help` picks it up automatically from `plugin::catalog()`.

## Commits

Conventional Commits, lowercase subject: `feat:`, `fix:`, `docs:`, `test:`,
`ci:`, `build:`, `refactor:`. Branches are `feat/…`, `fix/…`, `build/…`.
