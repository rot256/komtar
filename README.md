# komtar

> Tell the agent what to change on the page. Stupid simple, works with any framework, single binary:

Start your project's development server, then proxy it through Komtar:

```sh
komtar proxy http://127.0.0.1:5173
open http://127.0.0.1:3939
```

Ask your coding agent to wait for queued suggestions:

```sh
cat .komtar
```

Right-click any element to suggest an edit. Select text before right-clicking to
include that exact passage. Press Enter to queue the suggestion; Shift+Enter adds
a newline.

## Collaborative editing

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
