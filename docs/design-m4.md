# M4 — Running SQL

The milestone where the client stops being a viewer. Architecture lives in
[design.md](design.md); this document holds only what is specific to M4, and is deleted when M4
is finished — the crates it produces are the record after that.

M3 was entirely front-end: every use case it needed already existed. M4 is the opposite. The
`Session` trait has no `execute` and no `estimate`, `RawSql` is a name in a table and not a
type, and `Modal` can only say something and be dismissed. Most of this milestone is below the
TUI, which is why A2 — query execution over the agent surface — lands immediately after it and
needs no new use case of its own.

---

## 1. Definition of done

| | |
| --- | --- |
| 1 | `e` hands the terminal to `$EDITOR` and takes it back, and an editor that dies badly still leaves a usable screen |
| 2 | A SQL tab runs its buffer and shows the result in the same grid a preview uses, paging and copy included |
| 3 | Nothing reaches the database without an estimate: `Session::execute` accepts only an `ApprovedQuery`, and the only place that constructs one is the approval logic |
| 4 | A query over the configured threshold raises a dialog with a real choice in it, and answering it runs the same query rather than a rebuilt one |
| 5 | Cancel stops the query **at the database**, not just the task waiting for it |
| 6 | A syntax error points at the line it is on, without the UI knowing which database reported it |
| 7 | Several SQL tabs are open at once, each with its own buffer, result and running state |

Against the mock in CI, and against both real drivers by hand.

---

## 2. Scope

**In:** the `$EDITOR` handoff and the scratch files behind it; the SQL pipeline types; `execute`
and `estimate` on `Session`, in all three drivers; the run–estimate–approve path through the
store; cancellation that reaches the server; a `Modal` that can ask rather than only tell; error
positions; SQL tabs alongside preview tabs.

**Out**, with the milestone that picks it up: the command palette and SQL templates (M7 — the
palette exists to insert a template, so building it earlier means building it against nothing);
history (M8); definition tabs (M5); theming and the key map, which `sqlake-config` is still
right to keep unwritten.

Not in scope because it is already built: the grid, paging, sorting, selection and copy all work
on a `PagedResult` and do not care where the rows came from; `BusyItem` and `Action::Cancel`
already exist and already abandon a reply; `TerminalGuard::restore` is already the single
restore path, and already says in its own doc comment that M4 is its third caller.

design.md §6 reserves `Ctrl-p` for the palette "(M4)", which contradicts §14 — the palette is
M7's. Fixed in this document's own PR rather than left to be discovered.

---

## 3. Decisions

**D1 — the editor handoff is a third kind of intent, not an `Action`.** design.md §7.2 calls it
`Action::EditExternally`, and that is wrong in a way worth being explicit about: an `Action` goes
to the store, the store runs on its own task, and handing the terminal over has to happen
between two frames on the thread that owns the terminal. `Intent` gains a variant the render
loop answers itself. `ViewCmd` is not it either — `UiState::apply` has a snapshot and no
terminal, and giving it one puts `Tui` into every view test.

**D2 — the buffer is view state until it is run.** What is being typed is one person's
half-finished sentence: it is not in the snapshot, and an agent reading the same session has no
business seeing it. What crosses into `sqlake-app` is the text of a query somebody asked to run.
This is the same line `UiState::filter` already sits on, and it is what keeps A2 from needing a
buffer concept at all — an agent sends SQL, it does not edit a document.

**D3 — a query is named by an id the caller chose, like a connection.** `PreviewView` is keyed
by `(conn, table)` because a preview *is* its relation. Two runs of the same SQL are two
different things with two different results, so there is no natural key, and the caller needs to
be able to wait for the one it started. `Action::Connect` already carries this shape and already
carries the reason in its doc comment; `QueryId` follows it.

**D4 — "over the threshold" is a branch, not an error.** design.md §4.2 settles this; M4 only
has to not undo it. The consequence for the UI is that `RunQueryOutput::NeedsApproval` carries
the `PreparedSql` back out, and answering the dialog dispatches the *same* prepared statement
with `Approval::Approved`. Rebuilding it from the buffer would run whatever the buffer says now,
which after an `$EDITOR` round trip need not be what the estimate was for.

