# sqlake

A mouse-friendly TUI database client for PostgreSQL and BigQuery, written in Rust.
Read [docs/design.md](docs/design.md) before making architectural decisions;
[docs/design-m0.md](docs/design-m0.md) is the current milestone.

## Architecture rules

These are load-bearing. Breaking one silently undoes a design decision.

- **Dependencies flow one way**: `sqlake` → `sqlake-tui` → `sqlake-app` → `sqlake-core`,
  with `sqlake-driver-*` depending only on `sqlake-core`.
- **No driver branching in the UI.** `if driver == Postgres` never appears in `sqlake-tui`.
  Express the difference as a field on `Capabilities` instead.
- **The UI never sees `Value`.** It receives `RenderedGrid`, which formats cells on demand.
- **Mouse and keyboard produce the same `Intent`.** Anything reachable by mouse must have a
  `KEYMAP` entry; the coverage test in `input.rs` enforces this.
- **View-local state does not round-trip through the store.** Scrolling, selection, column
  widths and split positions are `ViewCmd`, applied synchronously to `UiState`.
- **Terminal mode changes happen only in `TerminalGuard`.** The panic hook and the `$EDITOR`
  launch both go through it.
- **Never write to stdout while the TUI is up.** `tracing` goes to a log file only.

## Testing

- **In-source tests by default** — `#[cfg(test)] mod tests` at the bottom of the file. Do not
  widen visibility to `pub(crate)` just to reach something from a test.
- `tests/` is reserved for behaviour observable from outside the crate (driver conformance).
- Screen snapshots use `insta` with ratatui's `TestBackend`.
- `sqlake-driver-mock` means every test runs with no database and no network.
