# M7 — SQL templates

The statements somebody types again, kept and parameterised. Architecture lives in
[design.md](design.md); this document holds only what is specific to M7, and is deleted when
M7 is finished — the crates it produces are the record after that.

M7 and M8 share one SQLite file, so the storage layer is designed once, here, with what M8
needs from it written down rather than discovered when M8 re-cuts the schema. §6 is that list.

---

## 1. Definition of done

| | |
| --- | --- |
| 1 | A statement in a SQL tab can be saved as a named template, edited and deleted |
| 2 | `Ctrl-p` opens the palette, and a template picked in it lands in the buffer |
| 3 | `{{param}}` placeholders are filled before the statement is a statement, and `{{table}}` offers what the tree has selected |
| 4 | A template is text a person can read: what runs is what the buffer shows, not something assembled behind it |
| 5 | Every run is recorded — the failures included — so M8 has something to search |
| 6 | Templates are on the agent surface, from the same store the palette reads |

Against the mock in CI, and against both real drivers by hand.

---

## 2. Scope

**In:** the SQLite-backed store and the trait over it; `Template → BoundTemplate → RawSql`;
the palette; parameter entry; recording a run.

**Out**, with the milestone that picks it up: searching history, re-running from it, promoting
a history row to a template (M8); sharing or syncing templates (no milestone — the file is
this machine's); proxies (M6, deferred deliberately, not blocked by anything here).

Not in scope because it is already built: `RawSql` and the gate over it — a template's product
is a `RawSql` and goes through `ValidatedSql` and `ApprovedQuery` like anything typed;
`state_dir()` in `sqlake-config`, whose doc comment already names the SQLite file as one of
the things that belongs there; `Ctrl-p`, reserved in design.md §6 for exactly this; the SQL tab, which is where a template
lands.

One edge of "already built" that is not: `QueryView` carries no timing at all — no start, no
duration, no rows or bytes counted. T6 adds them, because a history row without them is a list
of statements rather than a record of what happened.

---

## 3. Decisions

**D1 — the store is a trait in `sqlake-core` and an implementation in a new crate.** The same
shape as `Profiles` and `Drivers`: `Store::spawn` takes an `Arc<dyn Library>`, the real one is
`rusqlite`, and the tests get an in-memory one. That is what keeps "every test runs with no
database and no network" true — and it is the reason the trait cannot live in `sqlake-app`,
which sits above the crate that would implement it.

The crate is `sqlake-library`. Not `sqlake-store`, because `Store` already means the
application's state store and two of them in one sentence is one too many; not
`sqlake-history`, because it holds templates too.

**D2 — one file, two tables, migrated forward.** `$XDG_STATE_HOME/sqlake/library.db`, mode
0600, WAL. Schema versioning from the first commit rather than when it first hurts: this is the
only file sqlake writes that a person cannot fix in an editor, and a v2 that cannot open a v1
is a personal tool eating somebody's saved work.

**D3 — SQLite blocks, and the caller is what knows that.** `rusqlite` blocks, and blocking
inside the tokio runtime stalls every other connection, so every call goes through
`spawn_blocking` — which is what the store already does with `Profiles::resolve`, and the
reason that trait's doc comment says so out loud.

An actor thread of its own was the first answer and is the wrong one. The session actor exists
to serialise a *stateful* driver session — a cursor, a transaction, a cancel that has to reach
the query it stops. SQLite's state is the file, and a `Mutex<Connection>` serialises that with
no message type per method. What the actor would buy is an async signature, and an async
signature over a blocking file is the lie moved rather than removed.

**D4 — a placeholder is substituted, not bound.** Both servers take parameters, and using them
would be the safer-looking answer. It is the wrong one here: `Session::execute` accepts only an
`ApprovedQuery`, and an `ApprovedQuery` requires an `Estimate` of *the statement that runs*. An
estimate of `WHERE created_at > ?` is not an estimate of the query with the date in it —
BigQuery prices a partitioned scan on the value. Substituting first keeps the gate honest and
keeps the buffer readable: what is on screen is what runs.

Which makes the quoting this milestone's real work rather than an afterthought. A placeholder
is typed — an identifier goes through `QuotedIdent`, a value through the driver's `Escaping`,
both of which exist and are already what `Capabilities` carries. A placeholder whose type
cannot be established is refused at binding time rather than pasted in.

**D5 — the palette lists what exists, and nothing it might.** No fuzzy invention of commands:
it lists templates, filtered as you type. Commands in it are for a later milestone that has
some. Every row is a mouse target, so every row needs a `KEYMAP` entry — the coverage test in
`input.rs` is what says so, and it will refuse the palette until it has one.

**D6 — parameter entry is a stage, and it is visible.** `Template → BoundTemplate → RawSql`,
as design.md §4.1 committed. The intermediate type is not ceremony: it is what stops a template
with an unfilled placeholder reaching the buffer, which would otherwise be a syntax error
reported against text nobody wrote.

design.md §4.1 says M7 puts `PreparedSql` back "with something in it". It does not: D4 removes
the thing it would have carried. `BoundTemplate` is the stage that milestone was reaching for,
and it sits before `RawSql` rather than after `ValidatedSql`. §4.1 gets corrected when this
lands.

**D7 — history is recorded in M7 and searched in M8.** Recording needs no interface, so it can
land with the storage layer that makes it possible; searching is a tab and a query language and
is M8's whole content. Doing it this way means M8's search opens on a file with real history in
it rather than on an empty table, and the recording hook has had a milestone's use before
anything depends on its shape.

**D8 — everything is recorded, and nothing leaves the machine.** Failures included: in a
personal tool the failures are the useful part, and a history that quietly drops them is a
history that lies about what the session did. That means statements with a password in the text
are recorded too — `CREATE ROLE … PASSWORD` is the case — so the file is 0600 under the state
directory, and nothing in this milestone copies it anywhere: not to the socket, not into a
response, not into the log. §6 keeps the question of redaction open; it does not keep the
file's permissions open.

---

## 4. Tasks

Each is one PR, reviewed before the next starts.

| | Task | Done when |
| --- | --- | --- |
| T1 | `Library` in `sqlake-core`, `sqlake-library` over `rusqlite`, the in-memory one for tests, injected into `Store::spawn` | Both tables exist at v1, a v1 file opens on a later build, and the whole suite still runs with no file on disk |
| T2 | `Template → BoundTemplate → RawSql` in `sqlake-core`, with typed placeholders | An unfilled placeholder cannot become a `RawSql`; an identifier is quoted by the driver's rules and a value escaped by them |
| T3 | Saving, editing and deleting a template, through the store | A statement in a SQL tab is saved, comes back after a restart, and the snapshot carries the list |
| T4 | The palette: `Ctrl-p`, filtering, and insertion into the buffer | A template picked in the palette is in the buffer, and `input.rs` has the binding its rows demand |
| T5 | Parameter entry, and `{{table}}` from the selected node | A template with two placeholders asks for both, and the one named `table` offers what the explorer has selected |
| T6 | Recording a run: timings on `QueryView`, a row per run | A run, a failure and a cancellation each leave one row saying which they were |
| T7 | `template_list` and `template_apply` on the socket and as MCP tools | An agent renders a template with its parameters and gets SQL back — not a run |

T2 depends on nothing and can go first if T1 stalls. T4 and T5 both depend on T3. T6 depends
only on T1, and is the one M8 builds on. T7 answers text and never runs it: an agent that wants
the statement executed already has `query_run`, and a template tool that ran things would be a
second, quieter path to the same gate.

---

## 5. Questions M7 has to answer

Left open deliberately; the answers belong in the code that settles them.

1. **Whether a template is bound to a driver.** The schema has `driver TEXT` with NULL meaning
   any. `LIMIT 10` and `LIMIT 10` are the same on both, and `array_agg` is not — so the field
   earns its place — but a palette that hides templates when the wrong tab is focused is also
   a palette that appears to have lost them.
2. ~~**What a placeholder's type looks like in the text.**~~ Answered by T2: `{{name}}` is a
   value and `{{ident:name}}` an identifier, with an unknown prefix refused rather than read as
   part of a name. Inference from the name was the third option and the one that fails
   silently — `{{table}}` in `WHERE table = …` is a value, and no rule about the word can know
   that. The ugliness is real and is paid by the rarer of the two.
3. **What the palette does when the buffer is not empty.** Insert at the cursor, replace, or
   open a tab. Replacing loses work; inserting produces two statements where one was meant.

---

## 6. What M8 needs from the schema

Written now so the file is cut once. M8 adds no columns to what T1 creates.

- `query_history` as design.md §12 lists it — `connection_id`, `driver`, `sql`, `started_at`,
  `duration_ms`, `row_count`, `bytes_processed`, `status` (`ok` | `error` | `cancelled`),
  `error`, `pinned`.
- The FTS5 table is external-content over `query_history`, which means the three triggers that
  keep it in sync are part of the schema and not something M8 remembers to add. An
  external-content FTS table without them returns rows that are not there any more.
- `started_at` is UTC milliseconds, and the column M8 orders by. Local time is a rendering
  decision and belongs where every other one does.
- A run has an id before it has a duration, so T6 writes the row when the run starts and
  updates it when the run settles. M8 searching a table whose rows appear only on completion
  would show nothing for the query still running, which is the one somebody is most likely to
  be looking at.

Two things M8 will want that are deliberately not designed here, because the shape depends on
what searching them is like: retention, and redaction. Both are read questions, and a file
nobody has searched yet is a file nobody knows what they want out of.