**D5 — only a byte estimate is compared to a threshold.** The two drivers do not estimate the
same quantity: BigQuery's dry run gives bytes that turn into money, PostgreSQL's `EXPLAIN` gives
planner cost units that do not. `Estimate` is an enum over what was actually measured, and the
threshold in `config.toml` applies to the bytes arm. A cost-unit estimate is shown and never
gates — a number nobody can price is not a number to block on, and a fixed cutoff on it would be
a superstition compiled into the client.

**D6 — cancellation does not travel through the session actor.** The actor serialises access to
the `Box<dyn Session>`, so a cancel sent down the same channel queues behind the query it is
meant to stop, and arrives after it finishes. `execute` therefore takes a cancellation handle
the store keeps, and the driver's own out-of-band route — PostgreSQL's cancel request on a
second socket, BigQuery's `jobs.cancel` — is what it triggers. `Capabilities::cancel` already
exists to say when there is no such route.

**D7 — the error's position is computed by the driver, in lines and columns.** PostgreSQL
reports a one-based character offset into the statement; BigQuery writes `[3:15]` into the
message text. Both are the driver's dialect of the same fact, and converting them where they are
produced is the only way the TUI can mark a line without asking who reported it — the same rule
as `Capabilities`, applied to errors.

**D8 — one statement per run.** `ValidatedSql` already promises "single vs. multiple statements
determined", and M4 spends that promise by refusing the multiple case. PostgreSQL's simple query
protocol will happily run `DROP TABLE x; SELECT 1` as one round trip and report success, and a
client whose result grid can show only the last of them is a client that hides what it did.

**D9 — `Modal` gains choices rather than growing a second dialog type.** Today it is a title, a
body and a dismiss button. A confirmation is the same rectangle with more than one button, and
each button is an `Intent` — which puts the dialog under the same coverage test as everything
else, and means "approve" has a key binding because it could not have been built without one.

---

## 4. Tasks

Each is one PR, reviewed before the next starts.

| | Task | Done when |
| --- | --- | --- |
| T1 | A tab is a preview *or* a SQL buffer | Two SQL tabs and a preview tab coexist; closing one leaves the others' state alone |
| T2 | The `$EDITOR` handoff, scratch files, and the `editor` / `editor_args` settings | Editing in neovim returns to an intact screen; an editor that exits instantly says so; killing the editor rudely still restores the terminal |
| T3 | `RawSql → ValidatedSql → PreparedSql`, `estimate` and `execute` on `Session`, in all three drivers | The conformance suite runs a query and an estimate against mock, PostgreSQL and the BigQuery fixture |
| T4 | `ApprovedQuery`, the `RunQuery` use case, `QueryId`, and the query's place in the snapshot | A query runs and its rows reach the grid; `ApprovedQuery::new` is unreachable from outside the approval module, checked by the fact that nothing else compiles against it |
| T5 | A `Modal` that asks, and the approval round trip through it | A query over the threshold stops, and approving runs the statement that was estimated — not the buffer as it stands |
| T6 | Cancellation that reaches the server | Cancelling a ten-second query returns the connection immediately, and the server agrees it is gone |
| T7 | Error position, and the marker in the buffer view | A syntax error on line 3 marks line 3, for both drivers, with no driver named in `sqlake-tui` |

T4 depends on T3, T5 and T6 and T7 on T4. T1 and T2 are independent of the pipeline and can
land first, which is deliberate: they are the half of the milestone that has no database in it.

---

## 5. Questions M4 has to answer

Left open deliberately; the answers belong in the code that settles them.

1. **What a `SELECT` with no `LIMIT` does.** design.md §4.1 says `PreparedSql` has "implicit
   `LIMIT` applied", which is easy to say and awkward to mean: appending one changes the
   statement the user wrote and the one the error positions refer to, and it is wrong outright
   for a CTE ending in `INSERT … RETURNING`. Fetching lazily instead makes the limit a property
   of the cursor rather than of the text, at the price of holding one open.
2. **Whether a failed run keeps the previous result.** `PreviewView::last_error` already answers
   this for pages — the rows survive, the error sits beside them. A re-run is less obviously the
   same question: the rows on screen are now the answer to a statement that no longer exists.
3. **What a read-only connection does with a write.** Refusing in the client duplicates a check
   the server already makes and gets it wrong for anything it cannot parse; letting it through
   means the guard is `default_transaction_read_only` alone, and the profile's `readonly = true`
   is then a description rather than a defence.
