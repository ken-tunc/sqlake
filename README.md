# sqlake

A mouse-friendly database client for the terminal.

Scope is deliberately narrow — PostgreSQL and BigQuery, built as a personal tool. Every action
is reachable with the mouse, and every one of those actions also has a key binding.

> **Status: design complete, implementation not started.** Nothing here runs yet.

## Design

- [Design](docs/design.md) — architecture, driver model, type design, milestones
- [M0 — Foundation](docs/design-m0.md) — the current milestone

## Planned features

1. Connection management
2. Table listing
3. Table preview
4. SQL execution (editing delegated to `$EDITOR`)
5. Table definitions — columns, indexes, triggers
6. Proxy and tunnel settings
7. SQL templates
8. Query history

## Notable decisions

- **Built on ratatui.** Immediate-mode rendering means layout rectangles are in hand at draw
  time, which makes mouse hit testing a matter of `rect.contains(pos)`. See
  [ADR-001](docs/design.md#2-choosing-the-ui-foundation-adr-001).
- **No built-in SQL editor.** The buffer is handed to `$EDITOR`, the same way `git commit`
  does it, so an existing neovim setup applies unchanged.
- **Use case inputs and outputs are types.** "Before" and "after" are distinct types, so a
  skipped step is a compile error — most importantly, a BigQuery query cannot be executed
  without first being estimated and approved.

## License

MIT
