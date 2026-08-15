# sqlake — Design

A mouse-friendly database client that runs in the terminal.

Scope is deliberately narrow: PostgreSQL and BigQuery, built as a personal tool. Where a
trade-off exists, prefer "hard to break" and "possible to finish" over "extensible".

**The code is the primary documentation.** What has been built states its own reasoning in doc
comments, and is deliberately not repeated here — a summary of working code is a copy that
drifts. What is left in this document is the part nothing has built yet, plus the decisions a
type cannot make on its own. [Agent surface](design-agent.md) does the same for its subject.

---

## 1. Principles

| Principle | Why |
| --- | --- |
| **Keep UI and logic strictly separate** | If the domain layer stays UI-agnostic, a bad call in the TUI layer cannot take the whole project down with it |
| **Mouse and keyboard produce the same intents** | Nothing should be reachable by mouse only. Mouse dies over SSH and under some tmux configurations |
| **Express use case inputs and outputs as types** | Making "before" and "after" distinct types turns a skipped step into a compile error |
| **Own no editor; delegate to `$EDITOR`** | A built-in editor cannot beat the user's existing neovim setup. Not writing one is the best outcome |
| **Express driver differences as `Capabilities`, never as UI branches** | BigQuery has no indexes and no triggers. Once `if bigquery` appears in the UI layer, the design is gone |
| **Confirm anything that costs money or destroys data** | Prevent BigQuery full-scan accidents structurally, not by convention |
| **Depend on as few external crates as possible** | The ratatui ecosystem is useful but thinly maintained in places. Write what should be written; delegate what can be delegated |
| **v1 leans read-only** | DDL and DML are available through SQL. Direct cell editing in the UI is not built |

---

## 2. Workspace layout

```
sqlake/
├── Cargo.toml                    # workspace
├── docs/
└── crates/
    ├── sqlake/                   # bin: CLI args, dependency wiring, startup
    ├── sqlake-core/              # domain types, Driver/Session traits. No DB dependencies
    ├── sqlake-app/               # use cases, state, actions. UI-agnostic and testable
    ├── sqlake-tui/               # rendering and input, on ratatui
    ├── sqlake-api/               # agent surface: protocol, schema, socket client and server
    ├── sqlake-mcp/               # agent surface: MCP stdio server over sqlake-api
    ├── sqlake-config/            # profile and settings persistence, secret resolution
    ├── sqlake-store/             # SQLite: history, templates, session restore
    ├── sqlake-driver-postgres/
    ├── sqlake-driver-bigquery/
    └── sqlake-driver-mock/       # for UI development and tests. Every screen works with no DB
```

`crates/` is the list of what exists; the rest are created by the milestone that first needs
one. An empty placeholder crate is dead weight and hides which parts are real.

Dependencies flow one way:

```
              ┌ sqlake-tui ┐
sqlake(bin) ──┤            ├──→ sqlake-app ──→ sqlake-core ←── sqlake-driver-*
              └ sqlake-api ┘         ↘ sqlake-config, sqlake-store
                    ↑
              sqlake-mcp
```

`sqlake-tui` and `sqlake-api` are **peers**: two front-ends over the same application layer,
neither depending on the other (§8). The TUI only needs to know how to start a listener, which
is `sqlake-api`'s job.

---

## 3. Domain model (`sqlake-core`)

`Driver`/`Session`, `Capabilities`, `Value`, `Ident` and the tree types live in
`crates/sqlake-core/` and carry their reasoning as doc comments. What follows is the part of
the model nothing has built.

### 3.1 Table definition (feature 5)

```rust
pub struct TableDetail {
    pub table: TableRef,
    pub kind: RelationKind,           // Table | View | MatView | Routine | External
    pub comment: Option<String>,
    pub columns: Vec<ColumnDef>,
    pub sections: Vec<DetailSection>, // variable; drivers fill in only what they have
    pub ddl: Option<String>,
    pub stats: Vec<(String, String)>, // row count, size, last modified, ...
}

pub struct DetailSection { pub title: String, pub table: ResultSet }
```

Making `sections` an array of `ResultSet` means indexes, triggers, constraints, partitioning
and clustering all render through **the same data grid**. BigQuery simply fills in
partitioning and clustering and leaves the rest empty.

---

