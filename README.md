# sqlake

A mouse-friendly database client for the terminal.

Scope is deliberately narrow — PostgreSQL and BigQuery, built as a personal tool. Every action
is reachable with the mouse, and every one of those actions also has a key binding.

> **Status: M0 is done.** `cargo run` opens the client against a built-in mock database. No real
> driver yet, so it talks to nothing: PostgreSQL arrives in M1 and BigQuery in M2.

## Try it

```sh
cargo run                 # the mock database, in the explorer
cargo run -- --no-mouse   # when the terminal or tmux swallows mouse events
```

Click a schema's `▸` to expand it, double-click a table to open it. `Tab` moves between the
panes; `Space` and `Enter` do in the tree what a click and a double-click do. In the grid,
`J`/`K` and `H`/`L` move the cell cursor, the arrows scroll, `s` sorts the selected column and
`m` fetches the next page. `q` quits. Anything reachable with the mouse has a key binding —
a test enforces it rather than a promise.

Logs go to `$XDG_STATE_HOME/sqlake/sqlake.log` and never to the screen.

## Design

- [Design](docs/design.md) — architecture, driver model, type design, milestones
- [M0 — Foundation](docs/design-m0.md) — the milestone just finished
- [Agent surface](docs/design-agent.md) — driving sqlake from an AI agent

The code is the primary documentation: the design documents hold what has not been built and
the reasons behind decisions that a type cannot state for itself.

## Features

1. **Connection management** — M1
2. **Table listing** — M2
3. **Table preview** — M3
4. **SQL execution** (editing delegated to `$EDITOR`) — M4
5. **Table definitions** — columns, indexes, triggers — M5
6. **Proxy and tunnel settings** — M6
7. **SQL templates** — M7
8. **Query history** — M8

Plus an agent surface: the same operations as a socket API, CLI subcommands and an MCP server,
so an AI agent can use the connections and guardrails the interactive client already has. It
lands alongside the milestones above rather than after them — read-only access after M2, query
execution after M4.

## License

MIT
