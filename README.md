# Komtar

Tell the agent what to change on the page.

Stupid simple. Works with any framework. Single binary.

## Installation

Install with Cargo:

```sh
cargo install komtar
```

Or download a prebuilt macOS ARM64 binary, or a statically linked Linux AMD64 or
Linux ARM64 binary, from the
[latest release](https://github.com/rot256/komtar/releases/latest), then place
`komtar` somewhere on your `PATH`.

## Usage

Serve a static site with live reload:

```sh
komtar serve ./public
open http://127.0.0.1:3939
```

Komtar serves files and directory `index.html` files, returning a normal 404
when a path does not exist. When a served file changes, open pages reload. If
the suggestion dialog is open, Komtar waits until the suggestion is submitted
or dismissed so the draft is not lost. Dotfiles (including the configured
FIFO) are not exposed by the static server.

For a project that already has a development server, proxy it through Komtar:

```sh
komtar proxy http://127.0.0.1:5173
open http://127.0.0.1:3939
```

The original shorthand remains supported:

```sh
komtar http://127.0.0.1:5173
```

Ask your coding agent to wait for queued suggestions:

```sh
komtar recv
```

`recv` waits for a nonempty delivery, prints one batch of unchanged v1 NDJSON
records, and exits. After processing that batch, the agent runs `komtar recv`
again directly. Run `komtar agent` to print the complete workflow and a sample
feedback record.

1. **Right-Click Any Element:** Open the edit suggestion box.
2. **Select Text First:** Include the exact passage as context.
3. **Right-Click an Agent Answer:** Comment on that response and let the next
   requested answer attach to it.
4. **Press `/`:** Open an unanchored page-level comment box when focus is not
   in an editable field.
5. **Press Enter to Submit:** Use Shift+Enter to add a newline.

## Agent answers

An agent can answer an explicit browser question with Markdown on standard
input:

```sh
printf 'The limit comes from the upstream API.' | komtar send
printf 'Use the shorter heading.' | komtar send --anchor '#intro'
```

General answers stack above the Komtar badge. Anchored answers appear beside
the matching element and fall back to a general “Target unavailable” card
until that element exists. Markdown is rendered and sanitized by the server.
Comments on agent answers carry an opaque `komtar-agent:` target selector;
passing that selector back to `send --anchor` attaches the requested answer to
the original response. Page-level comments opened with `/` have a blank target
selector and should receive unanchored answers.
Element links use the `komtar:` scheme; percent-encode reserved selector
characters and spaces:

```md
[Show the introduction](komtar:%23intro%20.item)
```

Every connected browser receives every answer. The newest 500 answers are
kept in memory, and answers received in the previous ten minutes are replayed
to newly connected pages. Dismissal is local to one browser tab and survives a
reload in that tab. History is cleared when Komtar restarts.

Use `send` and `recv` as the agent API. The raw transports remain available for
interoperability and debugging: browser comments use `<fifo>` (default
`.komtar`) and agent messages use `<fifo>.send`. Direct writers must avoid
concurrent writes. Both are private `0600` FIFOs, coordinated through an
internal `<fifo>.lock` file. Komtar removes all three files on handled shutdown
or an ordinary error, and exits if any is deleted or replaced while it runs.
As with any filesystem FIFO, an uncatchable `SIGKILL` or power loss can leave a
stale inode; the next server validates and adopts a stale FIFO safely.

With a custom transport path, pass the same global option to every helper:

```sh
komtar serve ./public --fifo feedback.pipe
komtar recv --fifo feedback.pipe
printf 'Answer' | komtar send --fifo feedback.pipe
```

`send` fails immediately when no Komtar server is reading. `recv` intentionally
waits indefinitely when no browser feedback arrives.

## Collaborative Editing

Keep Komtar on loopback and expose it only to your tailnet:

```sh
komtar serve ./public
```

In another terminal:

```sh
tailscale serve 3939
```

Share the URL printed by Tailscale Serve. Everyone in your tailnet can point at
the page and suggest changes, and all suggestions are queued in the same `.komtar`
FIFO for your coding agent. Komtar does not provide its own authentication.
