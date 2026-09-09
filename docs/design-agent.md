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

Built, as of A3:

```
sqlake                          launch the TUI (unchanged)
sqlake --session work           launch the TUI and listen on a named socket

sqlake api snapshot             print the live Snapshot as JSON
sqlake api schema               print the request/response schema

sqlake connection list
sqlake connection open|close    against a session that outlives the command
sqlake schema list              namespaces in a connection
sqlake table list|preview|describe
sqlake template list|apply    saved statements; `apply` answers SQL and runs nothing
sqlake history search         what this session has run, and what each run cost
sqlake query estimate|run|status|wait|cancel
sqlake mcp                      speak MCP on stdio
```

Every subcommand prints JSON on stdout and diagnostics on stderr, so output is consumable
without parsing prose.

### 2.1 Two ways to reach a connection

| Mode | When | How |
| --- | --- | --- |
| **Attached** | A session is listening on the socket `--session`/`$SQLAKE_SESSION` names, and the command needs one | Connect to its socket. Reuses live connections, live tunnels and already-satisfied auth |
| **One-shot** | Nothing is listening on the socket the name resolves to | Start the store in-process, run the command, tear down |

Attached mode is the reason a server exists at all. Connections behind a bastion, an MFA
prompt or a `gcloud auth` flow are expensive to establish; making an agent re-establish them
per command is both slow and, for anything interactive, impossible. Reusing the session the
human already opened solves that, and it is exactly herdr's value proposition.

One-shot mode is what makes the surface usable in CI and in a fresh shell, and it is also the
simpler of the two, so it is built first.

It answers what a store that dies with the command can honestly answer. Reading and estimating
it can; `connection open` and `query run` it cannot, because both hand back something — a
connection, a handle — that nothing afterwards can use. Both are refused rather than answered,
because "it worked" is the wrong thing to say about a no-op. **That leaves no way to run a
query and get its rows in one command**, which is a real gap and open question 5.

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

- A **byte budget** rather than a dialog. `agent.max_bytes_billed` in `config.toml`, refused at
  load time if it is above the session's own — a ceiling that does nothing is one somebody
  wrote expecting an effect. `query run --max-bytes` lowers it further and can never raise it.
- Within budget → the query runs.
- Over budget → the API answers `{"state": "needs_approval", "budget": …}` with the estimate.
  The agent cannot turn that into an approval itself; it has to surface the number to a human,
  who approves it in the TUI.

`NeedsApproval` being a normal output rather than an error (architecture §4.2) is what makes
this a clean protocol response instead of an error path with special handling.

### 3.2 Write access is opt-in

Agent sessions are **read-only by default**.

- PostgreSQL: the connection sets `default_transaction_read_only = on`, so enforcement is the
  server's, not ours. It catches what a keyword check cannot — a `SELECT` calling a function
  that writes.
- BigQuery has no equivalent, so `ValidatedSql::kind` is the only defence there is. Built:
  `ApprovedQuery::within` takes an `Access`, so a write on a read-only connection cannot reach
  `Session::execute`. It errs towards refusing, which is a profile setting away from being
  lifted; the other direction is how an agent deletes a table.

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
`issuer` column. Failures included.

One history, one place to look when something unexpected happened to the data.

Two words, `human` and `agent`, rather than `agent:<name>`: a name is a thing this client would
have to be told and could not check, and one that a caller chooses for itself is a label rather
than a fact. Which side of the socket a request came from is what it actually knows, and it is
what the question "was that me?" is asking.

### 3.5 Secrets never cross the socket

`Snapshot` carries no `ResolvedProfile` today, and serialisation must not be the thing that
changes that. The connection view exposes an id, a name, a driver kind and a status — never a
host, a user or anything derived from a credential.

---

## 4. Asynchrony

Queries are not instantaneous, and an agent blocking on a socket read for four minutes is a
poor client.

