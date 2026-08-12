# M0 — Foundation

The scaffolding every later milestone sits on: the workspace, the domain types, the app-layer
plumbing, and — the part that actually determines how the tool feels — the mouse and rendering
foundation in `sqlake-tui`.

M0 ships **no real database driver**. Everything is exercised against `sqlake-driver-mock`.

Parent document: [design.md](design.md).

---

## 1. Definition of done

M0 is complete when all of the following hold:

1. `cargo run` opens a full-screen TUI with a mock connection in the explorer.
2. **The tree and the data grid can be driven entirely with the mouse** — expanding nodes,
   opening a preview, scrolling, sorting, resizing columns, moving the splitter, switching and
   closing tabs.
3. **Every one of those operations also has a key binding**, enforced by a test rather than by
   inspection.
4. Loading and error states are visible: the mock injects latency and failures, and neither
   freezes the UI.
5. Quitting restores the terminal. Panicking restores the terminal.
6. `cargo test` passes with no database and no network.
7. `cargo fmt --check`, `cargo clippy -- -D warnings` and `cargo test` run in CI.

---

## 2. Scope

### In scope

| Area | What is built |
| --- | --- |
| Workspace | 5 crates, pinned toolchain, shared lints, CI |
| `sqlake-core` | ids, `Value`, `ResultSet`, tree types, `Capabilities`, the M0 subset of `Driver`/`Session` |
| `sqlake-driver-mock` | fixtures covering every `Value` variant, injectable latency and failures |
| `sqlake-app` | `Snapshot`, `Action`, store task, session actor, three use cases, `RenderedGrid` |
| `sqlake-tui` | `TerminalGuard`, render loop, `HitMap`, gesture synthesis, input mapping, `UiState`, nine widgets |
| Tests | in-source unit tests, `insta` screen snapshots, the intent-coverage test |

### Out of scope

Deliberately deferred, with the milestone that picks each one up:

| Deferred | Lands in |
| --- | --- |
| PostgreSQL and BigQuery drivers | M1 / M2 |
| Config files, profiles, keyring (`sqlake-config`) | M1 |
| SQLite history and templates (`sqlake-store`) | M7 / M8 |
| `$EDITOR` integration | M4 |
| SQL staged types (`RawSql` … `ApprovedQuery`) | M4 |
| `describe()` and the definition view | M5 |
| Tunnels and proxies | M6 |
| Context menu, cell detail popover, OSC 52 copy | M3 |
| Command palette, help modal, theming | M4 |

**Decision D1 — create five crates, not nine.** `sqlake-config`, `sqlake-store` and the two
real drivers are created in the milestone that first needs them. Empty placeholder crates are
dead weight and hide which parts actually exist.

---

## 3. Workspace and conventions

```
sqlake/
├── Cargo.toml              # workspace, shared deps, shared lints
├── rust-toolchain.toml     # pinned stable
├── .github/workflows/ci.yml
├── docs/
└── crates/
    ├── sqlake/
    ├── sqlake-core/
    ├── sqlake-app/
    ├── sqlake-tui/
    └── sqlake-driver-mock/
```

- **Edition 2024**, toolchain pinned to the current stable in `rust-toolchain.toml`.
- Dependencies are declared once in `[workspace.dependencies]`; member crates use
  `dep = { workspace = true }`.
- Shared lints in `[workspace.lints]`: `unsafe_code = "forbid"`,
  `missing_debug_implementations = "warn"`, plus `clippy::all` and a curated set from
  `clippy::pedantic` (not the whole group — it is too noisy to keep at deny).
- Errors: `thiserror` in library crates, each with its own error enum. `anyhow` only in the
  binary.
- **English everywhere** — documentation, code comments, commit messages.
- Commits follow Conventional Commits (`feat:`, `fix:`, `refactor:`, `docs:`, `test:`,
  `chore:`), scoped by crate where useful (`feat(tui): ...`).

---

## 4. `sqlake-core`

No database dependencies. `serde`, `time`, `uuid`, `async-trait` and `thiserror` only.

### 4.1 Identifiers and references

```rust
pub struct ConnId(Uuid);
pub struct TabId(u32);

/// A position in the object hierarchy. The number of levels is driver-dependent,
/// so this is a path rather than a fixed struct.
pub struct NodeRef {
    pub kind: NodeKind,          // Root | Database | Schema | Relation
    pub path: Vec<String>,       // ["public"], ["public", "users"], ...
}

pub struct TableRef { pub path: Vec<String> }

pub enum RelationKind { Table, View, MatView, Routine, External }
```

