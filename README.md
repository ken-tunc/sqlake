# sqlake

A mouse-friendly database client for the terminal.

Scope is deliberately narrow — PostgreSQL and BigQuery, built as a personal tool. Every action
is reachable with the mouse, and every one of those actions also has a key binding.

> **Status: M0 through M3 are done, and the read-only agent surface with them.** PostgreSQL and
> BigQuery both connect, the explorer browses them, and a preview pages, sorts, shows a cell in
> full and copies as CSV or JSON. Running your own SQL is M4.

## Try it

```sh
cargo run -- --mock       # the built-in mock database, with no server to set up
cargo run                 # the first connection in connections.toml
cargo run -- --no-mouse   # when the terminal or tmux swallows mouse events

# and without a terminal at all
cargo run -- --mock table preview public.users --limit 5
```

Click a schema's `▸` to expand it, double-click a table to open it. `Tab` moves between the
panes; `Space` and `Enter` do in the tree what a click and a double-click do. In the grid,
`J`/`K` and `H`/`L` move the cell cursor and `Shift` with an arrow extends a selection; the
arrows scroll, and scrolling towards the end fetches the next page — `m` asks for it outright.
`s` sorts the selected column, `Enter` shows a cell in full, `y` copies the selection as CSV
and `a` copies everything — shift on either for JSON. `.` opens the context menu, `q` quits.
Anything reachable with the mouse has a key binding, and a test enforces it rather than a
promise.

Logs go to `$XDG_STATE_HOME/sqlake/sqlake.log` and never to the screen.

## Design

- [Design](docs/design.md) — architecture, driver model, type design, milestones
- [Agent surface](docs/design-agent.md) — driving sqlake from an AI agent

The code is the primary documentation: the design documents hold what has not been built and
the reasons behind decisions that a type cannot state for itself. What a finished milestone
built is described by the crates it produced, not by a document beside them — which is why
each milestone's own document is deleted when it is done.

## Features

1. **Connection management** — M1 ✅
2. **Table listing** — M2 ✅
3. **Table preview** — M3 ✅
4. **SQL execution** (editing delegated to `$EDITOR`) — M4
5. **Table definitions** — columns, indexes, triggers — M5
6. **Proxy and tunnel settings** — M6
7. **SQL templates** — M7
8. **Query history** — M8

Plus an agent surface: the same operations as a socket API, CLI subcommands and an MCP server,
so an AI agent can use the connections and guardrails the interactive client already has. It
lands alongside the milestones above rather than after them: read-only access is built, and
query execution follows M4.

## License

MIT
