# sqlake — Design

A mouse-friendly database client that runs in the terminal.

Scope is deliberately narrow: PostgreSQL and BigQuery, built as a personal tool. Where a
trade-off exists, prefer "hard to break" and "possible to finish" over "extensible".

Milestone-level plans live alongside this document: [M0 — Foundation](design-m0.md).

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

## 2. Choosing the UI foundation (ADR-001)

**Decision: ratatui 0.30.x.**

### Candidates

| Candidate | Assessment |
| --- | --- |
| **ratatui 0.30.2** | 5.6M downloads/month, 5,878 dependent crates. More boilerplate than the alternatives, but it stays confined to `sqlake-tui`. **Adopted** |
| iocraft 0.8.4 | Declarative, flexbox layout — both appealing. But the only components are `View`, `Text` and `TextInput`: no table, tree, scrolling, modal or focus handling. What this project needs is widgets, not layout |
| cursive | Mouse-first by design, with event routing built in. But it is a poor fit for dense data grids, and its `cb_sink` callback model does not compose with a tokio-based design |
| rat-salsa / rat-widget | Serious work — event handling, focus and scrolling included. But 229 downloads/month and effectively a single maintainer. Kept as a fallback, not a foundation |
| reratui, rxtui, reactive_tui, revue | Several new "React-like" frameworks exist. All 0.x, single-author, no track record. Not a foundation |

### What decided it

1. **Mouse hit testing is easier on ratatui** — counterintuitive, and the deciding factor.
   iocraft computes layout inside Taffy, so a component can only read its own rectangle
   through `use_component_rect()`, which returns *the previous frame's* value. A hit-testing
   layer still has to be written by hand, but under worse conditions: one frame of lag and
   no z-ordering. ratatui is immediate mode, so the `Rect`s returned by `Layout::split()` are
   already in hand — hit testing is `rect.contains(pos)`, and z-order is draw order.

2. **There is a way out.** The parts exist in the ecosystem (`rat-ftable` for large tables,
   `tui-tree-widget`, `tui-overlay`, `rat-focus`). The plan is to write them, but a stuck
   component can be swapped for an existing crate. iocraft offers no such escape hatch.

3. **A reference implementation exists.** `rainfrog` (a PostgreSQL TUI in Rust and ratatui,
   with a mouse mode) solves the same problem and can be read when a decision is unclear.

### Policy on third-party widgets

| Part | Policy |
| --- | --- |
| SQL editor | **None. Delegated to `$EDITOR`** (§9). Zero dependency on editor crates |
| Data grid | **Own** — a thin wrapper over ratatui's `Table` and `TableState`. Width computation, per-type alignment and column resizing are all specific to this project. Swap in `rat-ftable` if performance demands it |
| Tree | **Own.** Holding the lazy-load state directly is simpler. A flattened `Vec<VisibleNode>` plus an offset is roughly 200 lines |
| Modal / menu | **Own** — `Clear` widget plus a centred `Rect`. Tens of lines |
| Scrollbar | ratatui's own `Scrollbar` plus hand-written hit regions |
| Focus | **Own** — a `FocusId` enum and an ordered array. No crate needed |

The result: the UI layer depends on **`ratatui` and `crossterm`, and nothing else**.

---

## 3. Workspace layout

```
sqlake/
├── Cargo.toml                    # workspace
├── docs/
└── crates/
    ├── sqlake/                   # bin: CLI args, dependency wiring, startup
    ├── sqlake-core/              # domain types, Driver/Session traits. No DB dependencies
    ├── sqlake-app/               # use cases, state, actions. UI-agnostic and testable
    ├── sqlake-tui/               # rendering and input, on ratatui
    ├── sqlake-config/            # profile and settings persistence, secret resolution
    ├── sqlake-store/             # SQLite: history, templates, session restore
    ├── sqlake-driver-postgres/
    ├── sqlake-driver-bigquery/
    └── sqlake-driver-mock/       # for UI development and tests. Every screen works with no DB
```

Dependencies flow one way:

```
sqlake(bin) → sqlake-tui → sqlake-app → sqlake-core ← sqlake-driver-*
                                     ↘ sqlake-config, sqlake-store
```

