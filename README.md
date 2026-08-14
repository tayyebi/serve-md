# serve-md

A minimal, zero-framework web server that lists and renders Markdown files. It
serves a clean, reader-friendly HTML page for browsers and plain text for
`curl`/`wget`, with optional HTTP Basic auth. Ships as a single binary — no
dependencies beyond the markdown renderer (`comrak`).

## Features

- Recursively lists every `*.md` file in the served directory (skipping `.git`,
  `target`, `node_modules`, and friends), sorted by path.
- Browser (`GET /`): semantic HTML listing with relative path, size, and mtime.
  Click a file (`/view/<path>`) to see it rendered with GFM tables,
  strikethrough, autolinks, and task lists. A **Raw** link shows the source.
- Terminal (`GET /` with a `curl`/`wget` user agent): a plain-text listing.
  `GET /view/<path>` renders the markdown to reader-friendly ASCII (wrapped
  paragraphs, underlined headings, ASCII tables, indented lists and code
  blocks). `GET /raw/<path>` returns the untouched markdown source.
- Optional HTTP Basic auth via `--user`/`--pass` (constant-time check).
- Path-traversal safe: requests are validated and resolved strictly under the
  served directory, only `.md` files are served.

## Usage

```
serve-md 0.2.1
A minimal web server that lists and renders Markdown files.

USAGE:
    serve-md [OPTIONS]

OPTIONS:
        --host <HOST>      Address to bind to            [default: 127.0.0.1]
        --port <PORT>      Port to listen on             [default: 8080]
        --dir <DIR>        Directory to serve            [default: .]
        --user <USER>      Require Basic auth username
        --pass <PASS>      Password for --user (or env SERVE_MD_PASSWORD)
        --no-open          Do not open a browser on startup
        --verbose          Log each request to stdout
    -h, --help             Print help
    -V, --version          Print version

EXAMPLES:
    serve-md
    serve-md --host 0.0.0.0 --port 9000 --dir ./docs
    serve-md --user admin --pass secret
    curl -u admin:secret http://127.0.0.1:8080/view/README.md
```

`--pass` is optional when `SERVE_MD_PASSWORD` is set in the environment.

## Terminal examples

```
# list files
$ curl http://127.0.0.1:8080/

# view a file (rendered to ASCII: wrapped text, tables, headings)
$ curl http://127.0.0.1:8080/view/docs/guide.md

# raw markdown
$ curl http://127.0.0.1:8080/raw/docs/guide.md
```

## Build

```
cargo build --release
```

GitHub Actions runs `fmt`, `clippy`, `test`, and a release build on every push.
Pushing a `v*` tag builds release binaries for Linux and Windows and attaches
them to a GitHub release.

```
git tag v0.1.0
git push origin v0.1.0
```

## License

MIT
