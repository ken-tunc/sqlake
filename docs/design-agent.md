# Agent surface — driving sqlake without the TUI

An AI agent should be able to inspect and query a database through sqlake, using the same
connections, the same guardrails and the same history as the interactive client.

Architecture lives in [design.md](design.md). This document holds only what is specific to the
agent surface.

---

## 1. Why this is cheap

The application layer already has no idea a terminal exists. `sqlake-app` owns connections,
the object tree, use cases and an immutable `Snapshot`; `sqlake-tui` is one front-end over it.

**The agent surface is a second front-end over the same layer, not new logic.** Everything
below is transport, serialisation and policy. If any of it needs a new use case, that is a
sign the interactive client is missing a feature too.

This is what the UI/logic split in §1 of the architecture was for. It is worth stating
explicitly so it stays true: **no query construction and no driver knowledge** may live in the
agent surface.

Rendering is the opposite case, and the distinction is easy to get backwards. The shared type
is `PagedResult` — the rows as the driver returned them, with no display decision attached —
and **each front-end renders it for itself**. The TUI's rules are actively wrong here: `∅` does
not parse as null, a JSON document collapsed to `{2 keys}` destroys exactly what was asked
for, and a newline replaced by `␊` corrupts the text. So `sqlake-api` owns its own
`Value` → JSON serialisation, and reaching for `sqlake-tui`'s formatter would be a bug rather
than reuse — which is what the peer relationship in architecture §2 is protecting.

---

## 2. Shape

Modelled on herdr: a long-lived process holding session state, a socket API against it, and
thin noun-verb subcommands over that API.

```
sqlake                          launch the TUI (unchanged)
sqlake --session work           launch the TUI and listen on a named socket

sqlake api snapshot             print the live Snapshot as JSON
sqlake api schema               print the request/response schema

sqlake connection list|open|close
sqlake schema list              namespaces in a connection
sqlake table list|describe|preview
sqlake query estimate|run|status|cancel|wait
sqlake history search

sqlake mcp                      speak MCP on stdio
```

Every subcommand prints JSON on stdout and diagnostics on stderr, so output is consumable
without parsing prose.

### 2.1 Two ways to reach a connection

| Mode | When | How |
| --- | --- | --- |
| **Attached** | A `sqlake` session is running | Connect to its socket. Reuses live connections, live tunnels and already-satisfied auth |
| **One-shot** | No session, or `--no-attach` | Start the store in-process, run the command, tear down |

Attached mode is the reason a server exists at all. Connections behind a bastion, an MFA
prompt or a `gcloud auth` flow are expensive to establish; making an agent re-establish them
per command is both slow and, for anything interactive, impossible. Reusing the session the
human already opened solves that, and it is exactly herdr's value proposition.

One-shot mode is what makes the surface usable in CI and in a fresh shell, and it is also the
simpler of the two, so it is built first.

### 2.2 Socket

`$XDG_RUNTIME_DIR/sqlake/<session>.sock` (`~/.local/state/sqlake/run/` as a fallback), mode
`0600`, owner-only. No TCP listener, ever — this process holds live database connections and
resolved credentials.

Requests and responses are newline-delimited JSON. The protocol types live in a new
`sqlake-api` crate that both the server and the client depend on, so the two cannot drift.

### 2.3 `api schema`

`sqlake api schema` prints the full request and response schema, generated from the protocol
types rather than maintained by hand.

This exists because an agent that can read the schema does not need the surface documented in
its prompt, and a schema generated from the types cannot describe a call that does not exist.
The same output feeds the MCP tool definitions in §5.

---

## 3. Safety

An agent driving a database is where the type-level guards stop being theoretical. None of
this is new machinery; it is the existing guards with an agent-shaped policy on top.

### 3.1 Cost: `ApprovedQuery` is already the answer

`Session::execute` accepts only an `ApprovedQuery`, so no caller — agent or human — can run a
query that was never estimated. What differs for an agent is how approval is granted:

- A **byte budget** rather than a dialog. `agent.max_bytes_billed` in `config.toml`, defaulting
  well below the profile's own ceiling.
