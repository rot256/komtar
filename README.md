# web-fifo

`web-fifo` is a development reverse proxy that lets you right-click any part of
a webpage, queue an edit suggestion, and read the suggestions later as
newline-delimited JSON from a FIFO. It is one self-contained Rust binary: the
browser JavaScript and CSS are embedded, and the target project needs no package,
plugin, or source-code change.

## Quick start

Start the project's normal development server, then put `web-fifo` in front of
it:

```sh
hugo server --port 1313
web-fifo proxy http://127.0.0.1:1313
open http://127.0.0.1:3939
```

In another terminal, let an agent or a person wait for feedback:

```sh
cat .web-fifo
```

The reader receives every currently queued suggestion in submission order and
then exits. If the queue is empty, it waits for the next suggestion.

Right-click an element to open **Suggest Edit:**. Select words in a long paragraph
before right-clicking to display and include the exact excerpt. Enter submits;
Shift+Enter inserts a newline. Clicking outside, Escape, or Cancel closes the
editor. Shift+right-click leaves the browser's native context menu available.

## Installation

Build or install with a current stable Rust toolchain:

```sh
cargo install --path .
```

The eventual package repository is `https://github.com/rot256/web-fifo`; this
checkout does not initialize or contact that repository.

## Projects and frameworks

The proxy works with any HTTP development server. Only browse through the
`web-fifo` address.

```sh
# Hugo
hugo server --port 1313
web-fifo proxy http://127.0.0.1:1313

# Python static server
python -m http.server 8000
web-fifo proxy http://127.0.0.1:8000

# Vite
npm run dev -- --port 5173
web-fifo proxy http://127.0.0.1:5173

# A Rust application
cargo run --bin my-server
web-fifo proxy http://127.0.0.1:3000
```

Ordinary request methods, bodies, response headers, redirects, binary content,
and WebSocket upgrades are forwarded. Successful, uncompressed `text/html`
responses up to 2 MiB receive this development-only script:

```html
<script type="module" src="/_web-fifo/client.js"></script>
```

Visiting the upstream server directly leaves the page unchanged. Production and
release builds are not modified; injection happens at runtime only while the
developer deliberately browses through `web-fifo proxy`.

## Script-tag fallback

If a reverse proxy is inconvenient, run the API/client service separately:

```sh
web-fifo serve --listen 127.0.0.1:3939
```

Add the script only to a development page:

```html
<script type="module" src="http://127.0.0.1:3939/_web-fifo/client.js"></script>
```

Loopback origins (`localhost`, `127.0.0.1`, and `::1`) are allowed by default.
Explicitly allow another exact origin when needed:

```sh
web-fifo serve --allow-origin https://preview.example
```

## CLI

```text
web-fifo [OPTIONS] proxy <UPSTREAM>
web-fifo [OPTIONS] serve

--listen <ADDRESS>       default: 127.0.0.1:3939
--fifo <PATH>            default: .web-fifo
--allow-origin <URL>     repeatable exact origin for script-tag mode
```

The upstream must be an `http://` URL. A base path is supported. `/_web-fifo/`
is reserved on the proxied site for the embedded client and API.

## FIFO and JSON contract

The FIFO is created with mode `0600`. `web-fifo` refuses to replace a regular
file at the configured path. Up to 500 records remain in memory until a reader
opens the FIFO; they are lost when the process exits.

Each line is one version-1 JSON object:

```json
{
  "version": 1,
  "id": "47746edb-e8e4-45c1-96bc-73a788ad969d",
  "timestamp": "2026-08-29T12:34:56Z",
  "comment": "Rewrite this sentence.",
  "page": {
    "url": "http://127.0.0.1:3939/guide/",
    "title": "Guide"
  },
  "target": {
    "selector": "#intro",
    "tag": "p",
    "id": "intro",
    "classes": ["lead"],
    "selectedText": "the exact selected sentence",
    "text": "The surrounding element text...",
    "html": "<p id=\"intro\" class=\"lead\">...</p>"
  },
  "pointer": {
    "page": {"x": 931, "y": 486},
    "viewport": {"x": 931, "y": 486},
    "target": {"x": 18, "y": 12},
    "scroll": {"x": 0, "y": 0},
    "viewportSize": {"width": 1280, "height": 720},
    "targetSize": {"width": 640, "height": 96},
    "devicePixelRatio": 1
  }
}
```

API endpoints are `GET /_web-fifo/api/status` and
`POST /_web-fifo/api/comments`. Requests are limited to 64 KiB. Selected text,
element text, HTML, and comments are bounded before queueing.

## Limitations

- This is a local development tool. It binds to loopback by default and does not
  provide authentication or TLS.
- Strict Content Security Policies that require a per-request script nonce can
  block the injected module. Use the explicit development script tag with the
  appropriate nonce, or relax the development CSP for the client URL.
- Encoded or oversized HTML is passed through without injection and produces a
  warning. Non-HTML content is never modified.
- Queued records are intentionally in-memory, not durable storage.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
uv run playwright install chromium
uv run pytest
cargo build --release
```

Licensed under the MIT License.