`sqlake-core` depends on little more than `serde`, `tokio` (sync only) and `async-trait`.
**Building `sqlake-driver-mock` in M0 is the key move** — with it, both the UI and the use
cases can be developed without standing up a database.

---

## 4. Domain model (`sqlake-core`)

### 4.1 Driver abstraction

```rust
#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DriverKind;                       // Postgres | BigQuery | Mock
    fn capabilities(&self) -> Capabilities;
    async fn connect(&self, p: &ResolvedProfile) -> Result<Box<dyn Session>>;
}

#[async_trait]
pub trait Session: Send + Sync {
    /// Lazy tree expansion. root → catalog → schema → table all go through this.
    async fn children(&self, of: &NodeRef) -> Result<Vec<TreeNode>>;

    /// Table definition (feature 5).
    async fn describe(&self, t: &TableRef) -> Result<TableDetail>;

    /// Table preview (feature 3). Some implementations issue no SQL at all.
    async fn preview(&self, t: &TableRef, req: &PageRequest) -> Result<ResultSet>;

    /// Estimation. pg: EXPLAIN, bq: dryRun.
    async fn estimate(&self, sql: &PreparedSql) -> Result<Estimate>;

    /// Execution. Accepts approved queries only (§5).
    async fn execute(&self, q: &ApprovedQuery) -> Result<QueryStream>;

    async fn close(self: Box<Self>);
}

pub struct QueryStream {
    pub meta:    ResultMeta,                   // column definitions, job id, start time
    pub batches: mpsc::Receiver<Result<RowBatch>>,
    pub cancel:  CancelHandle,                 // pg: CancelToken, bq: jobs.cancel
}
```

### 4.2 Capabilities

The UI consults this to decide what to show. `if driver == Postgres` never appears in the UI.

```rust
pub struct Capabilities {
    pub hierarchy: &'static [NodeKind],  // pg: [Database, Schema, Relation]
                                         // bq: [Project, Dataset, Table]
    pub indexes: bool,
    pub triggers: bool,
    pub constraints: bool,
    pub partitioning: bool,
    pub transactions: bool,
    pub cancel: bool,
    pub streaming: bool,                 // if false, fetch everything before displaying
    pub cost_estimate: bool,             // bq dryRun
    pub free_preview: bool,              // bq tabledata.list, which is not billed
    pub quote_style: QuoteStyle,         // "ident" vs `ident`
}
```

### 4.3 Value model

One representation so that database-specific types never leak into the UI. The property that
matters most is **never crashing on an unknown type**.

```rust
pub enum Value {
    Null,
    Bool(bool),
    Int(i64), Float(f64), Decimal(String),   // string, to avoid losing precision
    Text(String), Bytes(Bytes),
    Date(..), Time(..), Timestamp(..), Interval(..),
    Json(serde_json::Value),
    Array(Vec<Value>),
    Struct(Vec<(String, Value)>),            // bq RECORD, pg composite
    Opaque { type_name: String, text: String },  // escape hatch for unknown types
}
```

Numbers are right-aligned; `NULL` renders as a dimmed `∅`. `Struct` and `Array` collapse to
`{...}` and `[3 items]` in the grid, and clicking the cell opens a formatted-JSON popover.

### 4.4 Table definition (feature 5)

