# Komtar

Tell the agent what to change on the page.

Stupid simple. Works with any framework. Single binary.

## Installation

Install with Cargo:

```sh
cargo install --git https://github.com/rot256/komtar
```

Or download a prebuilt static binary for macOS ARM64, Linux AMD64, or Linux ARM64
from the [latest release](https://github.com/rot256/komtar/releases/latest), then
place `komtar` somewhere on your `PATH`.

## Usage

Start your project's development server, then proxy it through Komtar:

```sh
komtar proxy http://127.0.0.1:5173
open http://127.0.0.1:3939
```

Ask your coding agent to wait for queued suggestions:

```sh
cat .komtar
```

1. **Right-Click Any Element** — Open the edit suggestion box.
2. **Select Text First** — Include the exact passage as context.
3. **Press Enter to Submit** — Use Shift+Enter to add a newline.

## Collaborative Editing

Keep Komtar on loopback and expose it only to your tailnet:

```sh
komtar proxy http://127.0.0.1:5173
```

In another terminal:

```sh
tailscale serve 3939
```

Share the URL printed by Tailscale Serve. Everyone in your tailnet can point at
the page and suggest changes, and all suggestions are queued in the same `.komtar`
FIFO for your coding agent. Komtar does not provide its own authentication.
