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

`crates/` is the list of what exists. What is still to come inside M0: `UiState`, the nine
widgets — `Pane`, `Splitter`, `Scrollbar`, `Tree`, `DataGrid`, `TabBar`, `StatusBar`, `Modal`,
`Toast` — the binary that wires them together, and screen snapshots.

Deferred past M0: context menu, buttons beyond the status bar, command palette, help modal.

Staged types are built by the milestone that constructs them, so `RawSql` → `ApprovedQuery`
arrives in M4, `ResolvedProfile` in M1 and templates in M7.

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

Only the ones no file states for itself. How a thing works belongs with that thing; what is
left here is mostly about what was deliberately *not* built, which nothing can say by
existing.

**D1 — create five crates, not eleven.** `sqlake-config`, `sqlake-store` and the two real
drivers are created in the milestone that first needs them. Empty placeholder crates are dead
weight and hide which parts actually exist.

**D5 — a modal pushes a full-screen backdrop rectangle.** Without it a click outside the modal
falls through, and a confirmation dialog becomes a way to trigger the thing it was confirming.

---

## 4. Remaining tasks

T1 through T12 are merged; the code and `git log` are the record of them. What is left:

| # | Task | Depends on |
| --- | --- | --- |
| T13 | `sqlake-tui`: `UiState`, `Pane`, `Splitter`, `Scrollbar`, `StatusBar`, `TabBar` | — |
| T14 | `sqlake-tui`: `Tree` widget, wired to the store | T13 |
| T15 | `sqlake-tui`: `DataGrid` — virtualisation, sorting, column resize | T13 |
| T16 | `sqlake-tui`: `Modal`, `Toast`, error surfacing | T13 |
| T17 | `sqlake` bin: CLI flags (`--no-mouse`, `--log-level`), logging to file, wiring | T14, T15 |
| T18 | `insta` snapshots at 100×30 and 60×20 — empty state, loaded grid, error modal — README, review against §1 | T17 |

`UiState` caches the `RenderedGrid` it builds from a `PagedResult` and rebuilds only when the
rows change, which `RenderedGrid::is_for` is there to answer.

`LoadMore` stays pressable at the end of a relation: the total row count is often unknown — a
BigQuery preview never reports one — so the store is built to survive the press rather than
the view to prevent it.

---

## 5. Open questions

Resolve during implementation; none block starting.

1. **Horizontal grid scrolling granularity** — per column or per cell? Per column is simpler
   and probably better with a mouse; revisit after using it against `public.wide`.
2. **Selection model** — a single cell in M0. Whether range selection lands in M0 or M3
   depends on how much of it OSC 52 copy needs, which is an M3 question.
3. **`ScrollState` shape** — whether the horizontal offset is a column index or a cell column.
   Tied to question 1.
4. **`insta` snapshot volume** — start with three screens. If they churn on every widget
   change, cut back to one and rely on unit tests instead.
