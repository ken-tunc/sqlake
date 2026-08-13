# M0 — Foundation

The scaffolding every later milestone sits on. Architecture lives in
[design.md](design.md); this document holds only what is specific to M0 — what is in scope,
the decisions M0 makes on its own, and the order the work happens in.

M0 ships **no real database driver**. Everything is exercised against `sqlake-driver-mock`.

---

## 1. Definition of done

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
| `sqlake-app` | `Snapshot`, `Action`, store task, session actor, three use cases, `PagedResult` |
| `sqlake-tui` | `TerminalGuard`, render loop, `HitMap`, gestures, input mapping, `RenderedGrid`, `UiState`, nine widgets |
| Tests | in-source unit tests, `insta` screen snapshots, the intent-coverage test |

Widgets built in M0: `Pane`, `Splitter`, `Scrollbar`, `Tree`, `DataGrid`, `TabBar`,
`StatusBar`, `Modal`, `Toast`. Deferred: context menu, buttons beyond the status bar, command
palette, help modal.

Use cases built in M0: `Connect`, `ExpandNode`, `PreviewTable`.

Staged types built in M0: `Ident` → `QuotedIdent`, and
`RowBatch` → `ResultSet` → `PagedResult` → `RenderedGrid`, whose last stage lives in
`sqlake-tui` because every decision in it is a terminal decision. The others arrive with the
milestone that constructs them — the SQL pipeline in M4, `ResolvedProfile` in M1, templates in
M7.

### Out of scope

Deliberately deferred, with the milestone that picks each one up:

| Deferred | Lands in |
| --- | --- |
| PostgreSQL and BigQuery drivers | M1 / M2 |
| Config files, profiles, keyring (`sqlake-config`) | M1 |
| SQLite history and templates (`sqlake-store`) | M7 / M8 |
| `$EDITOR` integration | M4 |
| `describe()` and the definition view | M5 |
| Tunnels and proxies | M6 |
| Context menu, cell detail popover, OSC 52 copy | M3 |
| Command palette, help modal, theming | M4 |

### Layout in M0

Simplified from the full screen: one grid pane, no cell-detail pane.

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

## 3. Decisions

**D1 — create five crates, not eleven.** `sqlake-config`, `sqlake-store` and the two real
drivers are created in the milestone that first needs them. Empty placeholder crates are dead
weight and hide which parts actually exist.

**D2 — `Session` carries only what M0 uses**: `children`, `preview`, `close`. Declaring
`describe`, `estimate` and `execute` now would force stub types (`PreparedSql`,
`ApprovedQuery`, `TableDetail`) into existence months before anything constructs one.
`Driver::connect` likewise takes no profile until `sqlake-config` exists in M1.

**D3 — not every input becomes a store action.** Scrolling, moving the selected cell, resizing
a column and moving a splitter are view-local. Routing them through an async task would add a
round trip to every wheel tick and make the UI feel laggy. This is what the
`Intent::View` / `Intent::App` split exists for.

**D4 — format cells lazily; sample widths eagerly.** The naive version materialises
`Vec<Vec<Cell>>`, which at 200k rows by 60 columns allocates twelve million strings to display
thirty of them. `RenderedGrid` holds the rows and formats on access. No cache in M0; if
profiling in M3 shows formatting cost during fast scrolling, a row-window cache goes behind
the same API and the signature does not change.

**D5 — a modal pushes a full-screen backdrop rectangle.** Without it a click outside the modal
falls through, and a confirmation dialog becomes a way to trigger the thing it was confirming.

**D6 — the mock driver injects latency and failures.** Without them, loading states and error
surfaces get written blind and a UI that blocks is only discovered once a real driver lands on
top of it.

**D7 — flattening the tree happens in the app layer.** Expansion state drives lazy loading, so
it is data and belongs in the snapshot. What the UI receives is already flat, so rendering is
a slice and an index. Scroll offset and selection stay in `UiState`.

---