### 4.2 The M0 subset of the driver traits

**Decision D2 — `Session` only carries what M0 uses.** `describe`, `estimate` and `execute`
are added by the milestone that needs them (M5 and M4). Declaring them now would force stub
types (`PreparedSql`, `ApprovedQuery`, `TableDetail`) into existence months before anything
constructs one.

```rust
#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DriverKind;
    fn capabilities(&self) -> Capabilities;
    async fn connect(&self) -> Result<Box<dyn Session>, DriverError>;
}

#[async_trait]
pub trait Session: Send + Sync {
    async fn children(&self, of: &NodeRef) -> Result<Vec<TreeNode>, DriverError>;
    async fn preview(&self, t: &TableRef, req: &PageRequest) -> Result<ResultSet, DriverError>;
    async fn close(self: Box<Self>);
}
```

`Driver::connect` takes no profile in M0; `ResolvedProfile` arrives with `sqlake-config` in M1.

```rust
pub struct PageRequest {
    pub offset: u64,
    pub limit: u32,
    pub sort: Option<Sort>,      // column index + direction
}
```

### 4.3 Values and result sets

```rust
pub enum Value { Null, Bool(bool), Int(i64), Float(f64), Decimal(String),
                 Text(String), Bytes(Vec<u8>), Date(..), Time(..), Timestamp(..),
                 Json(serde_json::Value), Array(Vec<Value>),
                 Struct(Vec<(String, Value)>), Opaque { type_name: String, text: String } }

pub struct Column { pub name: String, pub type_name: String, pub nullable: bool }
pub struct Row(pub Vec<Value>);

pub struct ResultSet {
    pub columns: Arc<Vec<Column>>,
    pub rows: Arc<Vec<Row>>,
    pub total_rows: Option<u64>,   // None when unknown or expensive to determine
}
```

`Arc` on both fields is what makes cloning a `Snapshot` cheap (§6.1).

`Interval` is omitted in M0 — no driver produces one yet, and its representation is a
PostgreSQL question best answered in M1.

### 4.4 Staged types present in M0

Only the pipelines M0 actually uses:

| Pipeline | In M0 |
| --- | --- |
| `Ident` → `QuotedIdent` | **Yes.** The mock does not build SQL, but this is the cheapest place to establish the pattern, and the grid needs display quoting |
| `RowBatch` → `ResultSet` → `RenderedGrid` | **Yes.** §6.4 |
| `RawSql` → … → `ApprovedQuery` | No — M4 |
| `Profile` → `ResolvedProfile` | No — M1 |
| `Template` → `BoundTemplate` | No — M7 |

```rust
/// A raw identifier, exactly as it came from the catalogue.
pub struct Ident(String);

/// Quoted and escaped for a specific dialect. SQL assembly accepts only this type.
pub struct QuotedIdent(String);

impl Ident {
    pub fn quote(&self, style: QuoteStyle) -> QuotedIdent { .. }
}
```

`QuotedIdent` has no public constructor, so the only way to obtain one is `Ident::quote`.

---

## 5. `sqlake-driver-mock`

The mock is not a stub — it is the test fixture that every other crate is developed against,
so it is built to exercise the hard cases from day one.

**Decision D6 — the mock injects latency and failures.** Without them, loading states and
error surfaces get written blind and the UI is only discovered to block once a real driver
lands.

```rust
pub struct MockDriver { pub behaviour: Behaviour }

#[derive(Default)]
pub struct Behaviour {
    /// Delay applied to every call.
    pub latency: Duration,
    /// Node paths whose expansion always fails.
    pub failing_nodes: Vec<Vec<String>>,
    /// Node paths that take an extra long time.
    pub slow_nodes: Vec<Vec<String>>,
}
```

### Fixture content

One connection named `mock`, two schemas, and tables chosen to break naive rendering:

