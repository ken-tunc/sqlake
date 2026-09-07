# sqlake

A mouse-friendly TUI database client for PostgreSQL and BigQuery, written in Rust.
Read [docs/design.md](docs/design.md) before making architectural decisions. It holds what has
not been built yet; for what has, the code and its doc comments are the documentation.
A `docs/design-m<N>.md`, when there is one, is the milestone being built; it goes when that
milestone is done.

## Architecture rules

These are load-bearing. Breaking one silently undoes a design decision.

- **Dependencies flow one way**: `sqlake` → `sqlake-tui` / `sqlake-api` → `sqlake-app` →
  `sqlake-core`, with `sqlake-driver-*` depending only on `sqlake-core`. The two front-ends
  are peers and **neither may depend on the other** — something both want goes down to
  `sqlake-app`, which is how `json::to_json` got there.
- **No driver branching in the UI.** `if driver == Postgres` never appears in `sqlake-tui`.
  Express the difference as a field on `Capabilities` instead.
- **Drawing code never sees `Value`.** It receives `RenderedGrid`, which formats cells on
  demand, or `RenderedDetail`, which lays one out in full. `sqlake-api` *does* see `Value`:
  an agent wants the document, not `{2 keys}`.
- **Nothing in `sqlake-app` assumes a character-cell display.** No widths, no glyphs, no
  elision. `sqlake-tui` and `sqlake-api` are peers over that layer and want opposite
  renderings of the same rows, so anything that picks one belongs in the front-end.
- **Mouse and keyboard produce the same `Intent`.** Anything reachable by mouse must have a
  `KEYMAP` entry; the coverage test in `input.rs` enforces this.
- **View-local state does not round-trip through the store.** Scrolling, selection, column
  widths and split positions are `ViewCmd`, applied synchronously to `UiState`.
- **Terminal mode changes happen only in `TerminalGuard`.** The panic hook and the `$EDITOR`
  launch both go through it.
- **Never write to stdout while the TUI is up.** `tracing` goes to a log file only. The one
  thing that does write is the OSC 52 copy, and it goes through the terminal's own writer
  after a frame — the renderer's channel, not a second one.

## Testing

- **In-source tests by default** — `#[cfg(test)] mod tests` at the bottom of the file. Do not
  widen visibility to `pub(crate)` just to reach something from a test.
- `tests/` is reserved for behaviour observable from outside the crate (driver conformance).
- Screen snapshots use `insta` with ratatui's `TestBackend`.
- `sqlake-driver-mock` means every test runs with no database and no network.