## 4. Workspace setup

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
  `missing_debug_implementations = "warn"`, `unreachable_pub = "warn"`, `clippy::all`, plus a
  curated set from `clippy::pedantic`. Not the whole pedantic group — it is too noisy to keep
  at deny.
- Lints are `warn` in the manifest and promoted with `-D warnings` in CI, so a local build
  stays workable while nothing lands with a warning.
- Errors: `thiserror` in library crates, each with its own error enum. `anyhow` only in the
  binary.

---

## 5. The mock fixture

The mock is not a stub — it is the fixture every other crate is developed against, so it is
built to exercise the hard cases from day one (D6).

```rust
pub struct Behaviour {
    pub latency: Duration,
    pub connect_fails: bool,                    // the most common real failure
    pub failing_nodes: Vec<Vec<String>>,        // expansion or preview always fails
    pub flaky_nodes: Vec<(Vec<String>, u32)>,   // fails n times, then succeeds
    pub failing_after: Vec<(Vec<String>, u32)>, // succeeds n times, then fails
    pub slow_nodes: Vec<Vec<String>>,
    pub slow_latency: Duration,
}
```

`flaky_nodes` exists because a permanent failure can only test a retry up to the point of
failing again. The part a user sees — the error clearing, the children arriving, the spinner
stopping — needs a failure that stops.

`failing_after` is its mirror, and the only way to reach a failure that arrives *after*
something is already on screen: a second page that does not come back while the first is still
displayed. Without it, "keep what is already there when the next page fails" cannot be tested
at all.

Every path a `Behaviour` names is **checked against the catalogue** when the driver is built,
and injecting on a node that is not there is a panic. Injection that silently matches nothing
is worse than no injection: renaming a fixture would leave every test green while the error
path it exercised quietly stopped being exercised.

One connection, three schemas, and relations chosen to break naive rendering:

| Relation | Purpose |
| --- | --- |
| `public.users` | The ordinary case. Whatever renders correctly here is not yet proven |
| `public.types_showcase` | One column per `Value` variant, plus an all-null row, an extremes row and a NaN |
| `public.wide` | 60 columns of unequal width — forces horizontal scrolling and width negotiation |
| `public.big` | 200,000 rows, generated on demand — proves virtualisation |
| `public.unicode` | CJK, ZWJ emoji, combining marks, fullwidth Latin, RTL, and the control characters that repaint a terminal |
| `public.empty` | Columns but no rows — the empty state has to look deliberate |
| `public."Mixed.Case"` | Identifiers that break unquoted interpolation: upper case, an embedded dot, a space, a reserved word, a non-ASCII header |
| `analytics.broken` | Preview always fails — the grid's error path |
| `analytics.slow` | Preview takes two seconds — the loading path |
| `analytics.unbounded` | Reports **no** total row count — every division by a total has to survive it |
| schema `restricted` | Expansion always fails — the tree's error path |

Three of these are load-bearing in a way that is easy to lose by accident, so a test pins each:

- `public.unicode` matters more than it looks: **display width is not character count**, and
  getting it wrong corrupts everything to the right of the mistake. The row that is widest on
  screen and the row that is longest in `char`s are **deliberately different rows** — if one
  row were widest under both, a sampler counting characters would lay the table out exactly
  like a correct one and neither would be caught.
- `public.wide`'s columns are **unequal**. Sixty columns that all want the same width cannot
  tell an even split apart from a negotiated layout.
- `analytics.unbounded` exists because `ResultSet::total_rows` documents `None` as the common
  case for real drivers. A fixture set where the total is always known lets the scrollbar,
  the row counter and jump-to-last-page be written against a guarantee BigQuery does not give.

The failing-expansion case hangs off a schema rather than a relation because relations do not
expand — putting it on a table would not exercise the tree's error path at all. That schema
holds a relation anyway: failure lives in `Behaviour`, so with injection off it must expand to
something rather than claim children and then produce none.

The mock hierarchy is **two levels deep** against PostgreSQL's three, and a three-level
hierarchy is available through `MockDriver::with_capabilities`. Both halves are needed. The
short hierarchy stops the tree assuming PostgreSQL's shape; being able to switch stops it
assuming the mock's, which — with the mock as M0's only driver — is otherwise the one shape it
ever sees.