- Within budget → the query runs.
- Over budget → the API returns the `NeedsApproval` variant with the estimate. The agent cannot
  turn that into an approval itself; it has to surface the number to a human, who approves it
  through the TUI or by re-issuing with `--approve-up-to`.

`NeedsApproval` being a normal output rather than an error (architecture §4.2) is what makes
this a clean protocol response instead of an error path with special handling.

### 3.2 Write access is opt-in

Agent sessions are **read-only by default**.

- PostgreSQL: the connection sets `default_transaction_read_only = on`, so enforcement is the
  server's, not ours.
- BigQuery has no equivalent, so classification happens at `ValidatedSql`, which already
  distinguishes single from multiple statements. Adding statement *kind* there gives a single
  place to reject DML and DDL — and it protects the interactive client's read-only profiles
  at the same time.

Turning it off is per-profile and explicit: `agent = { write = true }`. There is no flag that
grants write access for one command.

### 3.3 Output budget

An agent that pulls 200,000 rows into its context has not read the table; it has destroyed its
own working memory. So:

- A default row limit, lower than the TUI's page size.
- Truncation is **always explicit** in the response: `{ "rows": [...], "returned": 200,
  "total": 200000, "truncated": true }`. Never silently cut.
- Wide results are the same problem sideways: a column budget, with the omitted column names
  listed so the agent knows what it did not see.

### 3.4 Audit

Every agent-issued query goes into the same `query_history` table as a human's, with an
`issuer` column (`human`, or `agent:<name>`). Failures included.

One history, one place to look when something unexpected happened to the data.

### 3.5 Secrets never cross the socket

`Snapshot` carries no `ResolvedProfile` today, and serialisation must not be the thing that
changes that. The connection view exposes an id, a name, a driver kind and a status — never a
host, a user or anything derived from a credential.

### 3.6 An `Action`'s parameters are only as bounded as the caller

The TUI cannot construct most bad actions: a column index comes from the grid it drew, a
`TableRef` from a node the tree loaded. A socket has neither, so every `Action` field it can
set becomes an input the store has to bound itself.

`SortPreview` is the worked example, and it is half done. M2/T1 added the capability check —
a connection that cannot order a preview is refused rather than left to the driver — but the
column index is still whatever arrived. Out of range, the driver rejects it, `preview.sort`
keeps it, and `preview_table`'s retry re-issues the same doomed request for as long as the
preview exists. The rule is that the store validates what a front-end used to make
unrepresentable, and A1 cannot land without a sweep of `Action` for the rest of it.

---

## 4. Asynchrony

Queries are not instantaneous, and an agent blocking on a socket read for four minutes is a
poor client.

`query run` returns a handle immediately. `query status <id>` reports progress, and
`query wait <id> --timeout 30s` blocks until the query finishes, fails or the timeout expires
— the same shape as `herdr agent wait`. `query cancel <id>` maps onto the existing
`CancelHandle`, so an agent can stop something it started.

This is the one place the agent surface needs something the TUI does not: a stable, external
id for a running query. `BusyId` is process-local and already exists; it becomes the handle.

---

## 5. MCP

`sqlake mcp` speaks MCP over stdio, wrapping the same client used by the subcommands. Tools
map one-to-one onto the commands in §2, and their definitions are generated from the same
schema as `api schema`.

It is a separate crate (`sqlake-mcp`) so the MCP SDK stays out of the TUI's dependency tree.

MCP comes *after* the CLI rather than instead of it. The CLI is testable with a shell, usable
by any agent that can run a command, and is the thing the MCP server is implemented in terms
of. Building MCP first would mean debugging two layers at once.

---

## 6. Crates

```
crates/
├── sqlake-api/     # protocol types, schema generation, socket client and server
└── sqlake-mcp/     # MCP stdio server over sqlake-api
```

Both depend on `sqlake-app`. Neither depends on `sqlake-tui`, and `sqlake-tui` does not depend
on them — the TUI only needs to know how to start a listener, which is `sqlake-api`'s job.

That rule has teeth rather than being decorative: `sqlake-tui` holds the only existing
`Value` formatter, so "neither depends on `sqlake-tui`" is what stops `sqlake-api` reusing it.
Value serialisation is `sqlake-api`'s own module (§1).

