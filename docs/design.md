# sqlake — Design

A mouse-friendly database client that runs in the terminal.

Scope is deliberately narrow: PostgreSQL and BigQuery, built as a personal tool. Where a
trade-off exists, prefer "hard to break" and "possible to finish" over "extensible".

This document holds the architecture. Companion documents hold only what is specific to their
subject: [M0 — Foundation](design-m0.md), [Agent surface](design-agent.md).

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
`sqlake-driver-mock` is what lets both the UI and the use cases be developed and tested
without standing up a database.

---

## 3. Domain model (`sqlake-core`)

### 3.1 Driver abstraction

```rust
#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DriverKind;
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

    /// Execution. Accepts approved queries only (§4).
    async fn execute(&self, q: &ApprovedQuery) -> Result<QueryStream>;

    async fn close(self: Box<Self>);
}
```

A method is added to `Session` by the milestone that first needs it. Declaring the whole
surface up front would force stub types into existence months before anything constructs one.

### 3.2 Capabilities

The UI consults this to decide what to show. `if driver == Postgres` never appears in the UI.

```rust
pub struct Capabilities {
    /// The levels below the root, outermost first.
    pub hierarchy: &'static [HierarchyLevel],
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

pub struct HierarchyLevel {
    pub kind: NodeKind,          // Root | Catalog | Namespace | Relation
    pub label: &'static str,     // "schema" or "dataset", "database" or "project"
}
```

`NodeKind` is structural and `label` is what the user reads. Keeping them apart is what lets
BigQuery call a namespace a "dataset" and PostgreSQL call it a "schema" without either word
reaching a `match` in the UI. It also means the hierarchy can be a different *depth* per
driver, not just differently named.

### 3.3 Value model

One representation so that database-specific types never leak into the UI. The property that
matters most is **never crashing on an unknown type**.

```rust
pub enum Value {
    Null,
    Bool(bool),
    Int(i64), Float(f64), Decimal(String),   // string, to avoid losing precision
    Text(String), Bytes(Vec<u8>),
    Date(..), Time(..),
    Timestamp(..),                           // no zone: pg timestamp, bq DATETIME
    TimestampTz(..),                         // with zone: pg timestamptz, bq TIMESTAMP
    Json(serde_json::Value),
    Array(Vec<Value>),
    Struct(Vec<(String, Value)>),            // bq RECORD, pg composite
    Opaque { type_name: String, text: String },  // escape hatch for unknown types
}
```

Timestamps stay split because both engines have both concepts, and collapsing them loses
whether a value was anchored to an instant or to a wall clock.

### 3.4 Table definition (feature 5)

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

## 4. Use cases and types (`sqlake-app`)

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
- Outputs echo the request back with the result. Without that, a late reply is applied to
  whatever is selected now, and a stale page overwrites a newer one.

### 4.1 Stages as types

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

Conversions go through `TryFrom` or a dedicated function carrying a failure reason, and
**constructors are not `pub`**: `ApprovedQuery::new` is callable only from the approval logic
in the same module.

### 4.2 `RenderedGrid` formats lazily

The obvious implementation materialises `Vec<Vec<Cell>>`. At 200k rows by 60 columns that
allocates twelve million strings in order to display thirty of them. So `RenderedGrid` owns
the `ResultSet` and formats cells on access; only column widths are computed eagerly, from a
sample of the first rows.

Widths are sampled rather than measured over everything for a second reason: a column that
resized itself as pages arrived would be unusable.

Three details that only surface against real data:

- Width is measured in **display columns, not characters**. Seven CJK characters occupy
  fourteen terminal columns.
- Newlines and tabs are replaced before they reach the terminal, and cell text is clamped, so
  a megabyte-long value is not re-measured on every frame.
- `NULL` renders as a glyph. A blank cell is indistinguishable from an empty string.

### 4.3 "Needs approval" is an output, not an error

```rust
pub enum RunQueryOutput {
    Started       { handle: QueryHandle },
    NeedsApproval { estimate: Estimate, prepared: PreparedSql },
}
```

"Over the threshold, so confirmation is needed" is not a failure — it is **a normal branch**.
The UI shows a confirmation dialog on `NeedsApproval` and calls the same use case again with
`Approval::Approved`.