---

## 6. Testing in M0

| Subject | What is asserted |
| --- | --- |
| `Ident::quote` | Embedded quotes, dots, upper case, empty strings, both quote styles |
| `Value → Cell` | Every variant; NULL, huge decimals, full-width text, embedded newlines, `Opaque` |
| Column width sampling | Display width, not character count, against the `unicode` fixture |
| `HitMap::at` | z-order resolution, overlapping rectangles, exact rectangle edges |
| Gesture synthesis | Double-click window boundaries, drag continuing outside the rectangle |
| Intent coverage | Every mouse-reachable `IntentKind` has a `KEYMAP` entry |
| Tree flattening | Expand, collapse, lazy load, failure state, retry |
| Use cases and store | `MockDriver` injected; success, failure and latency paths |
| Screen snapshots | `TestBackend` at 100×30 and 60×20; empty state, loaded grid, error modal |

---

## 7. CI

`.github/workflows/ci.yml`, on push and pull request:

```
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

Ubuntu only in M0; macOS is added when the first platform-specific path appears (keyring, M1).

---

## 8. Task breakdown

Ordered by dependency; each is roughly one commit.

| # | Task | Depends on |
| --- | --- | --- |
| T1 | Workspace skeleton, `rust-toolchain.toml`, shared lints, `.gitignore`, CI | — |
| T2 | `sqlake-core`: ids, `NodeRef`, `Value`, `ResultSet`, `Capabilities`, `Ident`/`QuotedIdent` | T1 |
| T3 | `sqlake-core`: `Driver` and `Session` traits, `DriverError` | T2 |
| T4 | `sqlake-driver-mock`: fixtures, latency and failure injection | T3 |
| T5 | `sqlake-app`: `Snapshot`, `Action`, `TreeState` | T2, T7 |
| T6 | `sqlake-app`: store task, session actor, event loop | T4, T5 |
| T7 | `sqlake-tui`: `RenderedGrid`, width sampling, `Value → Cell` | T2 |
| T8 | `sqlake-app`: the three use cases | T6, T7 |
| T9 | `sqlake-tui`: `TerminalGuard`, panic hook, `--panic-test` | T1 |
| T10 | `sqlake-tui`: `HitMap`, `Target`, z levels | T9 |
| T11 | `sqlake-tui`: `MouseState`, gesture synthesis | T10 |
| T12 | `sqlake-tui`: `Intent`, `KEYMAP`, input mapping, coverage test | T11, T5 |
| T13 | `sqlake-tui`: `UiState`, `Pane`, `Splitter`, `Scrollbar`, `StatusBar`, `TabBar` | T10 |
| T14 | `sqlake-tui`: `Tree` widget, wired to the store | T13, T8 |
| T15 | `sqlake-tui`: `DataGrid` — virtualisation, sorting, column resize | T13, T8 |
| T16 | `sqlake-tui`: `Modal`, `Toast`, error surfacing | T13 |
| T17 | `sqlake` bin: CLI flags (`--no-mouse`, `--log-level`), logging to file, wiring | T14, T15 |
| T18 | Snapshot tests, README, review against §1 | T17 |

T9 through T12 are the part worth being slow about. They are cheap to write and expensive to
change once nine widgets depend on them.

---

## 9. Open questions

Resolve during implementation; none block starting.

1. **Horizontal grid scrolling granularity** — per column or per cell? Per column is simpler
   and probably better with a mouse; revisit after using it against `public.wide`.
2. **Selection model** — a single cell in M0. Whether range selection lands in M0 or M3
   depends on how much of it OSC 52 copy needs, which is an M3 question.
3. **`ScrollState` shape** — whether the horizontal offset is a column index or a cell column.
   Tied to question 1.
4. **`insta` snapshot volume** — start with three screens. If they churn on every widget
   change, cut back to one and rely on unit tests instead.
