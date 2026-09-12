# sqlake

A mouse-friendly database client for the terminal.

Scope is deliberately narrow — PostgreSQL and BigQuery, built as a personal tool. Every action
is reachable with the mouse, and every one of those actions also has a key binding.

> **Status: everything but proxies and tunnels.** Connecting, browsing, previewing, running
> your own SQL, what a relation *is*, saved statements and query history are all built, and so
> is the whole agent surface — a socket API, CLI subcommands and an MCP server. What is left is
> M6: reaching a database through `ssh -L` or `cloud-sql-proxy`.

## Try it

```sh
cargo run -- --mock       # the built-in mock database, with no server to set up
cargo run                 # the first connection in connections.toml
cargo run -- --no-mouse   # when the terminal or tmux swallows mouse events

# and without a terminal at all
cargo run -- --mock table list public
cargo run -- --mock table preview public.users --limit 5
cargo run -- --mock table describe public.users
cargo run -- --mock query estimate 'select count(*) from public.users'
cargo run -- --mock template list
cargo run -- history search orders
```

Each of those answers one request and exits. Running a statement needs a session that outlives
the command, because `query run` answers a handle and a handle nothing can ask about afterwards
is no answer at all:

```sh
sqlake --session work &                       # a client listening on a socket
sqlake --session work query run 'select 1'    # answers a handle…
sqlake --session work query wait <id>         # …and this blocks for the rows
```

Attached commands use the session's own connections, credentials and history rather than
opening a second set. `sqlake mcp` speaks MCP over stdio and offers the same operations as
tools, against the same session.

## Getting around

Click a schema's `▸` to expand it, double-click a table to open it. `Tab` moves between the
panes and `Ctrl-Tab`, `[` and `]` between the tabs; `Space` and `Enter` do in the tree what a
click and a double-click do.

| | |
| --- | --- |
| **Explorer** | `d` shows what a relation is; `/` searches the tree; `c` opens a connection and `D` closes one |
| **Grid** | `J`/`K` and `H`/`L` move the cell cursor, `Shift` with an arrow extends a selection; the arrows scroll, and scrolling towards the end fetches the next page — `m` asks for it outright. `s` sorts the selected column, `Enter` shows a cell in full, `y` copies the selection as CSV and `a` copies everything — `Shift` on either for JSON |
| **SQL** | `n` opens a tab, `e` opens `$EDITOR` on its buffer, `r` runs what is in it. A query over the byte budget asks first |
| **Definitions** | `{` and `}` walk the sections — columns, indexes, triggers, constraints, partitioning, and a `CREATE` statement built from the catalogue |
| **Saved statements** | `Ctrl-p` opens the palette, `Ctrl-s` keeps what is in the buffer, `Ctrl-d` deletes one. A template's `{{placeholders}}` are asked for and quoted by the rules of the connection it is going to |
| **History** | `Ctrl-r` opens what has been run — failures and cancellations too — `u` puts one back in a buffer, and `Ctrl-s` keeps it as a template |
| | `.` opens the context menu, `q` quits |

Anything reachable with the mouse has a key binding, and a test enforces it rather than a
promise.

## Where it keeps things

| | |
| --- | --- |
| `$XDG_CONFIG_HOME/sqlake/connections.toml` | the profiles, and where their secrets come from |
| `$XDG_CONFIG_HOME/sqlake/config.toml` | settings that are not about one connection |
| `$XDG_STATE_HOME/sqlake/library.db` | saved statements and query history, owner-readable only |
| `$XDG_STATE_HOME/sqlake/sqlake.log` | the log, which never goes to the screen |
| `$XDG_RUNTIME_DIR/sqlake/` | a session's socket, when `--session` opened one — under the state directory instead on macOS, which sets no runtime directory |

The XDG variables are honoured on every platform, macOS included: this is a file people edit by
hand, and `~/Library/Application Support` is not where anyone edits one.

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
4. **SQL execution** (editing delegated to `$EDITOR`) — M4 ✅
5. **Table definitions** — columns, indexes, triggers, generated DDL — M5 ✅
6. **Proxy and tunnel settings** — M6
7. **SQL templates** — M7 ✅
8. **Query history** — M8 ✅

Plus an agent surface — the same operations as a socket API, CLI subcommands and an MCP server,
so an AI agent uses the connections and guardrails the interactive client already has. It ran
alongside the milestones above rather than after them, and is built: reads, query execution
under the same byte budget, and MCP.

M6 is last because nothing else needed it, and it is the one feature whose absence is obvious
the moment somebody does.

## License

MIT
