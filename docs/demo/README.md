# Recording the demo

The README's demo GIF is the single highest-leverage asset in this repo: it is
what someone sees before they decide whether to read anything. It should show
one command turning a folder into something an agent searches, in under fifteen
seconds.

## What to record

`script.md` has the exact keystrokes. The shape:

1. `ls docs/` — a plain folder of Markdown. Nothing special.
2. `serve-md --plugin webmcp --dir ./docs` — one command. The banner prints the
   website, the MCP endpoint and the llms.txt.
3. `curl` the MCP endpoint and show `search_docs` returning a real answer with
   a heading and an anchor.
4. Cut to the browser showing the same document rendered.

The point being made is *one binary, three surfaces*. Do not narrate it; let
the output do that.

## How

```sh
./record.sh          # produces demo.cast
agg demo.cast demo.gif --font-size 16 --theme asciinema
```

[asciinema](https://asciinema.org) records, [agg](https://github.com/asciinema/agg)
converts to GIF. Commit `demo.cast` — it is small, diffable, and lets anyone
re-render the GIF — and `demo.gif`, then uncomment the image in `README.md`.

Keep the terminal at 80x24 so the GIF is legible on a phone.
