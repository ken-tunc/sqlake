# A1 — Read-only agent surface

The milestone that gives `sqlake-app` a second front-end. Architecture lives in
[design.md](design.md) and the surface's own reasoning in [design-agent.md](design-agent.md);
this document holds only what is specific to A1, and is deleted when A1 is finished — the
crates it produces are the record after that.

Everything A1 ships is transport, serialisation and policy. It adds no use case: connecting,
walking the object tree and reading a page of a relation are the three M0 built, and M2 is
where real drivers started implementing them. If A1 turns out to need a fourth, that is a
report that the interactive client is missing a feature, not a licence to write one here.

---

## 1. Definition of done

| | |
| --- | --- |
| 1 | A shell lists connections, namespaces and tables and reads a page of one, with no terminal involved |
| 2 | Every command prints JSON on stdout and diagnostics on stderr, and a result that was cut says so in the response |
| 3 | A running `sqlake --session <name>` answers the same commands over its socket, on the connections it already has open |
| 4 | `sqlake api schema` prints a schema generated from the protocol types, so it can describe no call that does not exist |
| 5 | Nothing crossing the socket carries a host, a user, or anything derived from a credential |
| 6 | An `Action` arriving from a socket cannot ask for something the store will keep asking a driver for |

Against both real drivers for 1–3, and against the mock in CI.

---

## 2. Scope

**In:** the `sqlake-api` crate — protocol types, schema generation, `Value` serialisation,
socket client and server — the noun-verb subcommands over it, one-shot and attached modes,
the sweep that bounds `Action`, and the store's answer to "when is this finished".

**Out**, with the milestone that picks it up: `query estimate|run|status|wait|cancel`, the byte
budget, `NeedsApproval`, read-only enforcement and `issuer` in history (A2); the MCP server
(A3); streaming rows (design-agent.md §8.2); a line in the TUI's status bar when an agent is
doing something (§8.4).

Not in scope because it is already built: profiles, secret resolution and `Capabilities` are
M1's and M2's, and A1 consumes them unchanged.

---

## 3. Decisions

**D1 — `Action` is bounded by the store, not by `sqlake-api`.** The TUI cannot construct most
bad actions: a column index comes from the grid it drew, a `TableRef` from a node the tree
loaded. A socket has neither, so every field it can set is an input somebody has to check, and
the only somebody that sees every front-end is the store. Checking in `sqlake-api` instead
would be a check one front-end has — and the TUI can drift into the same mistake, since
nothing stops a future gesture computing an index from stale state.

**D2 — a socket gets `ExpandNode`, not `ToggleNode`.** A toggle is a statement about a state
the caller can already see. An agent cannot see it, and in attached mode it can change between
the read and the dispatch, because a human is clicking in the same session. So the idempotent
form is its own action: `ExpandNode` loads a node's children and leaves an already-expanded one
alone. `ToggleNode` stays for the front-end that has the row in front of it. The rejected
alternative — have the client read the snapshot and decide which to send — makes every listing
a race with the human whose session it borrowed.

**D3 — "finished" is a predicate over the snapshot, not a reply to a request.** The store
publishes state; it does not answer callers, and giving it a correlation id per action would
mean two control paths over the same state that can disagree. A request/response API needs to
know when to print, so it waits for a condition — this node is loaded or failed, this preview
is ready or failed — with a timeout. What looks like the flaw is the point: in attached mode
the wait can be satisfied by work the human's clicks started, and for A1's read-only,
idempotent commands "the tree node is loaded" is exactly what was asked for regardless of who
loaded it.

**D4 — rows are arrays, and the columns are described once.** The obvious shape is an object
per row (`{"id": 1, "email": "…"}`), and it repeats every column name on every row, into a
context window that §3.3 of design-agent.md is otherwise busy protecting. Arrays also survive
a relation with two columns of the same name, which an object silently collapses.

**D5 — a `Value` becomes the JSON that loses least, not the JSON that looks best.** `Decimal`
stays a string, because a JSON number is a double in most parsers and an arbitrary-precision
numeric is exactly what the driver went to the trouble of not rounding. `Bytes` is base64 and
says so. `Struct` and `Json` are objects, `Array` is an array — the whole reason design.md
§10.2 decodes `RECORD` and `REPEATED` structurally rather than flattening them. `Opaque` keeps
both its type name and its text, so an agent can see that the driver did not understand a value
rather than reading the fallback as the value. The TUI's rules are all wrong here, which is
what "the two front-ends are peers" means in practice.

**D6 — the row and column budgets belong to `sqlake-api`, and are applied to a page the store
has already cached in full.** A budget is a rendering decision — how much of this is worth
putting in front of the reader — and design.md forbids `sqlake-app` from holding one. Applying
it in the store would also mean the human sharing the session gets the agent's budget.

**D7 — the schema is generated, by `schemars`.** A hand-written JSON Schema is a second
description of the protocol that nothing checks, and the drift is silent in the direction that
matters: it keeps describing a call after the call changes. JSON Schema rather than anything
else because A3's MCP tool definitions want it anyway, which settles design-agent.md §8.3.

**D8 — one session unless a name is given, and a socket with nobody behind it is not a
session.** `--session <name>`, `$SQLAKE_SESSION`, then `default`. Per-project addressing is a
naming convention on top of that rather than a feature, which is the answer to §8.1. A socket
file outlives the process that made it, so a client that cannot connect to one removes it and
proceeds as if there were no session — the alternative is a stale file making every command
fail until somebody deletes it by hand.

---

## 4. Tasks

Each is one PR, reviewed before the next starts.

| | Task | Done when |
| --- | --- | --- |
| T1 | Bound every `Action` field a socket can set; add `ExpandNode` | A sort naming a column that is not there is refused rather than retried for as long as the preview lives, and expanding a loaded node twice fetches once |
| T2 | The store's headless story: waiting for a snapshot to settle, with a timeout | Connect → expand → preview runs to completion in a test with no terminal and no polling loop of its own |
| T3 | `sqlake-api`: the wire types and `Value` → JSON | A snapshot and a page serialise, a JSON document survives the trip intact, and no field carries a credential |
| T4 | The protocol and `api schema` | Every request the server answers appears in the generated schema, enforced by a test rather than by review |
| T5 | One-shot mode and the subcommands | `sqlake --mock table preview public.users` prints rows on stdout, and a cut result says how much it left |
| T6 | The socket: server, client, attached mode | `sqlake --session work` answers `connection list` from another shell on the connection it already has open |

---

## 5. Questions A1 has to answer

Left open deliberately; the answers belong in the code that settles them.

1. **What a one-shot command does when resolving a profile blocks.** `Profiles::resolve` can
   put a keyring dialog on a screen nobody is looking at, and an agent's command would hang
   behind it. A timeout, a refusal to prompt at all, or attached mode being the only supported
   way to reach such a profile.
2. **What `api snapshot` means in one-shot mode.** A store that started a moment ago holds
   nothing, so the command is either honest and nearly empty, or it is attached-only.
3. **What the exit status says.** A failed request is still a well-formed response; whether the
   process exits non-zero for one is a choice between "JSON is the whole answer" and "a shell
   script can branch on it".
