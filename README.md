# Puzzle

Puzzle is a small terminal client for a shared Codex app-server. The current
version displays the number of active, durable root threads and updates the
count as thread status changes.

Run Puzzle with its default Muster socket:

```console
cargo run
```

Pass another Unix socket as the first argument when needed:

```console
cargo run -- /path/to/app-server.sock
```

Press `q`, Escape, or Control-C to exit.