### 4.4 The relationship to `Action`

`Action` is a raw intent from the UI ("this button was pressed"); a use case `Input` is
validated. **The UI never calls a use case directly.** The store builds an `Input` from an
`Action`, invokes the use case, and folds the `Output` into the `Snapshot`.

`Action` carries only what touches data or performs I/O. An action carries no parameter the
store already owns: sorting names a column but not a direction, so two fast header clicks
cannot race with a sort already in flight.

---

## 5. Concurrency and state

### 5.1 Topology

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
- **The store never awaits a use case inline.** Every call is spawned and its result returns
  as an internal event, so one slow expansion cannot block every other action.
- One actor task per connection owns the `Box<dyn Session>` and serialises access, so drivers
  need not be internally concurrent. Holding more than one physical connection is a
  driver-level concern (§11.1), not something the application layer arranges.

### 5.2 Render loop

```rust
let mut term_events = crossterm::event::EventStream::new();   // feature = "event-stream"
let mut snapshot    = store.subscribe();
let mut ui          = UiState::default();
let mut dirty       = true;

loop {
    if dirty {
        hits.clear();
        terminal.draw(|f| views::shell::render(f, &snapshot.borrow(), &mut ui, &mut hits))?;
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
Mouse-move events fire for every cell, so `dirty` is set only when the hovered target
actually changes.

### 5.3 Two kinds of state

| Kind | Owner | Examples |
| --- | --- | --- |
| `Snapshot` (data) | store task | connection status, tree, result sets, running queries |
| `UiState` (appearance) | TUI loop | scroll offset, selected cell, column widths, split position, hover, focus |

Transient UI state does not go into the store. Mixing the two makes the scroll position jump
on every asynchronous update.

The same split decides what an input event becomes: `Intent::View` is applied synchronously
inside the render loop, `Intent::App` is dispatched to the store. Routing a wheel tick through
an async task would add a round trip to every notch.

Tree expansion is data, not appearance: it drives lazy loading. So the store owns it and
publishes the tree already flattened, and drawing the tree is a slice and an index rather than
a recursive walk.

### 5.4 Terminal state, logging and panics

- Writing to stdout while the TUI is up corrupts the screen, so `tracing` plus
  `tracing-appender` write to `~/.local/state/sqlake/sqlake.log` and nowhere else.
- **Terminal mode changes happen in exactly one place.** Raw mode, the alternate screen and
  mouse capture all have to be undone on every exit path — a clean quit, a panic, and handing
  the terminal to `$EDITOR` (§8) — so all three go through one `restore` function, called from
  `TerminalGuard::drop`.
- The panic hook calls `restore` before delegating to the previous hook. The hook runs before
  unwinding, so the screen is usable by the time the backtrace prints. Without this, a panic
  leaves the message invisible on the alternate screen.

---

## 6. Mouse foundation

ratatui has no hit testing, but it hands every rectangle to the code that draws it. So each
widget records "this rectangle is this thing" while drawing, and a click is resolved against
that record.

### 6.1 HitMap

```rust
pub struct HitMap { entries: Vec<(Rect, u8 /* z */, Target)> }

