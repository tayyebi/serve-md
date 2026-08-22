# serve-md

A minimal, zero-framework **static file server** with first-class Markdown
and HTML rendering. Every file in the served directory is reachable at its
real path — `index.html`, `style.css`, `favicon.ico`, images, video, `.md`
docs, all of it. `.md` and `.html` files additionally render as a clean,
reader-friendly page for browsers and plain text for `curl`/`wget`, with
optional HTTP Basic auth. Ships as a single binary — no dependencies beyond
the markdown renderer (`comrak`).

## Features

- **Static file server**: any file under the served directory is served at
  its real path (`GET /style.css`, `GET /images/logo.png`, `GET
  /favicon.ico`, ...) with a guessed `Content-Type`, byte-for-byte.
- **Index resolution**: `GET /` (or any directory path, e.g. `GET /docs`)
  tries `index.html`, then `index.md`, in that directory; if neither exists,
  it falls back to a recursive listing of every file in the served tree.
- **Markdown & HTML rendering**: requesting `.md`/`.markdown` or
  `.html`/`.htm` files directly negotiates format —
  - Browser: Markdown renders to HTML (GFM tables, strikethrough, autolinks,
    task lists); HTML files are served as-is.
  - Terminal (`curl`/`wget` user agent): Markdown renders to reader-friendly
    ASCII (wrapped paragraphs, underlined headings, ASCII tables, indented
    lists/code blocks); HTML gets its tags stripped with block-aware line
    breaks.
  - `Accept: text/markdown` or `Accept: text/plain` on **any** `.md`/`.html`
    request forces that format regardless of user agent — Markdown source,
    or converted plain text, respectively.
- Recursively skips `.git`, `target`, `node_modules`, and friends when
  listing or resolving files.
- Optional HTTP Basic auth via `--user`/`--pass` (constant-time check).
- Path-traversal safe: requests are validated and resolved strictly under the
  served directory.

## Usage

```
serve-md 0.3.0
A minimal web server that lists and renders Markdown and HTML files.

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
    curl -u admin:secret http://127.0.0.1:8080/README.md
```

`--pass` is optional when `SERVE_MD_PASSWORD` is set in the environment.

## Terminal examples

```
# list files (or serve index.html/index.md if present)
$ curl http://127.0.0.1:8080/

# view a markdown file (rendered to ASCII: wrapped text, tables, headings)
$ curl http://127.0.0.1:8080/docs/guide.md

# force raw markdown source regardless of client
$ curl -H 'Accept: text/markdown' http://127.0.0.1:8080/docs/guide.md

# static assets are served as-is
$ curl http://127.0.0.1:8080/favicon.ico -o favicon.ico
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
