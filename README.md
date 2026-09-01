# Puzzle

Puzzle is a small terminal client for a shared Codex app-server. It lists the
primary Codex thread associated with each Muster window in recent-activity
order and updates the view as thread status changes. Inside tmux, Puzzle opens
a viewport beside the list. Selecting a window starts or finds a persistent
Codex client and swaps its live pane into that viewport.

Run Puzzle with its default Muster socket:

```console
cargo run
```

Pass another Unix socket as the first argument when needed:

```console
cargo run -- /path/to/app-server.sock
```

Use `j` and `k` or the arrow keys to move through the windows, wrapping at both
ends. Each movement opens the selected window before accepting another key.
The first visit starts a client in the detached `puzzle-parking` tmux session;
later visits swap the existing client immediately. Enter moves focus into the
displayed client. Press `q`, Escape, or Control-C to exit Puzzle without
stopping those clients.
