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
cat .komtar
```

1. **Right-Click Any Element:** Open the edit suggestion box.
2. **Select Text First:** Include the exact passage as context.
3. **Press Enter to Submit:** Use Shift+Enter to add a newline.

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