| Table | Purpose |
| --- | --- |
| `public.users` | Ordinary case. ~50 rows, mixed types |
| `public.types_showcase` | One column per `Value` variant, including `Opaque`, `Struct`, `Array`, `Bytes` |
| `public.wide` | 60 columns — forces horizontal scrolling and width sampling |
| `public.big` | 200,000 rows, generated lazily — proves virtualisation |
| `public.unicode` | Japanese, emoji, combining marks, CJK full-width, embedded newlines and tabs |
| `public.empty` | Zero rows — the empty-state path |
| `analytics.broken` | Expansion always fails — the error path |
| `analytics.slow` | Two-second expansion — the loading path |

`public.unicode` matters more than it looks: display width is not character count, and getting
column widths wrong there corrupts the entire grid.

---

## 6. `sqlake-app`

### 6.1 Snapshot

Immutable, published on a `watch` channel, cheap to clone.

```rust
pub struct Snapshot {
    pub rev: u64,
    pub connections: Vec<ConnectionView>,
    pub trees: HashMap<ConnId, Arc<TreeView>>,
    pub tabs: Vec<TabView>,
    pub active_tab: Option<TabId>,
    pub busy: Vec<BusyItem>,        // status bar: what is running, and how to cancel it
    pub toasts: Vec<Toast>,
}

pub struct ConnectionView {
    pub id: ConnId,
    pub name: String,
    pub kind: DriverKind,
    pub status: ConnStatus,         // Connecting | Ready | Failed(String) | Closed
    pub capabilities: Capabilities,
}
```

**Decision D7 — flattening the tree happens in the app layer.** Expansion state drives lazy
loading, so it is data and lives in the snapshot. What the UI receives is already flat, so
rendering is a slice and an index:

```rust
pub struct TreeView { pub nodes: Vec<VisibleNode> }

pub struct VisibleNode {
    pub depth: u16,
    pub label: String,
    pub node_ref: NodeRef,
    pub state: NodeState,          // Leaf | Collapsed | Loading | Expanded | Failed(String)
}
```

Scroll offset and selection are *not* here — they belong to `UiState` (§7.6).

### 6.2 Intents: the split between view and app

**Decision D3 — not every input becomes a store action.** Scrolling, moving the selected cell,
resizing a column and moving a splitter are view-local. Routing them through an async task
would add a round trip to every wheel tick and make the UI feel laggy. Everything that touches
data or performs I/O goes to the store.

```rust
pub enum Intent {
    /// Handled inside the render loop. Never leaves the TUI crate.
    View(ViewCmd),
    /// Dispatched to the store. May perform I/O.
    App(Action),
}

pub enum ViewCmd {
    FocusPane(PaneId),
    ScrollBy { pane: PaneId, delta: i32 },
    ScrollTo { pane: PaneId, ratio: f32 },   // from a scrollbar track click
    MoveSelection(Dir),
    SelectCell { row: usize, col: usize },
    ResizeColumn { col: usize, delta: i16 },
    MoveSplit { split: SplitId, delta: i16 },
    EvenSplit(SplitId),
    DismissModal,
    DismissToast(ToastId),
}

pub enum Action {
    Connect(DriverKind),
    Disconnect(ConnId),
    ToggleNode { conn: ConnId, node: NodeRef },
    PreviewTable { conn: ConnId, table: TableRef },
    SortPreview { tab: TabId, col: usize, dir: SortDir },
    LoadMore { tab: TabId },
    CloseTab(TabId),
    SelectTab(TabId),
    Cancel(BusyId),
    Quit,
}
```

Sorting is an `Action` rather than a `ViewCmd` because it re-queries with an `ORDER BY`; the
mock sorts server-side so the real drivers inherit the same path.

### 6.3 Store and session actors

```rust
pub struct Store { tx: mpsc::Sender<Action>, rx: watch::Receiver<Arc<Snapshot>> }
```

- One store task owns `AppState` and is the only writer.
- One actor task per connection owns the `Box<dyn Session>` and serialises access:

```rust
enum SessionCmd {
    Children { of: NodeRef,  reply: oneshot::Sender<Result<Vec<TreeNode>, DriverError>> },
    Preview  { table: TableRef, req: PageRequest,
               reply: oneshot::Sender<Result<ResultSet, DriverError>> },
    Close,
}
```

The store never awaits a session reply inline — it spawns a task that awaits the `oneshot` and
sends the result back as an internal event. Otherwise one slow expansion blocks every other
action, which is exactly what the two-connection rule in the PostgreSQL design exists to
avoid.

