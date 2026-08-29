# Puzzle

Puzzle is a small terminal client for a shared Codex app-server. It lists the
primary Codex thread associated with each Muster window in recent-activity
order and updates the view as thread status changes.

Run Puzzle with its default Muster socket:

```console
cargo run
```

Pass another Unix socket as the first argument when needed:

```console
cargo run -- /path/to/app-server.sock
```

Use `j` and `k` or the arrow keys to scroll. Press `q`, Escape, or Control-C to
exit.
