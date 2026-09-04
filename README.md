# Puzzle

Puzzle is a small terminal client for a shared Codex app-server. It lists the
primary Codex thread associated with each Muster window in recent-activity
order and updates the view as thread status changes. For each idle thread,
Puzzle asks Terra for a description of the latest completed turn in no more
than 40 characters. Summaries are generated on an ephemeral fork, cached under
the user's state directory, and never added to the source conversation.

Inside tmux, Puzzle opens a viewport beside the list. Selecting a window starts
or finds a persistent Codex client and swaps its live pane into that viewport.

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
displayed client. Press `/` to find windows by slug or displayed description;
type to narrow the list, use the arrow keys to move through matches, and press
Escape to clear the find. Press `q` or Escape outside find, or Control-C at any
time, to exit Puzzle without stopping those clients.