```rust
enum Event {
    ChildrenLoaded { conn: ConnId, of: NodeRef, result: Result<Vec<TreeNode>, DriverError> },
    PreviewLoaded  { tab: TabId, result: Result<ResultSet, DriverError> },
    Connected      { conn: ConnId, result: Result<Capabilities, DriverError> },
}
```

So the store task selects over `(Action, Event)` and republishes a snapshot after each.

### 6.4 `RenderedGrid` and lazy formatting

**Decision D4 — format cells lazily; sample widths eagerly.**

The naive version materialises `Vec<Vec<Cell>>`. At 200k rows × 60 columns that is 12M strings
allocated to display 30 of them. But the UI must still never see a `Value` (§5 of the parent
document), so `RenderedGrid` owns the `ResultSet` and formats on access:

```rust
pub struct RenderedGrid {
    result: Arc<ResultSet>,
    columns: Vec<RenderedColumn>,
    pub total_rows: Option<u64>,
}

pub struct RenderedColumn {
    pub name: String,
    pub natural_width: u16,   // sampled; the view may override
    pub align: Align,
}

impl RenderedGrid {
    pub fn columns(&self) -> &[RenderedColumn];
    pub fn row_count(&self) -> usize;
    /// Formats on demand. Only visible cells are ever built.
    pub fn cell(&self, row: usize, col: usize) -> Cell;
}

pub struct Cell { pub text: String, pub align: Align, pub kind: CellKind }
pub enum CellKind { Null, Number, Text, Complex, Error }
```

- `natural_width` comes from sampling the first 200 rows with `unicode-width`, clamped to
  `[3, 60]`. `CellKind` drives styling, so the theme stays in the TUI crate while the
  classification stays with the data.
- Per-column width overrides from dragging live in `UiState` and are applied at draw time.
- No cache in M0. If profiling in M3 shows formatting cost during fast scrolling, add a small
  row-window cache behind the same API — the signature does not change.

### 6.5 Use cases

Three, each with named input and output types:

| Use case | Input | Output |
| --- | --- | --- |
| `Connect` | `{ kind: DriverKind }` | `{ conn: ConnId, capabilities, roots: Vec<TreeNode> }` |
| `ExpandNode` | `{ conn: ConnId, node: NodeRef }` | `{ children: Vec<TreeNode> }` |
| `PreviewTable` | `{ conn: ConnId, table: TableRef, page: PageRequest }` | `{ grid: RenderedGrid }` |

Each is a struct holding its dependencies, implementing the `UseCase` trait from the parent
design. Injecting `MockDriver` makes them testable in isolation.

---

## 7. `sqlake-tui`

### 7.1 `TerminalGuard`

The single place where terminal mode changes.

```rust
pub struct TerminalGuard { mouse: bool }

impl TerminalGuard {
    pub fn enter(mouse: bool) -> io::Result<Self>;   // raw mode, alt screen, capture, hide cursor
}

impl Drop for TerminalGuard {
    fn drop(&mut self) { let _ = restore(self.mouse); }
}

/// Free function so the panic hook can call it without owning the guard.
fn restore(mouse: bool) -> io::Result<()>;
```

Install a panic hook at startup that calls `restore()` and then delegates to the previous hook.
The hook runs before unwinding, so the screen is already sane by the time the backtrace prints.
`Ctrl-C` arrives as an ordinary key event in raw mode, so no signal handling is needed;
`SIGTERM` and `SIGHUP` are out of scope for M0.

A hidden `--panic-test` flag panics immediately after entering the alternate screen. It exists
so the restore path can be verified by hand, since it cannot be asserted in a unit test.

### 7.2 Render loop

As in §6.2 of the parent design, with one addition: `HitMap` is cleared and reused across
frames rather than reallocated, and the loop applies `Intent` rather than `Action`:

```rust
fn apply(&mut self, intent: Intent) {
    match intent {
        Intent::View(cmd) => self.ui.apply(cmd),      // synchronous, local
        Intent::App(action) => self.store.dispatch(action),
    }
}
```

### 7.3 `HitMap` and z-order

Z levels are constants, not magic numbers:

```rust
pub const Z_BASE: u8      = 0;
pub const Z_CONTENT: u8   = 10;   // rows and cells inside a pane
pub const Z_CHROME: u8    = 20;   // scrollbars, splitters, column edges
pub const Z_BACKDROP: u8  = 90;   // modal backdrop
pub const Z_MODAL: u8     = 100;
pub const Z_MENU: u8      = 110;
```