```rust
pub struct TableDetail {
    pub table: TableRef,
    pub kind: RelationKind,           // Table | View | MatView | Routine | External
    pub comment: Option<String>,
    pub columns: Vec<ColumnDef>,      // name, type, nullable, default, pk, comment, ordinal
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

## 5. Use cases and types (`sqlake-app`)

Every operation in the app layer is a `UseCase` whose input and output are expressed as types.
Making "before" and "after" distinct types turns a skipped step into a compile error.

```rust
#[async_trait]
pub trait UseCase {
    type Input;
    type Output;
    async fn execute(&self, input: Self::Input) -> Result<Self::Output, AppError>;
}
```

- Dependencies (`Session` handles, `Store`, `HistoryRepo`) are injected as struct fields.
- `Input` and `Output` are types specific to the use case. No tuples like
  `(String, u32, bool)`, and no generic `serde_json::Value`.
- Inject the mock driver and a use case can be verified in isolation, input to output.

### 5.1 Stages as types

| Pipeline | Stages |
| --- | --- |
| SQL | `RawSql` → `ValidatedSql` → `PreparedSql` → **`ApprovedQuery`** |
| Identifiers | `Ident` → **`QuotedIdent`** |
| Results | `RowBatch` → `ResultSet` → **`RenderedGrid`** |
| Connection info | `Profile` → **`ResolvedProfile`** |
| Templates | `Template` → `BoundTemplate` → **`RawSql`** |

The guarantees each stage carries:

| Type | Invariant |
| --- | --- |
| `ValidatedSql` | Parsed; single vs. multiple statements determined |
| `PreparedSql` | Parameters bound; implicit `LIMIT` applied |
| `ApprovedQuery` | Estimated cost within threshold, or explicitly approved by the user |
| `QuotedIdent` | Quoted and escaped |
| `ResultSet` | Column definitions and row count fixed |
| `RenderedGrid` | Column widths, alignment and truncation settled |
| `ResolvedProfile` | Secrets resolved from keyring or command; subject to `zeroize` |

Three things become **impossible to write**:

- `Session::execute` accepts only `ApprovedQuery`
  → **there is no code path that executes without estimating.** The BigQuery billing accident
  is prevented structurally.
- SQL assembly accepts only `QuotedIdent`
  → forgetting to quote is a compile error, so an upper-case table name cannot break a query.
- The UI receives only `RenderedGrid`
  → rendering code cannot start formatting `Value` on its own.

Conversions go through `TryFrom` or a dedicated function carrying a failure reason
(`fn prepare(sql: ValidatedSql, params: &Params) -> Result<PreparedSql, PrepareError>`), and
**constructors are not `pub`**: `ApprovedQuery::new` is callable only from the approval logic
in the same module.

### 5.2 "Needs approval" is an output, not an error

```rust
pub struct RunQueryInput {
    pub tab: TabId,
    pub sql: RawSql,
    pub approval: Approval,          // Ask | Approved { up_to_bytes: u64 }
}

pub enum RunQueryOutput {
    Started       { handle: QueryHandle },
    NeedsApproval { estimate: Estimate, prepared: PreparedSql },
}
```

"Over the threshold, so confirmation is needed" is not a failure — it is **a normal branch**,
so it is a variant of `Output` rather than an `Err`. The UI shows a confirmation dialog on
`NeedsApproval` and calls the same use case again with `Approval::Approved`.

### 5.3 Layout, and the relationship to `Action`

```
crates/sqlake-app/src/
├── action.rs            # intents raised by the UI
├── store.rs             # applies actions, invokes use cases, updates the snapshot
├── snapshot.rs
└── usecase/
    ├── mod.rs           # UseCase trait and AppError
    ├── connect.rs
    ├── expand_node.rs
    ├── preview_table.rs
    ├── describe_table.rs
    ├── run_query.rs
    ├── save_template.rs
    └── search_history.rs
```

`Action` is a raw intent from the UI ("this button was pressed"); a use case `Input` is
validated. **The UI never calls a use case directly.** The store builds an `Input` from an
`Action`, invokes the use case, and folds the `Output` into the `Snapshot`. That step is one
more raw-to-validated conversion.

---

## 6. Concurrency and state

### 6.1 Topology

```
   ┌─────────────────── tokio multi-thread runtime ───────────────────┐
   │                                                                  │
 TUI main loop                    Session actor (one task per conn)   │
   │  dispatch(Action) ─────────────►  mpsc<SessionCmd>               │
   │                                          │                       │
   │                                   UseCase / Driver / DB          │
   │                                          │                       │
   │  ◄─── watch<Arc<Snapshot>> ─── Store task ◄── Event              │
   └──────────────────────────────────────────────────────────────────┘
```

- The **store task** solely owns `AppState`, applies `Action` through use cases, and publishes
  an immutable `Arc<Snapshot>` on a `tokio::sync::watch` channel.
- Heavy data such as row buffers lives behind `Arc<Vec<Row>>` and is shared across snapshots,
  so cloning a snapshot is effectively a pointer copy.
- Each connection is serialised by one task. Cancellation bypasses the queue: the UI invokes
  `CancelHandle` directly.

### 6.2 Render loop

```rust
let mut term_events = crossterm::event::EventStream::new();   // feature = "event-stream"
let mut snapshot    = store.subscribe();
let mut ui          = UiState::default();
let mut dirty       = true;

