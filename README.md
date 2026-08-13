# sqlake

A mouse-friendly database client for the terminal.

Scope is deliberately narrow — PostgreSQL and BigQuery, built as a personal tool. Every action
is reachable with the mouse, and every one of those actions also has a key binding.

> **Status: design complete, implementation not started.** Nothing here runs yet.

## Design

- [Design](docs/design.md) — architecture, driver model, type design, milestones
- [M0 — Foundation](docs/design-m0.md) — the current milestone
- [Agent surface](docs/design-agent.md) — driving sqlake from an AI agent

## Planned features

1. Connection management
2. Table listing
3. Table preview
4. SQL execution (editing delegated to `$EDITOR`)
5. Table definitions — columns, indexes, triggers
6. Proxy and tunnel settings
7. SQL templates
8. Query history

Plus an agent surface: the same operations as a socket API, CLI subcommands and an MCP server,
so an AI agent can use the connections and guardrails the interactive client already has.

## License

MIT