## 4. Types still to be built (`sqlake-app`)

Every operation in the app layer is a `UseCase` (`crates/sqlake-app/src/usecase/`) whose input
and output are expressed as types, so a skipped step is a compile error rather than a runtime
surprise.

### 4.1 Stages as types

| Pipeline | Stages |
| --- | --- |
| SQL | `RawSql` → `ValidatedSql` → `PreparedSql` → **`ApprovedQuery`** |
| Connection info | `Profile` → **`ResolvedProfile`** |
| Templates | `Template` → `BoundTemplate` → **`RawSql`** |

The guarantees each stage carries:

| Type | Invariant |
| --- | --- |
| `ValidatedSql` | Parsed; single vs. multiple statements determined |
| `PreparedSql` | Parameters bound; implicit `LIMIT` applied |
| `ApprovedQuery` | Estimated cost within threshold, or explicitly approved by the user |
| `ResolvedProfile` | Secrets resolved from keyring or command; subject to `zeroize` |

`Session::execute` accepts only an `ApprovedQuery`, so **there is no code path that executes
without estimating.** The BigQuery billing accident is prevented structurally.

Conversions go through `TryFrom` or a dedicated function carrying a failure reason, and
**constructors are not `pub`**: `ApprovedQuery::new` is callable only from the approval logic
in the same module. The stages already built — `Ident → QuotedIdent` and
`RowBatch → ResultSet → PagedResult → RenderedGrid` — follow the same pattern and state their
own invariants.

### 4.2 "Needs approval" is an output, not an error

```rust
pub enum RunQueryOutput {
    Started       { handle: QueryHandle },
    NeedsApproval { estimate: Estimate, prepared: PreparedSql },
}
```

"Over the threshold, so confirmation is needed" is not a failure — it is **a normal branch**.
The UI shows a confirmation dialog on `NeedsApproval` and calls the same use case again with
`Approval::Approved`.

---

## 5. Screen layout

Where the layout is heading. Today's is the left half of it: one tree pane and one grid pane,
with the cell-detail pane arriving in M3 and the SQL and definition tabs in M4 and M5.

```
┌ sqlake ── [● prod-pg] [○ bq-analytics] [+] ─────────────────────── ⚙ ─┐
│┌ Explorer ──────┐┌ [Preview: users ×] [SQL#1 ×] [Definition ×] [+] ──┐│
││ ▾ prod-pg      ││ ┌ id ▲ │ email         │ created_at       │ ...  ┐││
││  ▾ public      ││ │    1 │ a@example.com │ 2026-01-02 10:00 │      │││
││   ▸ users      ││ │    2 │ b@example.com │ 2026-01-02 10:04 │      │││
││   ▸ orders     ││ └──────┴───────────────┴──────────────────┴──────┘││
││  ▸ analytics   ││ ┌ Cell / Row detail ─────────────────────────────┐││
│└────────────────┘│ └────────────────────────────────────────────────┘││
│                  └──────────────────────────────────────────────────┘│
│ ✓ 1,234 rows · 82ms · scanned 12.4MB          [Cancel] [History] [?] │
└──────────────────────────────────────────────────────────────────────┘
```

---

## 6. Mouse and keyboard

`KEYMAP` in `crates/sqlake-tui/src/input.rs` is the list of bindings, and the two coverage
tests beside it are what keep it complete: one asserts that every capability reachable with
the mouse has a key, the other that the key produces the capability it names. Adding a target
or a gesture stops the crate compiling until it has a sample.

What that mechanism does not cover, because it has not been built:

- Mouse capture takes native text selection away from the terminal. The plan is a permanent
  hint in the status bar ("Shift — or Option — + drag to select") plus **OSC 52 copy** of a
  cell, a row or a whole result, which works over SSH (M3). `--no-mouse` disables capture
  entirely and exists today.
- Some terminals and tmux configurations cannot deliver right-click, so every context menu
  entry (M3) also needs a key binding. The coverage test enforces that only once the menu is a
  hit target.
- Reserved keys: `e` opens `$EDITOR` (M4), `Ctrl-p` the command palette (M4).

---

## 7. No SQL editor: delegate to `$EDITOR` (feature 4)

Write the buffer to a temporary file, launch `$EDITOR`, and read it back when the editor
exits. The same mechanism as `git commit`.