loop {
    if dirty {
        let mut hits = HitMap::new();
        terminal.draw(|f| views::shell::render(f, &snapshot.borrow(), &mut ui, &mut hits))?;
        ui.hits = hits;             // the next event is resolved against these rectangles
        dirty = false;
    }

    tokio::select! {
        Some(Ok(ev)) = term_events.next() => {
            for intent in input::translate(ev, &ui) { apply(intent); }
            dirty = true;
        }
        Ok(()) = snapshot.changed() => dirty = true,
        Some(()) = ui.animation_tick() => dirty = true,   // only while a spinner is running
    }
}
```

**Do not run at a fixed frame rate.** Redraw only when an event arrives or state changes.
Mouse-move events fire for every cell, so `dirty` is set **only when the hovered target
actually changes**.

### 6.3 Two kinds of state

| Kind | Owner | Examples |
| --- | --- | --- |
| `Snapshot` (data) | store task | connection status, tree, result sets, running queries |
| `UiState` (appearance) | TUI loop | scroll offset, selected cell, column widths, split position, hover, focus |

Transient UI state does not go into the store. Mixing the two makes the scroll position jump
on every asynchronous update.

### 6.4 Logging and terminal restore

- Writing to stdout while the TUI is up corrupts the screen, so `tracing` plus
  `tracing-appender` write to `~/.local/state/sqlake/sqlake.log` and nowhere else.
  Controlled by `RUST_LOG`.
- Terminal restore (`LeaveAlternateScreen`, `DisableMouseCapture`, `disable_raw_mode`) is
  **concentrated in `TerminalGuard::drop`**. The panic hook and the `$EDITOR` launch both go
  through that single path. **Write this first** — without it, development corrupts the
  terminal constantly.

---

## 7. Mouse foundation (`sqlake-tui::hit`)

ratatui has no hit testing, but the rectangles are in hand at draw time, so the layer is thin.

### 7.1 HitMap

Render functions take `&mut HitMap` and record which rectangle belongs to whom.

```rust
pub struct HitMap { entries: Vec<(Rect, u8 /* z */, Target)> }

pub enum Target {
    Pane(PaneId),
    TreeRow    { conn: ConnId, idx: usize },
    TreeToggle { conn: ConnId, idx: usize },
    GridCell   { tab: TabId, row: usize, col: usize },
    GridHeader { tab: TabId, col: usize },       // click to sort
    GridColEdge{ tab: TabId, col: usize },       // drag to resize
    Scrollbar  { pane: PaneId, part: ScrollPart },
    Splitter(SplitId),
    Tab(TabId), TabClose(TabId),
    Button(ButtonId), MenuItem(MenuItemId),
}

