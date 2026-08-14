# M0 — Foundation

The scaffolding every later milestone sits on. Architecture lives in
[design.md](design.md); this document holds only what is specific to M0 — what is in scope,
the decisions M0 makes on its own, and the order the work happens in.

M0 ships **no real database driver**. Everything is exercised against `sqlake-driver-mock`.

---

## 1. Definition of done

All seven met, checked against the code rather than from memory:

| | | |
| --- | --- | --- |
| 1 | `cargo run` opens a full-screen TUI with a mock connection in the explorer | ✅ |
| 2 | The tree and the grid are driveable entirely with the mouse | ✅ |
| 3 | Every one of those operations also has a key binding, enforced by a test | ✅ |
| 4 | Loading and error states are visible; neither freezes the UI | ✅ |
| 5 | Quitting restores the terminal. Panicking restores the terminal | ✅ |
| 6 | `cargo test` passes with no database and no network | ✅ |
| 7 | `cargo fmt --check`, `cargo clippy -- -D warnings` and `cargo test` run in CI | ✅ |

On 3, the enforcement is `every_capability_reachable_with_the_mouse_has_a_key_binding` plus
`every_binding_produces_the_capability_it_claims` — the first would pass on a binding that
names a capability and does nothing, which is why there are two. Adding a `Target` or a
`Gesture` stops the crate compiling until it has a sample.

On 4, the binary runs on `Behaviour::fixture()`, so the latency, the failing schema and the
two-second relation are on screen the moment it starts rather than only in tests.

On 5, the panic hook restores through the same `restore` the guard's `Drop` uses, and then
ends the process: tokio catches a panic in a spawned task, so returning would leave the loop
drawing over the shell it had just handed back. `--panic-test` exists to prove the first half
by hand. SIGTERM is still a way to leave the terminal in raw mode, and is not covered.

What M0 does **not** have, so that a later milestone is not surprised: no real driver, no
config, no history, no `$EDITOR`, no context menu, no cell-detail popover, no OSC 52 copy, and
`Modal` is raised only by a failed connection — confirmations arrive with the operations that
need confirming, in M4.
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

## 4. What is left

Nothing. M0 is finished; `git log` is the record of how.

## 5. Questions M0 answered

Left open when M0 started, and settled by building it.

1. **Horizontal scrolling is per column.** `GridUi::col_offset` is a column index. A partly
   drawn leading column reads worse than a hard edge, and the wheel and the scrollbar both
   move in column steps anyway. Question 3 went with it.
2. **Selection is a single cell.** Range selection stays out until M3 decides how much of it
   OSC 52 copy actually needs; nothing in M0 wanted it.
3. **Six snapshots, not three.** They have not churned: a widget change moves a screen, which
   is the point, and the two that caught something — a column scrolled off by a selection, and
   the sizes at 60×20 — would not have been caught by a unit test, because both were about
   what the whole frame looked like rather than about any one rectangle.

## 6. What M0 chose to leave

- **The mock is the only driver.** Every capability the UI reads has exactly one answer in M0,
  which `MockDriver::with_capabilities` exists to work around in tests; M2 gives it a second.
- **A dialog is raised only by a failed connection.** `Modal` and its backdrop are built, and
  the confirmations D5 was written for arrive with the operations that need confirming (M4).
- **SIGTERM leaves the terminal in raw mode.** The panic path is covered, and it is the one
  the definition of done names, but a signal does not unwind and `Drop` does not run.