### 7.1 What this buys

- Syntax highlighting, completion, snippets, LSP (`sqls`, `sqlfluff`), key bindings, macros —
  **the user's existing neovim configuration applies unchanged.**
- No dependency on any editor crate, and no multi-line editing, undo/redo or search to write.
- The temporary file carries a `.sql` extension, so filetype detection works automatically.

### 7.2 Flow

1. `e`, or a click on the editor area → `Action::EditExternally { tab }`.
2. **The main loop handles this synchronously** — it hands the terminal over, so ordering
   matters and it must not go through the store.
   - Release `TerminalGuard`
   - Write the buffer to `~/.local/state/sqlake/scratch/{tab_id}.sql`
   - `Command::new(editor).args(&args).arg(path).status()` and wait
   - Re-acquire `TerminalGuard` and `terminal.clear()` for a full redraw
3. Read the file back into a `RawSql`. If it changed, record it as a draft in history.
4. `Ctrl-Enter` or the run button hands off to the `RunQuery` use case: estimate, approve, run.

### 7.3 Caveats

- Restore runs through the same path as the panic hook, so an editor that dies
  abnormally still leaves a usable screen.
- Editor resolution order: the `editor` setting, `$VISUAL`, `$EDITOR`, then `vi`. `editor_args`
  (e.g. `["--wait"]`) exists for GUI editors.
- The store task keeps running while the editor is open, so streams from running queries are
  still consumed. The display catches up on return.
- Scratch files are persisted per tab and reloaded on session restore.

### 7.4 What the SQL tab does own

Everything except editing: a read-only preview of the buffer, run and cancel, the estimated
byte count, error line highlighting, the result grid, and tab management.

### 7.5 Possible later (out of scope for v1)

A watch mode: use `notify` to watch the scratch file and re-run automatically on every save,
so neovim can stay open in another tmux pane. Genuinely useful for a personal tool, but it
adds a dependency.

---

## 8. A second front-end: the agent surface

The application layer has no idea a terminal exists, so the TUI is one front-end over it
rather than the only possible one. An AI agent driving sqlake — listing tables, previewing
data, running a query — is **a second front-end over the same `sqlake-app`, not new logic**.

Two things follow, and both are load-bearing:

- Nothing in the agent surface may construct a query, know a driver, or format a result. If
  something there needs a new use case, the interactive client is missing a feature too.
- The guards already in the type system apply unchanged. `Session::execute` accepts only an
  `ApprovedQuery`, so an agent cannot run an unestimated BigQuery scan any more than a human
  can; what differs is that approval is granted by a byte budget rather than by a dialog, and
  that going over it returns `NeedsApproval` (§4.2) for a human to answer.

The surface is a socket API against a running session, thin noun-verb subcommands over it, and
an MCP server wrapping the same client — modelled on herdr. Attaching to a running session is
the point: connections behind a bastion, an MFA prompt or a `gcloud auth` flow are expensive
to establish, and for anything interactive, impossible to re-establish per command.

Detail, including the safety policy for agents, is in [design-agent.md](design-agent.md).

---

## 9. Configuration and secrets (`sqlake-config`)

```
~/.config/sqlake/
├── config.toml        # theme, key map, editor, page size, cost thresholds
├── connections.toml   # connection profiles; never plaintext passwords
└── tunnels.toml       # proxy and tunnel definitions (feature 6)
~/.local/state/sqlake/
├── state.db           # SQLite: history, templates, session restore
├── scratch/           # per-tab working files (.sql)
└── sqlake.log
```

```toml
# connections.toml
[[connection]]
id = "prod-pg"
name = "Prod (read replica)"
driver = "postgres"
host = "127.0.0.1"
port = 5432
database = "app"
user = "readonly"
sslmode = "verify-full"
password = { keyring = true }   # or { command = "op read op://..." } / { env = "PGPASSWORD" }
tunnel = "prod-bastion"         # references a name in tunnels.toml
readonly = true
color = "red"                   # colour the production tab, to prevent accidents

[[connection]]
id = "bq-analytics"
driver = "bigquery"
project = "my-project"
location = "asia-northeast1"
auth = { adc = true }           # or { service_account = "/home/me/.config/.../sa.json" }
max_bytes_billed = "20GB"       # queries above this are refused
```