impl HitMap {
    pub fn push(&mut self, rect: Rect, z: u8, t: Target) { .. }
    pub fn at(&self, pos: Position) -> Option<&Target> {
        self.entries.iter().rev()
            .filter(|(r, _, _)| r.contains(pos))
            .max_by_key(|(_, z, _)| *z).map(|(_, _, t)| t)
    }
}
```

Carrying `z` is what lets **modals and popovers reliably swallow clicks meant for what is
behind them**. Deciding this up front avoids a guaranteed rewrite later.

### 7.2 Gesture synthesis

```rust
pub struct MouseState {
    pressed:    Option<(Target, Position)>,   // drag origin; tracked even outside the rect
    last_click: Option<(Target, Instant)>,    // double click: 300ms, same target
    hover:      Option<Target>,
}
```

`MouseEvent` is folded into
`Gesture { Click, DoubleClick, RightClick, DragBy(dx,dy), Scroll(±), HoverEnter/Leave }`, and
`(Target, Gesture)` is translated into intents.

### 7.3 One input pipeline — the most important decision here

```rust
// input.rs
fn on_mouse(target: &Target, g: Gesture, ui: &UiState) -> Vec<Intent>
fn on_key  (key: KeyEvent,   ui: &UiState) -> Vec<Intent>
```

**Mouse and keyboard produce the same intents.** Nothing becomes mouse-only by construction,
and a new feature cannot ship without a key binding. Both are pure functions, so tests can
enforce it.

### 7.4 Terminal caveats

- Mouse capture takes native text selection away from the terminal. The status bar
  permanently shows "Shift (or Option) + drag to select", **clipboard copy is implemented via
  OSC 52** (cell, row, or whole result — works over SSH), and `--no-mouse` disables capture
  entirely.
- Some terminals and tmux configurations cannot deliver right-click, so every context menu
  entry also has a key binding.

### 7.5 Operation matrix

| Part | Mouse | Keyboard |
| --- | --- | --- |
| Pane | click to focus | `Ctrl-h/j/k/l` |
| Splitter | drag to resize, double-click to even out | `Ctrl-←/→` |
| Scrolling | wheel, thumb drag, track click | `j/k`, `PgUp/PgDn`, `g/G` |
| Grid | header click sorts, edge drag resizes, cell click selects, shift-click extends, right-click opens menu | arrows, `v`, `y` |
| Tree | click `▸` to expand, double-click to preview | `Enter`, `←/→` |
| Tabs | click to switch, `×` or middle-click to close | `Ctrl-Tab`, `Ctrl-w` |
| Editor area | click to open `$EDITOR` | `e` |
| Modal | click outside to dismiss | `Esc` |
| Command palette | — | `Ctrl-p` |

---

## 8. Screen layout

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

```
crates/sqlake-tui/src/
├── app.rs           # event loop, terminal setup, TerminalGuard
├── hit.rs           # HitMap, Target, MouseState, Gesture
├── input.rs         # (Target, Gesture) -> Intent, KeyEvent -> Intent
├── editor.rs        # launching and returning from $EDITOR
├── ui_state.rs      # scroll, selection, column widths, splits, focus
├── theme.rs
├── widgets/         # pane, grid, tree, scrollbar, modal, menu, button, splitter, toast
└── views/
    ├── shell.rs     # overall layout, tab bar, status bar
    ├── explorer.rs  # features 1 and 2
    ├── grid.rs      # feature 3
    ├── sql.rs       # feature 4 (buffer display, execution, results)
    ├── detail.rs    # feature 5
    └── history.rs   # features 7 and 8
```

The data grid **must be virtualised** — only visible rows are drawn. `TableState::offset` is
managed by hand so that 100k rows cost the same as 100.

---

## 9. No SQL editor: delegate to `$EDITOR` (feature 4)

Write the buffer to a temporary file, launch `$EDITOR` (neovim or whatever else), and read it
back when the editor exits. The same mechanism as `git commit`.

### 9.1 What this buys

- Syntax highlighting, completion, snippets, LSP (`sqls`, `sqlfluff`), key bindings, macros —
  **the user's existing neovim configuration applies unchanged.**
- No dependency on any editor crate, and no multi-line editing, undo/redo or search to write.
- The temporary file carries a `.sql` extension, so filetype detection works automatically.

### 9.2 Flow

1. `e`, or a click on the editor area → `Action::EditExternally { tab }`.
2. **The main loop handles this synchronously** — it hands the terminal over, so ordering
   matters and it must not go through the store.
   - Release `TerminalGuard` (`disable_raw_mode`, `LeaveAlternateScreen`, `DisableMouseCapture`)
   - Write the buffer to `~/.local/state/sqlake/scratch/{tab_id}.sql`
   - `Command::new(editor).args(&args).arg(path).status()` and wait
   - Re-acquire `TerminalGuard` and `terminal.clear()` for a full redraw
3. Read the file back into a `RawSql`. If it changed, record it as a draft in history.
4. `Ctrl-Enter` or the run button hands off to the `RunQuery` use case: estimate, approve, run.

### 9.3 Caveats

- **Terminal restore is concentrated in `TerminalGuard::drop`.** Running the same code as the
  panic path means an editor that dies abnormally still leaves a usable screen.
- Editor resolution order: the `editor` setting, `$VISUAL`, `$EDITOR`, then `vi`. `editor_args`
  (e.g. `["--wait"]`) exists for GUI editors.
- The store task keeps running while the editor is open, so streams from running queries are
  still consumed. The display catches up on return.
- Scratch files are persisted per tab and reloaded on session restore.

### 9.4 What the SQL tab does own

Everything except editing: a read-only preview of the buffer, run and cancel, the estimated
byte count, error line highlighting, the result grid, and tab management.

### 9.5 Possible later (out of scope for v1)

A watch mode: use `notify` to watch the scratch file and re-run automatically on every save,
so neovim can stay open in another tmux pane. Genuinely useful for a personal tool, but it
adds a dependency, so not in v1.

---

## 10. Configuration and secrets (`sqlake-config`)

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
auth = { adc = true }           # or { service_account = "~/.config/.../sa.json" }
max_bytes_billed = "20GB"       # queries above this are refused
```