---

## 7. Milestones

The agent surface does not arrive in one piece. Each part becomes possible at a different
point, so it is **a track alongside M1–M8 rather than a milestone after them** — which also
keeps M1–M8 aligned one-to-one with the eight features.

| # | Lands after | Content | Done when |
| --- | --- | --- | --- |
| **A1** | M2 | Read-only CLI and socket API | `connection list`, `schema list`, `table list`, `table preview`, `api snapshot`, `api schema`. JSON output with explicit truncation. Both one-shot and attached modes work against both drivers |
| **A2** | M4 | Query execution over the API | `query estimate\|run\|status\|wait\|cancel`, the byte budget and `NeedsApproval`, read-only enforcement, `issuer` in history |
| **A3** | A2 | MCP server | `sqlake mcp` exposes the same operations as MCP tools, generated from the same schema |

Execution order: **M0 → M1 → M2 → A1 → M3 → M4 → A2 → A3 → M5 → M6 → M7 → M8.**

A3 sitting before M5 is a preference, not a constraint; it can slide later if the interactive
client turns out to want the attention more.

### Why A1 lands after M2

Nothing in A1 needs a terminal. Connecting, walking the object tree and reading a page of a
relation are the three use cases M0 already builds, and M2 is the point at which the real
drivers implement them. Preview is available then too: `Session::preview` exists from M0, and
M3 is about the *interactive* preview — paging, sorting, cell detail, copying — not about
whether a page of rows can be fetched.

Building A1 there is worth more than the feature itself: it exercises `sqlake-app` end to end
through a second front-end while the interactive client is still half-written. A layering
mistake shows up as "the CLI cannot do this without reaching into the TUI", which is exactly
the failure the architecture is meant to prevent, and it is much cheaper to hear about in M2
than once eight milestones are built on top of it.

It also front-loads the honest version of the store's headless story. `Store::spawn` is
already used without a terminal by its own tests; A1 makes that a shipped path rather than a
test-only one.

### What pulling it forward forces

**`api snapshot` prints a wire type, not `Snapshot`.** The internal snapshot holds `Instant`
and `Arc<PagedResult>` — neither of which means anything on the far side of a socket — and
deriving `Serialize` on it would make every future field change a protocol change. So
`sqlake-api` owns a separate wire representation and the conversion into it.

`PagedResult` is where the rows stop and the front-end begins: it carries `Value`s and no
display decision, so the TUI turns it into a `RenderedGrid` and `sqlake-api` serialises it as
JSON. Those two want opposite things from the same rows — a collapsed `{2 keys}` is right on
screen and destroys what an agent asked for — which is why nothing above `PagedResult` is
shared between them.

That decision would have been easy to get wrong by default. Making it in M2, before anything
depends on the shape, is most of the reason to pull A1 forward at all.

`Snapshot` needed the same correction one level up, and now has it: `tabs`, `active_tab` and
`Toast` — `sqlake-tui`'s vocabulary, not `sqlake-app`'s — moved out before this document was
built on top of them. A page is addressed by `(ConnId, TableRef)`, fetched and cached in
`sqlake-app`, reachable by whoever asks for it again; which of those a screen has open, their
order, and which has focus is `sqlake-tui`'s own `UiState`. `api snapshot` (§2.2) now serialises
a `Snapshot` with nothing on it that only makes sense in front of a human — the wire type still
has to exist for `Instant` and `Arc`, but it is no longer also deciding what to leave out.

## 8. Open questions

1. **Session naming and discovery.** herdr uses `--session <name>` with a default session. Is
   one implicit session enough here, or does per-project addressing matter?
2. **Whether `query run` should stream.** NDJSON on the socket would let an agent process rows
   as they arrive, but the row-budget in §3.3 makes streaming less useful than it sounds.
3. **Schema format.** JSON Schema is the obvious choice and MCP wants it anyway; worth
   confirming before generating anything by hand.
4. **Whether the TUI should show agent activity.** A line in the status bar when an
   agent-issued query is running would make the shared session legible rather than spooky.