**Secrets are never stored in plaintext.** Resolution order is `keyring` (macOS Keychain),
then a `command`, then an environment variable. The `Profile` → `ResolvedProfile` conversion
is that boundary, and `ResolvedProfile` is wiped from memory via `zeroize`. Connection-string
masking lives in exactly one function and never reaches the log.

---

## 10. Drivers

### 10.1 PostgreSQL

- Hold **two connections: one for queries, one for metadata**, so that expanding the tree is
  never blocked behind a long-running query. Both sit behind one `Session`.
- Cancellation uses `client.cancel_token()`; connect also sets a `statement_timeout`.
- Show the `EXPLAIN` row estimate first; run an exact `COUNT(*)` only on explicit request.

### 10.2 BigQuery

- **Where the crate lags the API, work around it in the driver and say so at the call.** It
  already does in one place: `datasets.list` models the dataset array as a plain `Vec`, and
  the API omits that key when a project has none, so an empty project's perfectly good 200
  arrives as a decode error. That is why connecting treats only an explicit refusal as a
  failure. The escape hatch when a workaround is not enough is a thin `reqwest` call.
- **Preview uses `tabledata.list`, which is not billed as a query.** Implementing it as
  `SELECT *` would incur scan costs on every preview. The correct implementation of
  `preview()` for BigQuery issues no SQL.
- Table definitions come from `tables.get` — schema, partitioning, clustering, row count and
  byte size, all free.
- `estimate()` is `jobs.insert(dryRun)`. Since `execute()` accepts only `ApprovedQuery`, there
  is no path that skips estimation; `maximumBytesBilled` is always set as a second layer.
- Cancellation is `jobs.cancel`. `location` comes from the profile, with inference from the
  dataset.
- `RECORD` and `REPEATED` fields are flattened into dotted column names (`user.name`) in the
  grid, with the original nesting shown in the detail popover.

---

## 11. Proxies and tunnels (feature 6)

```rust
#[async_trait]
trait Tunnel {
    async fn open(&self) -> Result<Endpoint>;   // returns the host:port to actually connect to
    async fn close(self);
}
```

- v1 implements **one type: `command`**. Spawn an arbitrary process and wait for a port to
  listen. That covers `cloud-sql-proxy`, `ssh -L` and `cloudflared` without new dependencies.
- BigQuery (HTTPS) uses `reqwest`'s `Proxy` support (HTTP or SOCKS5).
- Tunnels are named and reference-counted; when the last connection closes, the child process
  is killed by process group so no orphans are left behind.
- Leave room to add a native SSH tunnel via `russh` later.

```toml
# tunnels.toml
[[tunnel]]
name = "prod-bastion"
type = "command"
command = ["ssh", "-N", "-L", "15432:db.internal:5432", "bastion"]
wait_for = "127.0.0.1:15432"
timeout = "20s"
```

---

## 12. History and templates (features 7 and 8)

One SQLite file (`rusqlite`, bundled).

```sql
CREATE TABLE query_history (
  id INTEGER PRIMARY KEY, connection_id TEXT, driver TEXT,
  sql TEXT NOT NULL, started_at INTEGER, duration_ms INTEGER,
  row_count INTEGER, bytes_processed INTEGER,
  status TEXT,            -- ok | error | cancelled
  error TEXT, pinned INTEGER DEFAULT 0
);
CREATE VIRTUAL TABLE query_history_fts USING fts5(sql, content='query_history');

CREATE TABLE templates (
  id INTEGER PRIMARY KEY, name TEXT UNIQUE, body TEXT,
  driver TEXT,            -- NULL means any
  tags TEXT, created_at INTEGER, updated_at INTEGER
);
```

- The history tab searches incrementally through FTS5. Clicking a row writes it to a scratch
  file; starring it promotes it to a template.
- Templates carry `{{param}}` placeholders, staged like §4.1. Placeholders such as `{{table}}`
  are completed from the currently selected tree node.
- **Failed queries are recorded too** — in a personal tool, the failures are the useful part.

---

## 13. Testing

The testing rules that apply to every commit are in [CLAUDE.md](../CLAUDE.md). `sqlake-conformance`
holds the shared **driver conformance suite**: one set of cases, run against the mock and, via
`testcontainers`, against PostgreSQL. BigQuery's leg, against an emulator, arrives with the
driver in M2, so that "the driver behaves" means the same thing for all three.