**Secrets are never stored in plaintext.** Resolution order is `keyring` (macOS Keychain),
then a `command`, then an environment variable. The `Profile` → `ResolvedProfile` conversion
is that boundary, and `ResolvedProfile` is wiped from memory via `zeroize`. Connection-string
masking lives in exactly one function and never reaches the log.

---

## 11. Drivers

### 11.1 PostgreSQL

- `tokio-postgres` with `tokio-postgres-rustls`; `sslmode` maps onto the rustls configuration.
- Hold **two connections: one for queries, one for metadata**, so that expanding the tree is
  never blocked behind a long-running query.
- Cancellation uses `client.cancel_token()`. On connect, set `application_name = 'sqlake'`,
  a `statement_timeout`, and — when the profile is read-only —
  `SET default_transaction_read_only = on`.
- Read metadata from `pg_catalog` directly (`pg_class`, `pg_attribute`, `pg_index`,
  `pg_constraint`, `pg_trigger`, `pg_description`) rather than `information_schema`: faster
  and more informative.
- **Type decoding.** Receive through a `RawValue: FromSql` whose `accepts()` always returns
  true, holding `(Type, Vec<u8>)`. Decode the ~40 known OIDs strictly and route everything
  else to `Value::Opaque`. Extension and user-defined types then cannot crash the client.
- Preview issues `SELECT * FROM "sch"."tbl" ORDER BY … LIMIT n OFFSET m`. Identifiers can only
  be assembled through `QuotedIdent` (§5.1).
- Show the `EXPLAIN` row estimate first; run an exact `COUNT(*)` only on explicit request.

### 11.2 BigQuery

- `gcp-bigquery-client` with `yup-oauth2` (ADC, service account, or impersonation). Where the
  crate lags the API, wrap REST calls thinly inside the driver so `reqwest` can be used
  directly.
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

## 12. Proxies and tunnels (feature 6)

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

## 13. History and templates (features 7 and 8)

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
- Templates carry `{{param}}` placeholders. As types this is
  `Template` → `BoundTemplate` → `RawSql`, the same shape as §5.1. Placeholders like
  `{{table}}` are completed from the currently selected tree node.
- **Failed queries are recorded too** — in a personal tool, the failures are the useful part.

---

## 14. Testing

**In-source tests by default**, in a `#[cfg(test)] mod tests` at the bottom of each file.
Private pure functions can then be tested directly, with no need to widen visibility to
`pub(crate)` for the sake of a test. This design is heavy on pure functions and type
conversions, so the two fit well.

| Subject | Location | Content |
| --- | --- | --- |
| Stage conversions | in-source | `RawSql → ValidatedSql → PreparedSql`, `Ident → QuotedIdent` escaping, `Template → BoundTemplate` |
| Use cases | in-source | Inject the mock driver and check input to output, including the `NeedsApproval` branch |
| Input layer | in-source | `(Target, Gesture) -> Intent` and `KeyEvent -> Intent`. An exhaustive test enforces that **every intent has a keyboard path** |
| Value formatting | in-source | `Value -> Cell`: unknown types, NULL, huge numbers, full-width characters, embedded newlines |
| HitMap | in-source | z-order resolution and off-by-one errors at rectangle edges |
| Screen snapshots | in-source + `insta` | Render to `TestBackend` at fixed sizes and compare the `Buffer` |
| Driver conformance | `tests/` | One shared suite run against pg, bq and mock. `testcontainers` for pg; an emulator or `#[ignore]` for bq |

`tests/` is reserved for behaviour observable from outside the crate. Because
`sqlake-driver-mock` exists, use case and screen tests run in CI with no database.

---

## 15. Milestones