Built. `query run` returns a handle immediately; `query status <id>` reports progress, and
`query wait <id> --timeout-ms 30000` blocks until the query finishes, fails or the wait runs
out — a wait that runs out answers with the query as it stands rather than with a timeout,
because nothing went wrong. `query cancel <id>` stops it, and stops it at the server wherever
`capabilities.cancel` says so.

The handle is `QueryId`, not `BusyId` as this once said. `BusyId` names a piece of work in
flight and is gone the moment it finishes, so `query status` on a query that had already
finished would have had nothing to name.

The row budget rides on `query status` and `query wait` rather than on `query run`, for the
same reason: `run` answers a handle and no rows, so a limit there would be an argument that
does nothing. It cuts what is *written out* and never what the store fetched — a person sharing
the session must not inherit an agent's context window.

---

## 5. MCP

Built. `sqlake mcp` speaks MCP over stdio, wrapping the same `Backend` the subcommands use —
attached when a session answers, a store of its own otherwise. A local store here does outlive
the requests made against it, unlike a one-shot command's, so opening connections and starting
queries in one is not the no-op it would be for `sqlake query run`.

One tool per `RequestKind`, and a tool's input schema is that request's own branch of the
generated schema with two edits: the `request` tag goes, because the tool's *name* is it — so a
caller cannot write the wrong one — and `connection` stops being required, because the server
picks one the way a subcommand does. A call is deserialised as a `Request`, so
`deny_unknown_fields` and every type in the protocol apply to it unchanged.

`schema` is the one request that is not a tool: `tools/list` is the schema for an MCP client,
and offering the document as well would be the same surface described twice.

A failure is a *tool* error rather than a JSON-RPC one. The request was understood and routed;
what failed is the database, and a client handed a protocol error renders it opaquely and shows
the caller nothing.

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
| **A1** | M2 | Read-only CLI and socket API — **built**, in `sqlake-api` | `connection list`, `schema list`, `table list`, `table preview`, `api snapshot`, `api schema`. JSON output with explicit truncation. Both one-shot and attached modes work against both drivers |
| **A2** | M4 | Query execution over the API — **built** | `query estimate\|run\|status\|wait\|cancel` and `connection open\|close`. The byte budget and `NeedsApproval`, read-only enforcement. `issuer` in history waits for M8, which is where the history table arrives |
| **A3** | A2 | MCP server — **built** | `sqlake mcp` exposes the same operations as MCP tools, generated from the same schema |

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

Two of these are settled, by the code named against them.

1. ~~**Session naming and discovery.**~~ One session unless a name is given: `--session`,
   `$SQLAKE_SESSION`, then `default`. Per-project addressing is a shell convention on top of
   that — a directory that exports `SQLAKE_SESSION` has it — rather than something the client
   has to know what a project is to support. `sqlake-api::socket` carries it, including why a
   name has to be one path component.
2. **How a one-shot caller runs a query at all.** `query run` needs a session that outlives
   it, because the handle it answers with is otherwise useless — so a CI job that wants rows
   from a statement has no single command to reach them with. A `--wait` that turned run and
   wait into one call would fix it, at the cost of a subcommand that is two requests; a session
   started for the job is the answer today and is more setup than the case deserves.
3. **Whether `query run` should stream.** NDJSON on the socket would let an agent process rows
   as they arrive, and A2 did not need it: `query wait` blocks and the row budget in §3.3 caps
   what comes back anyway, so streaming would move rows the caller has already said it does not
   want. Left open rather than settled — a result too large for one message is the case that
   would decide it, and nothing has met one.
4. ~~**Schema format.**~~ JSON Schema, generated by `schemars` from the protocol types. A
   hand-written schema is a second description of the protocol that nothing checks, and it
   drifts in the direction that matters: it keeps describing a call after the call changes.
5. **Whether the TUI should show agent activity.** A line in the status bar when an
   agent-issued query is running would make the shared session legible rather than spooky.
   A2 changes this from a preference into something worth doing: an agent can now start a
   query on the session a person is using, and `[⟳ running a query]` in their status bar is
   the only thing that would tell them why their connection is busy.