---

## 14. Milestones

| # | Content | Done when |
| --- | --- | --- |
| **M0 — Foundation** ✅ | — | Done. `crates/` and `git log` are the record |
| **M1 — Connection management** ✅ | — | Done. `crates/` and `git log` are the record |
| **M2** | Table list (feature 2) — [design-m2.md](design-m2.md) | Lazy tree expansion for pg and bq, filter search. A second driver also gives `Capabilities` a second answer, which the mock alone cannot |
| **M3** | Table preview (feature 3) | Paging, sorting, cell detail, range selection, CSV/JSON copy via OSC 52, context menu |
| **M4** | Running SQL (feature 4) | `$EDITOR` launch and terminal restore, estimate → approve → run, cancellation, multiple tabs, error line display. The first confirmation dialogs — `Modal` exists, and until now only a failed connection raises one |
| **M5** | Table definitions (feature 5) | Columns, indexes, triggers, constraints, partitioning, DDL |
| **M6** | Proxy settings (feature 6) | `command` tunnels, HTTP proxy |
| **M7** | SQL templates (feature 7) | Save, parameter entry, insert from the palette |
| **M8** | Query history (feature 8) | FTS search, re-run, promote to template |

The agent surface (§8) runs as a track alongside these rather than after them, because each
of its parts becomes possible at a different point:

| # | Lands after | Content |
| --- | --- | --- |
| **A1** | M2 | Read-only CLI and socket API. Needs no terminal, and exercises `sqlake-app` through a second front-end while the interactive client is still half-written |
| **A2** | M4 | Query execution over the API, where `ApprovedQuery` and the byte budget arrive |
| **A3** | A2 | MCP server |

Execution order: **M0 → M1 → M2 → A1 → M3 → M4 → A2 → A3 → M5 → M6 → M7 → M8.**

---

## 15. Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Terminal state corrupted after returning from the external editor | unusable | One restore path, shared with the panic hook (`TerminalGuard`) |
| `$EDITOR` is a GUI editor and returns immediately | annoyance | `editor_args` can supply a `--wait` equivalent; warn when the editor exits instantly |
| Mouse capture steals native text selection | annoyance | OSC 52 copy as standard, a permanent hint about shift-drag, and `--no-mouse` |
| Terminals and tmux configurations without right-click or hover | missing features | Every feature has a key binding, enforced by the coverage tests (§6) |
| Too many staged types make the code verbose | velocity | Keep to the pipelines listed in §4.1; add a stage only when skipping it would cause a real accident |
| BigQuery billing accident | real cost | `tabledata.list` for preview; `ApprovedQuery` enforced by the type system; `maximumBytesBilled` |
| Unknown PostgreSQL type crashes the client | unusable | `Value::Opaque` fallback enforced by the type system |
| Accidental operation against production | real damage | `color` and `readonly` on the profile; read-only connections set `default_transaction_read_only = on` |

---

## 16. Dependencies

The UI layer reaches for **no other widget crate**: the data grid, tree, modals, scrollbar
handling and focus are written here, on top of `ratatui` and `crossterm`, because each has
requirements specific to this project and each is a few hundred lines at most. `rat-ftable`
is the fallback if the grid cannot be made fast enough.

The workspace `Cargo.toml` is the record of what is in use. What is not there yet, with the
choice already made:

```toml
# persistence (M7, M8)
rusqlite = { version = "0.37", features = ["bundled"] }

# proxying (M6) — BigQuery is HTTPS, so its tunnel is a reqwest concern
reqwest = { version = "0.12", features = ["socks"] }
```

For `ValidatedSql` (M4), start by checking whether splitting on semicolons and classifying the
statement is enough; reach for `sqlparser` only if it is not.

---

## 17. References

- [ratatui](https://ratatui.rs/) and [awesome-ratatui](https://github.com/ratatui/awesome-ratatui)
- [rainfrog](https://github.com/achristmascarl/rainfrog) — a PostgreSQL TUI in Rust and
  ratatui with a mouse mode. The closest reference implementation
- [rat-salsa / rat-widget](https://lib.rs/crates/rat-widget) — fallback for large tables