**Decision D5 — a modal pushes a full-screen backdrop rect at `Z_BACKDROP`.** Without it, a
click outside the modal box falls through to whatever is underneath, and a confirmation dialog
becomes a way to accidentally trigger the thing it was confirming. The backdrop resolves to
`Target::Backdrop`, which maps to `ViewCmd::DismissModal`.

Hit targets that need care:

- `TreeToggle` is pushed **before** and inside `TreeRow`, at the same z but later in draw
  order — the `▸` glyph is two cells wide and must win over the row.
- `GridColEdge` is a one-cell-wide rect on each column boundary, pushed at `Z_CHROME` so it
  beats the header and cell beneath it. One cell is hard to hit precisely, so the rect is
  widened to three cells (boundary ±1) while the visual separator stays one cell.

### 7.4 Gesture synthesis

```rust
pub enum Gesture {
    Down, Up, Click, DoubleClick, RightClick,
    DragBy { dx: i16, dy: i16 },
    Scroll(i8),
    HoverEnter, HoverLeave,
}
```

- Double click: within 300 ms, same `Target`, within ±1 cell.
- Drag: the `Target` captured on `Down` is retained until `Up`, even when the pointer leaves
  the rectangle. Without this, resizing a column stops the moment the pointer outruns the
  cursor.
- Hover: `dirty` is set only when the hovered `Target` differs from the previous one. Mouse
  motion otherwise redraws on every cell of movement.

### 7.5 Input mapping, and the test that keeps it honest

The key map is **data, not code** — which makes it enumerable, testable, and configurable later
for free.

```rust
pub struct KeyBinding {
    pub keys: &'static [KeyCombo],
    pub context: Context,          // Global | Explorer | Grid | Modal
    pub intent: IntentKind,
    pub description: &'static str, // feeds the help modal in M4
}

pub const KEYMAP: &[KeyBinding] = &[ .. ];
```

`IntentKind` is a flat enum naming every intent, with an exhaustive mapping from `Intent`:

```rust
impl IntentKind {
    /// Exhaustive match: adding an Intent variant breaks compilation here,
    /// which forces the new intent to be given a kind — and then a key binding.
    pub fn of(intent: &Intent) -> IntentKind { .. }
}
```

The test:

```
for every IntentKind produced by on_mouse over all (Target, Gesture) combinations,
assert that KEYMAP contains at least one binding for that IntentKind
```

This is the mechanical form of the "nothing is mouse-only" principle. The reverse is not
required: keyboard-only intents (the command palette, for instance) are fine.

### 7.6 `UiState`

```rust
pub struct UiState {
    pub focus: PaneId,
    pub split: SplitState,                       // explorer width in columns
    pub explorer: ScrollState,
    pub grids: HashMap<TabId, GridUi>,
    pub modal: Option<Modal>,
    pub mouse: MouseState,
    pub hits: HitMap,
}

pub struct GridUi {
    pub scroll: ScrollState,                     // row offset + horizontal column offset
    pub selection: Option<(usize, usize)>,
    pub width_overrides: HashMap<usize, u16>,    // from column-edge drags
}
```

### 7.7 Widgets built in M0

| Widget | Notes |
| --- | --- |
| `Pane` | Border, title, focus styling. Pushes `Target::Pane` |
| `Splitter` | One-cell divider, drag to move, double-click to even out |
| `Scrollbar` | Track, thumb, and arrows as separate hit targets |
| `Tree` | Virtualised. Separate hit targets for the toggle and the row |
| `DataGrid` | Sticky header, virtualised rows, sortable headers, draggable column edges |
| `TabBar` | Tab and close-button hit targets, middle-click to close |
| `StatusBar` | Running operations, row count, timing, cancel button |
| `Modal` | Backdrop plus centred box; validates the z-order design |
| `Toast` | Transient errors, dismissable by click |

Deferred: context menu, buttons beyond the status bar, command palette, help modal.

### 7.8 Layout in M0

Simplified from the full mock — one grid pane, no cell-detail pane:

```
┌ tab bar ───────────────────────────────────────────────┐  1 row
├──────────────┬─────────────────────────────────────────┤
│ Explorer     │ Preview grid                            │  flex
│ (tree)       │                                         │
├──────────────┴─────────────────────────────────────────┤
│ status bar                                             │  1 row
└────────────────────────────────────────────────────────┘
```

Minimum usable size is 60×20; below that, render a single "terminal too small" message rather
than a broken layout.

---

## 8. Testing in M0

In-source `#[cfg(test)] mod tests` throughout.

| Subject | What is asserted |
| --- | --- |
| `Ident::quote` | Embedded quotes, dots, upper case, empty strings, both quote styles |
| `Value → Cell` | Every variant; NULL, huge decimals, full-width text, embedded newlines, `Opaque` |
| Column width sampling | Display width, not character count, on the `unicode` fixture |
| `HitMap::at` | z-order resolution, overlapping rects, exact rectangle edges |
| Gesture synthesis | Double-click window boundaries, drag continuing outside the rect |
| Intent coverage | Every mouse-reachable `IntentKind` has a `KEYMAP` entry |
| Tree flattening | Expand, collapse, lazy load, failure state |
| Use cases | `MockDriver` injected; success, failure and latency paths |
| Screen snapshots | `TestBackend` at 100×30 and 60×20; empty state, loaded grid, error modal |

Snapshot tests use `insta`. They are brittle by nature, so they cover a small number of
representative screens rather than every state.

---

## 9. CI

`.github/workflows/ci.yml`, on push and pull request:

```
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

Ubuntu only in M0; macOS is added when the first platform-specific path appears (keyring, M1).

---

## 10. Task breakdown

Ordered by dependency; each is roughly one commit.

| # | Task | Depends on |
| --- | --- | --- |
| T1 | Workspace skeleton, `rust-toolchain.toml`, shared lints, `.gitignore`, CI | — |
| T2 | `sqlake-core`: ids, `NodeRef`, `Value`, `ResultSet`, `Capabilities`, `Ident`/`QuotedIdent` | T1 |
| T3 | `sqlake-core`: `Driver` and `Session` traits, `DriverError` | T2 |
| T4 | `sqlake-driver-mock`: fixtures, latency and failure injection | T3 |
| T5 | `sqlake-app`: `Snapshot`, `Action`, `ViewCmd`, `Intent`, `IntentKind` | T2 |
| T6 | `sqlake-app`: store task, session actor, event loop | T4, T5 |
| T7 | `sqlake-app`: `RenderedGrid`, width sampling, `Value → Cell` | T2 |
| T8 | `sqlake-app`: the three use cases | T6, T7 |
| T9 | `sqlake-tui`: `TerminalGuard`, panic hook, `--panic-test`, empty render loop that quits | T1 |
| T10 | `sqlake-tui`: `HitMap`, `Target`, z constants | T9 |
| T11 | `sqlake-tui`: `MouseState`, gesture synthesis | T10 |
| T12 | `sqlake-tui`: `KEYMAP`, input mapping, intent-coverage test | T11, T5 |
| T13 | `sqlake-tui`: `UiState`, `Pane`, `Splitter`, `Scrollbar`, `StatusBar`, `TabBar` | T10 |
| T14 | `sqlake-tui`: `Tree` widget, wired to the store | T13, T8 |
| T15 | `sqlake-tui`: `DataGrid` — virtualisation, sorting, column resize | T13, T8 |
| T16 | `sqlake-tui`: `Modal`, `Toast`, error surfacing | T13 |
| T17 | `sqlake` bin: CLI flags (`--no-mouse`, `--log-level`), logging to file, wiring | T14, T15 |
| T18 | Snapshot tests, README, M0 review against §1 | T17 |

T9 through T12 are the part worth being slow about. They are cheap to write and expensive to
change once nine widgets depend on them.

---

## 11. Open questions

Resolve during implementation; none block starting.

1. **Horizontal grid scrolling granularity** — per column or per cell? Per column is simpler
   and probably better with a mouse; revisit after using it against `public.wide`.
2. **Selection model** — a single cell in M0. Whether range selection (shift-click) lands in
   M0 or M3 depends on how much of it OSC 52 copy needs, which is an M3 question.
3. **`ScrollState` shape** — whether the horizontal offset is a column index or a cell column.
   Tied to question 1.
4. **`insta` snapshot volume** — start with three screens. If they churn on every widget
   change, cut back to one and rely on unit tests instead.