pub enum Target {
    Pane(PaneId),
    TreeRow { index: usize }, TreeToggle { index: usize },
    GridCell { row: usize, col: usize },
    GridHeader { col: usize },      // click to sort
    GridColEdge { col: usize },     // drag to resize
    Scrollbar { pane: PaneId, part: ScrollPart },
    Splitter(SplitId),
    Tab(TabId), TabClose(TabId),
    Button(ButtonId), Toast(ToastId),
    Backdrop,
}
```

The highest `z` wins; between equal `z` the most recently pushed wins, which is whatever was
drawn last and so is on top. Z levels are named constants, not magic numbers: content, chrome,
backdrop, modal, menu.

**A modal pushes a full-screen backdrop rectangle below itself.** Without it a click outside
the modal falls through, and a confirmation dialog becomes a way to trigger the thing it was
confirming. The backdrop resolves to `Target::Backdrop`, which dismisses the modal.

Two hit targets need care:

- The tree toggle glyph overlaps its row and must win. Same z, pushed later.
- A column boundary is drawn one cell wide, but asking the user to land on a single cell is
  asking too much. The hit area is three cells; the drawn line stays one.

The map is cleared and reused each frame rather than reallocated.

### 6.2 Gesture synthesis

The terminal reports presses, releases and motion. The UI wants clicks, double clicks and
drags. Translating once means no widget reasons about button state.

- A drag keeps the target captured at button-down, **even after the pointer leaves the
  rectangle**. Without this, resizing a column stops the moment the pointer outruns the cursor.
- Releasing after a drag is not a click. Emitting both would fire the row's action every time
  the user finished resizing something on it.
- A double click is judged from where each click *started*, and consumes the pair, so a third
  click begins a new one.
- Hover reports only when the target changes. Motion within one row is the common case, and
  reporting it would redraw on every cell of movement.

### 6.3 One input pipeline

```rust
fn on_mouse(target: Target, gesture: Gesture, ctx: &InputContext) -> Vec<Intent>
fn on_key  (event: KeyEvent, ctx: &InputContext) -> Vec<Intent>
```

**Mouse and keyboard produce the same intents.** `IntentKind` names a *capability* rather than
a variant, so clicking a tree row and arrowing onto it are the same kind. That is the level at
which "reachable by keyboard" is a claim about what the user can do.

The mechanism is a chain, and each link is enforced by the compiler or a test:

1. The kind enum and its complete list are generated from one macro invocation, so they cannot
   drift apart.
2. `IntentKind::of` is an exhaustive match, so adding an `Intent` variant stops the crate
   compiling until the new intent has a kind.
3. A test sweeps every `Target` × `Gesture` pair and asserts that every kind it can reach has
   a key binding. Exhaustive matches on both enums force the sample lists to grow with them.

The key map is **data, not code** — a table of `(keys, context, kind)`. Being enumerable is
what makes the test possible, and it gives a help modal and user-defined bindings for free. A
pane binding beats the global one for the same key, which is how `Esc` can dismiss a modal in
one context and a toast in another without being ambiguous.

The reverse direction is deliberately not required: keyboard-only capabilities are fine.

### 6.4 Terminal caveats

- Mouse capture takes native text selection away from the terminal. The status bar
  permanently shows "Shift (or Option) + drag to select", **clipboard copy is implemented via
  OSC 52** (cell, row, or whole result — works over SSH), and `--no-mouse` disables capture
  entirely.
- Some terminals and tmux configurations cannot deliver right-click, so every context menu
  entry also has a key binding.

### 6.5 Operation matrix

| Part | Mouse | Keyboard |
| --- | --- | --- |
| Pane | click to focus | `Tab`, `Ctrl-h` |
| Splitter | drag to resize, double-click to even out | `Ctrl-←/→`, `=` |
| Scrolling | wheel, thumb drag, track click | `j/k`, `PgUp/PgDn`, `g/G` |
| Grid | header click sorts, edge drag resizes, cell click selects, right-click opens menu | `J/K`, `H/L`, `s`, `<`/`>` |
| Tree | click `▸` to expand, double-click to open | `Space`, `←`/`→`, `Enter` |
| Tabs | click to switch, `×` or middle-click to close | `]`/`[`, `Ctrl-w` |
| Editor area | click to open `$EDITOR` | `e` |
| Modal | click outside to dismiss | `Esc` |
| Command palette | — | `Ctrl-p` |

Case carries the axis of control: lower case and the arrow keys move the **view**, upper case
moves the **selection**. `H`/`L` are to `J`/`K` what `Left`/`Right` are to `j`/`k`. Dropping
half of that pairing is easy to miss, because the coverage test works at the level of the
capability and both directions of a cursor are one capability.

---

## 7. Screen layout

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
├── hit.rs           # HitMap, Target, z levels
├── mouse.rs         # MouseState, gesture synthesis
├── intent.rs        # Intent, ViewCmd, IntentKind
├── input.rs         # KEYMAP, on_key, on_mouse
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

The data grid **must be virtualised** — only visible rows are drawn — so that 100k rows cost
the same as 100.

---

## 8. No SQL editor: delegate to `$EDITOR` (feature 4)

Write the buffer to a temporary file, launch `$EDITOR`, and read it back when the editor
exits. The same mechanism as `git commit`.

### 8.1 What this buys

- Syntax highlighting, completion, snippets, LSP (`sqls`, `sqlfluff`), key bindings, macros —
  **the user's existing neovim configuration applies unchanged.**
- No dependency on any editor crate, and no multi-line editing, undo/redo or search to write.
- The temporary file carries a `.sql` extension, so filetype detection works automatically.

### 8.2 Flow

1. `e`, or a click on the editor area → `Action::EditExternally { tab }`.
2. **The main loop handles this synchronously** — it hands the terminal over, so ordering
   matters and it must not go through the store.
   - Release `TerminalGuard`
   - Write the buffer to `~/.local/state/sqlake/scratch/{tab_id}.sql`
   - `Command::new(editor).args(&args).arg(path).status()` and wait
   - Re-acquire `TerminalGuard` and `terminal.clear()` for a full redraw
3. Read the file back into a `RawSql`. If it changed, record it as a draft in history.
4. `Ctrl-Enter` or the run button hands off to the `RunQuery` use case: estimate, approve, run.

### 8.3 Caveats

- Restore runs through the same path as the panic hook (§5.4), so an editor that dies
  abnormally still leaves a usable screen.
- Editor resolution order: the `editor` setting, `$VISUAL`, `$EDITOR`, then `vi`. `editor_args`
  (e.g. `["--wait"]`) exists for GUI editors.
- The store task keeps running while the editor is open, so streams from running queries are
  still consumed. The display catches up on return.
- Scratch files are persisted per tab and reloaded on session restore.

### 8.4 What the SQL tab does own

Everything except editing: a read-only preview of the buffer, run and cancel, the estimated
byte count, error line highlighting, the result grid, and tab management.

### 8.5 Possible later (out of scope for v1)

A watch mode: use `notify` to watch the scratch file and re-run automatically on every save,
so neovim can stay open in another tmux pane. Genuinely useful for a personal tool, but it
adds a dependency.

---

## 9. A second front-end: the agent surface

The application layer has no idea a terminal exists, so the TUI is one front-end over it
rather than the only possible one. An AI agent driving sqlake — listing tables, previewing
data, running a query — is **a second front-end over the same `sqlake-app`, not new logic**.

Two things follow, and both are load-bearing:

- Nothing in the agent surface may construct a query, know a driver, or format a result. If
  something there needs a new use case, the interactive client is missing a feature too.
- The guards already in the type system apply unchanged. `Session::execute` accepts only an
  `ApprovedQuery`, so an agent cannot run an unestimated BigQuery scan any more than a human
  can; what differs is that approval is granted by a byte budget rather than by a dialog, and
  that going over it returns `NeedsApproval` (§4.3) for a human to answer.

The surface is a socket API against a running session, thin noun-verb subcommands over it, and
an MCP server wrapping the same client — modelled on herdr. Attaching to a running session is
the point: connections behind a bastion, an MFA prompt or a `gcloud auth` flow are expensive
to establish, and for anything interactive, impossible to re-establish per command.

Detail, including the safety policy for agents, is in [design-agent.md](design-agent.md).

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

### 10.1 PostgreSQL

- `tokio-postgres` with `tokio-postgres-rustls`; `sslmode` maps onto the rustls configuration.
- Hold **two connections: one for queries, one for metadata**, so that expanding the tree is
  never blocked behind a long-running query. Both sit behind one `Session`.
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
  be assembled through `QuotedIdent` (§4.1).
- Show the `EXPLAIN` row estimate first; run an exact `COUNT(*)` only on explicit request.

### 10.2 BigQuery

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
- Templates carry `{{param}}` placeholders, staged like §4.1. Placeholders such as `{{table}}`
  are completed from the currently selected tree node.
- **Failed queries are recorded too** — in a personal tool, the failures are the useful part.

---

## 14. Testing

**In-source tests by default**, in a `#[cfg(test)] mod tests` at the bottom of each file.
Private pure functions can then be tested directly, with no need to widen visibility for the
sake of a test. This design is heavy on pure functions and type conversions, so the two fit
well.

`tests/` is reserved for behaviour observable from outside the crate — chiefly the shared
driver conformance suite, run against PostgreSQL via `testcontainers`, against BigQuery via an
emulator, and against the mock.

Screen snapshots use `insta` with ratatui's `TestBackend`. They are brittle by nature, so they
cover a small number of representative screens rather than every state.

Because `sqlake-driver-mock` exists, every other test runs with no database and no network.

---

## 15. Milestones

| # | Content | Done when |
| --- | --- | --- |
| **M0 — Foundation** | workspace, domain types, staged types, `UseCase` trait, store, render loop, `TerminalGuard`, `HitMap` and one input pipeline, base widgets, mock driver | The tree and the grid can be driven entirely by mouse against the mock connection, and every one of those operations also works from the keyboard. See [design-m0.md](design-m0.md) |
| **M1** | Connection management (feature 1) | `Profile → ResolvedProfile`, keyring, connection test, connection tabs |
| **M2** | Table list (feature 2) | Lazy tree expansion for pg and bq, filter search |
| **M3** | Table preview (feature 3) | Paging, sorting, cell detail, CSV/JSON copy via OSC 52 |
| **M4** | Running SQL (feature 4) | `$EDITOR` launch and terminal restore, estimate → approve → run, cancellation, multiple tabs, error line display |
| **M5** | Table definitions (feature 5) | Columns, indexes, triggers, constraints, partitioning, DDL |
| **M6** | Proxy settings (feature 6) | `command` tunnels, HTTP proxy |
| **M7** | SQL templates (feature 7) | Save, parameter entry, insert from the palette |
| **M8** | Query history (feature 8) | FTS search, re-run, promote to template |
| **M9** | Agent surface: CLI and socket API | One-shot subcommands against both drivers with JSON output, attaching to a running session, generated schema, read-only by default. See [design-agent.md](design-agent.md) |
| **M10** | Agent surface: MCP | `sqlake mcp` exposes the same operations as MCP tools |

**M0 determines the feel of everything else.** `HitMap`, the data grid and the staged types in
§4 are all expensive to replace later, so they are the parts worth finishing properly first.

---

## 16. Risks

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Terminal state corrupted after returning from the external editor | unusable | One restore path, shared with the panic hook (§5.4) |
| `$EDITOR` is a GUI editor and returns immediately | annoyance | `editor_args` can supply a `--wait` equivalent; warn when the editor exits instantly |
| Mouse capture steals native text selection | annoyance | OSC 52 copy as standard, a permanent hint about shift-drag, and `--no-mouse` |
| Terminals and tmux configurations without right-click or hover | missing features | Every feature has a key binding, enforced by the coverage test (§6.3) |
| Event-driven rendering misses an update | looks frozen | `dirty` is only set in the three `select!` arms; nothing else draws |
| Data grid performance (100k rows × 100 columns) | usability | Draw only visible rows; sample column widths; swap in `rat-ftable` if that is not enough |
| Too many staged types make the code verbose | velocity | Keep to the pipelines listed in §4.1; add a stage only when skipping it would cause a real accident |
| BigQuery billing accident | real cost | `tabledata.list` for preview; `ApprovedQuery` enforced by the type system; `maximumBytesBilled` |
| Unknown PostgreSQL type crashes the client | unusable | `Value::Opaque` fallback enforced by the type system |
| Accidental operation against production | real damage | `color` and `readonly` on the profile; read-only connections set `default_transaction_read_only = on` |

---

## 17. Dependencies

The UI layer depends on **`ratatui` and `crossterm`, and nothing else**. The data grid, tree,
modals, scrollbar handling and focus are written here, because each has requirements specific
to this project and each is a few hundred lines at most. `rat-ftable` is the fallback if the
grid cannot be made fast enough.

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

# UI
ratatui = "0.30"
crossterm = { version = "0.29", features = ["event-stream"] }
unicode-width = "0.2"

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

`crossterm` is declared directly, pinned to the version ratatui re-exports, only to turn on
the `event-stream` feature.

For `ValidatedSql`, start by checking whether splitting on semicolons and classifying the
statement is enough; reach for `sqlparser` only if it is not.

---

## 18. References

- [ratatui](https://ratatui.rs/) and [awesome-ratatui](https://github.com/ratatui/awesome-ratatui)
- [rainfrog](https://github.com/achristmascarl/rainfrog) — a PostgreSQL TUI in Rust and
  ratatui with a mouse mode. The closest reference implementation
- [rat-salsa / rat-widget](https://lib.rs/crates/rat-widget) — fallback for large tables
