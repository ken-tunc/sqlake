# M8 — Query history

Everything this client has run, findable. Architecture lives in [design.md](design.md); this
document holds only what is specific to M8, and is deleted when M8 is finished — the crates it
produces are the record after that.

M7 already writes the rows: `query_history` and its FTS index exist at schema v1, a row is
written when a statement is sent and settled when it ends, and `Library::history` reads the
newest back. What is left is finding one, and doing something with it.

---

## 1. Definition of done

| | |
| --- | --- |
| 1 | Typing in the history finds statements by their text, incrementally, and never on a keystroke that is a syntax error |
| 2 | Every run is there — the failures, the cancellations, and the ones the budget stopped — with what each of them cost |
| 3 | A run can be put back in a buffer to run again |
| 4 | A run can be kept as a template, under a name |
| 5 | An agent searches the same history through the same words |

Against the mock in CI, and against both real drivers by hand.

---

## 2. Scope

**In:** `Library::search`; the history in the snapshot; the history tab; re-running one;
keeping one as a template; `history_search` on the agent surface.

**Out**, with the milestone that picks it up: proxies (M6, deferred deliberately); pruning and
redaction (§5, and deliberately not designed until somebody has searched this); a draft of an
unsent buffer, which design.md §7.2 mentions and is not a *run*.

Not in scope because it is already built: writing the rows (M7 T6); the FTS index and the three
triggers that keep it honest (M7 T1); the grid, its scrolling, its selection and its copy; the
rule that a statement goes into an empty buffer or a tab of its own (M7 T4); the palette that
names a statement to keep it (M7 T8).

---

## 3. Decisions

**D1 — what somebody types is terms, not FTS5 syntax.** The search runs while they type, and
FTS5's own syntax makes `"` and `(` and a bare `*` into errors — so half of a query typed
carefully would be a red pane on the way to a green one. Each word is quoted as an FTS5 string
and joined with `AND`, and the last one gets a `*` so it matches as a prefix while it is still
being typed. The cost is that nobody can write an `OR`, which is a thing nobody typing into a
box while reading it wants.

**D2 — the history is one more grid.** Rows under columns is what the pane already draws, and a
history is rows: when, what, how long, how many, what happened. `sqlake-app` holds
`Vec<HistoryEntry>` and the TUI shapes it, the way `Laid` shapes a definition — which is the
same layering rule, and the reason an agent gets `started_at` as a timestamp while the screen
gets a column of local times.

**D3 — a run goes back into a buffer, not into a scratch file.** design.md §12 says a scratch
file, which predates the SQL tab: there is a buffer now, and the rule for what happens to it —
an empty one takes the statement, anything else gets a tab of its own — is M7's and already
written. §12 gets corrected when this lands.

**D4 — re-running is putting it back, not running it.** The statement lands in a buffer and
stops there. Running it is `r`, the same key it was run with the first time, through the same
estimate and the same budget. A history that ran things directly would be a second way past the
gate, and the one thing it would skip is the confirmation for the query that was expensive
enough to be worth finding again.

**D5 — keeping one is the palette that already names statements.** `Ctrl-s` over a history row
opens the same "save as" the SQL tab uses, with the run's text as the body. Two ways to name a
statement would be two dialogs to keep in step, and the second one would be the one that never
learns about the first's mistakes.

**D6 — the search is the store's, not the screen's.** It goes through an `Action` and comes
back in the snapshot, like everything else that touches the library: the agent surface asks the
same question through the same store, and a search that ran in the TUI would be a second
implementation for `sqlake-api` to disagree with.

---

## 4. Tasks

Each is one PR, reviewed before the next starts.

| | Task | Done when |
| --- | --- | --- |
| T1 | `Library::search` and the history in the snapshot | A search finds a statement by a word in it, a half-typed one never fails, and the store publishes what it found |
| T2 | The history tab: the filter line and the runs in the grid | `Ctrl-h` opens it, typing filters it, and the grid in it scrolls and copies like every other |
| T3 | Putting a run back in a buffer, and keeping one as a template | Both from the row under the cursor, through the machinery M7 built for each |
| T4 | `history_search` on the socket and as an MCP tool | An agent finds a statement it ran an hour ago, with what it cost |

T2 depends on T1; T3 on T2; T4 on T1 alone.

---

## 5. Questions M8 has to answer

Left open deliberately; the answers belong in the code that settles them.

1. **Whether a history is ever pruned.** It grows for ever, a few hundred bytes at a time, and
   `pinned` is a column nothing writes — it was put there for exactly this and can stay unused
   if the answer is "no". A year of heavy use is a few megabytes, so the honest first answer may
   be that the question is not one yet.
2. **What to do about a password in a statement.** `CREATE ROLE … PASSWORD` is recorded like
   everything else, and the file is the owner's alone (M7 D8). Redacting on the way in would
   need this client to understand which statements carry secrets, which is a parser it does not
   have; redacting on the way out would leave the file holding what the screen refuses to show.
3. **Whether the failures want their own filter.** Every run is in one list, and "show me what
   went wrong" is the most likely second question after "show me what I ran". A status filter is
   small; a filter language is not, and one of the two is where that ends.