| # | Content | Done when |
| --- | --- | --- |
| **M0 — Foundation** | workspace, domain types, staged types, `UseCase` trait, store, render loop, `TerminalGuard`, **`HitMap` and one input pipeline**, base widgets, mock driver | The tree and the grid can be driven entirely by mouse against the mock connection, and every one of those operations also works from the keyboard. See [design-m0.md](design-m0.md) |
| **M1** | Connection management (feature 1) | `Profile → ResolvedProfile`, keyring, connection test, connection tabs |
| **M2** | Table list (feature 2) | Lazy tree expansion for pg and bq, filter search |
| **M3** | Table preview (feature 3) | Paging, sorting, cell detail, CSV/JSON copy via OSC 52 |
| **M4** | Running SQL (feature 4) | `$EDITOR` launch and terminal restore, estimate → approve → run, cancellation, multiple tabs, error line display |
| **M5** | Table definitions (feature 5) | Columns, indexes, triggers, constraints, partitioning, DDL |
| **M6** | Proxy settings (feature 6) | `command` tunnels, HTTP proxy |
| **M7** | SQL templates (feature 7) | Save, parameter entry, insert from the palette |
| **M8** | Query history (feature 8) | FTS search, re-run, promote to template |

**M0 determines the feel of everything else.** `HitMap`, the data grid and the staged types in
§5 are all expensive to replace later, so they are the parts worth finishing properly first.

---

## 16. Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Terminal state corrupted after returning from the external editor | unusable | Concentrate restore in `TerminalGuard::drop` and run the same code as the panic path |
| `$EDITOR` is a GUI editor and returns immediately | annoyance | `editor_args` can supply a `--wait` equivalent; warn when the editor exits instantly |
| Mouse capture steals native text selection | annoyance | OSC 52 copy as standard, a permanent hint about shift-drag, and `--no-mouse` |
| Terminals and tmux configurations without right-click or hover | missing features | Every feature has a key binding, enforced by the exhaustive test in `input.rs` |
| Event-driven rendering misses an update | looks frozen | `dirty` is only set in the three `select!` arms; nothing else draws |
| Data grid performance (100k rows × 100 columns) | usability | Draw only visible rows; fix column widths by sampling the first N rows; swap in `rat-ftable` if that is not enough |
| Too many staged types make the code verbose | velocity | Keep to the pipelines listed in §5.1; add a stage only when skipping it would cause a real accident |
| BigQuery billing accident | real cost | `tabledata.list` for preview; `ApprovedQuery` enforced by the type system; `maximumBytesBilled` |
| Unknown PostgreSQL type crashes the client | unusable | `Value::Opaque` fallback enforced by the type system |
| Accidental operation against production | real damage | `color` and `readonly` on the profile; read-only connections set `default_transaction_read_only = on` |

---

## 17. Dependencies (provisional)

```toml
# shared
tokio = { version = "1", features = ["full"] }
async-trait = "0.1"
serde = { version = "1", features = ["derive"] }
toml = "0.9"
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
tracing-appender = "0.2"
directories = "6"
time = "0.3"
uuid = { version = "1", features = ["v4", "serde"] }

# UI — these two and nothing else
ratatui = "0.30"
crossterm = { version = "0.29", features = ["event-stream"] }

# configuration and secrets
keyring = "3"
zeroize = "1"

# persistence
rusqlite = { version = "0.37", features = ["bundled"] }

# PostgreSQL
tokio-postgres = { version = "0.7", features = ["with-serde_json-1", "with-time-0_3"] }
tokio-postgres-rustls = "0.13"
rustls = "0.23"

# BigQuery
gcp-bigquery-client = "0.28"
yup-oauth2 = "12"
reqwest = { version = "0.12", features = ["json", "rustls-tls", "socks"] }

# development
testcontainers = "0.25"
insta = "1"
```

For `ValidatedSql`, start by checking whether splitting on semicolons and classifying the
statement is enough; reach for `sqlparser` only if it is not. Versions are pinned with
`cargo add` at implementation time.

---

## 18. References

- [ratatui](https://ratatui.rs/) and [awesome-ratatui](https://github.com/ratatui/awesome-ratatui)
- [rainfrog](https://github.com/achristmascarl/rainfrog) — a PostgreSQL TUI in Rust and
  ratatui with a mouse mode. **The closest reference implementation**
- [rat-salsa / rat-widget](https://lib.rs/crates/rat-widget) — fallback for large tables
- [iocraft](https://github.com/ccbrown/iocraft) — not adopted; reasoning in §2
