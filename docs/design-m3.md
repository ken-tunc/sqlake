# M3 — Table preview

The milestone that makes the grid a thing you read rather than a thing that draws. Architecture
lives in [design.md](design.md); this document holds only what is specific to M3, and is
deleted when M3 is finished — the crates it produces are the record after that.

Everything M3 ships is front-end. `Session::preview` pages and sorts today, `PagedResult`
carries the values, and A1 already reads both through a second front-end. Nothing here needs a
new use case, and if it turns out to, that is a report that the agent surface is missing the
same thing.

---

## 1. Definition of done

| | |
| --- | --- |
| 1 | Reaching the end of a preview fetches the next page, without a control to press |
| 2 | Clicking a column header sorts by it, and which column and direction is visible without counting — on a connection that cannot sort a preview, the header says so rather than doing nothing |
| 3 | A cell whose value the grid had to shorten can be read in full, including a nested document |
| 4 | A range of cells can be selected, and copied as CSV or JSON — over SSH, where the clipboard is not ours |
| 5 | Right-click offers what the selection allows, and **every entry has a key binding**, enforced by the coverage test |
| 6 | The mouse-capture hint is on screen whenever there is room for it, not only while nothing is loading |

Against the mock in CI, and against both real drivers by hand.

---

## 2. Scope

**In:** paging on scroll, sort affordance and indicator, the cell-detail pane, range selection,
OSC 52 copy of a cell, a row, a selection or the whole result as CSV or JSON, and the context
menu — with `Target::MenuItem` making the coverage test able to see it.

**Out**, with the milestone that picks it up: `$EDITOR`, the command palette and the help modal
(M4); the definition tabs (M5); theming (M4). Filter search is M2's and already ships.

Not in scope because it is already built: `Action::LoadMore` and `Action::SortPreview` exist
and the store answers both; `PagedResult::value` gives the detail pane its content; `Modal` and
`Toast` exist; the capture hint is drawn in `chrome.rs`, and all M3 owes it is that a busy row
stops hiding it. M3 is the front-end over all of it.

---

## 3. Decisions

**D1 — paging happens when the view reaches the end, not when a button is pressed.** A "load
more" control is a question the client can answer itself: the only reason to ask is that
fetching costs something, and the page size is already the answer to how much. The row that
triggers it is a margin above the last loaded row rather than the last row itself, so the fetch
overlaps the scrolling instead of stopping it.

The rejected alternative is fetching on every scroll event that gets close. `Action::LoadMore`
is idempotent per `(conn, table)` in the store — a second one while a page is in flight is
dropped — but relying on that would make the client's correctness a fact about the store's
deduplication. The view tracks that it has asked.

Three things that record has to survive, none of which the store announces. Sorting starts the
relation again at page one — `sort_preview` resets `loaded_rows` — so a watermark that is not
reset with it stops paging for as long as the tab lives. A failed page leaves `data` `Ready` and
reports in `PreviewView::last_error`, so a record not cleared there makes one bad page
permanent: the store would serve it on the next ask, and the view never asks. And the end of
the relation has no flag at all — `total_rows` is `None` on BigQuery, and an empty page merely
fails to grow `loaded_rows` — so a short page is what the view has to read as the end, or every
scroll to the bottom of a finished relation is another round trip to the database.

The trigger is a scroll the user made, not a state the view finds itself in. `page_size` is
configurable down to a single row, so a margin wider than a page re-arms itself the moment the
page lands, and `public.big` walks itself to the end with nobody touching the wheel.

**D2 — the detail pane is a pane, not a popover.** The layout in design.md §5 already has it as
the third pane, and a popover over the grid hides the rows around the cell being read — which
is exactly the context that makes a value make sense. It is also where a nested `Value::Struct`
or `Value::Array` is rendered as a document, which needs height rather than a hover's worth of
space. Drawing code still never sees the `Value`: the pane is handed a rendered document built
beside `RenderedGrid`, for the same reason — wrapping, control characters and how deep to indent
are all terminal decisions, and the agent surface wants none of them.

**D3 — copy is OSC 52, and never a clipboard crate.** The terminal may be at the other end of
an SSH connection, and a clipboard crate would put the text on the clipboard of the machine the
database is near rather than the one the person is at. OSC 52 goes through the terminal, which
is the thing that knows where the person is. Its size limit is real and is handled by saying so
(§4, T5) rather than by silently truncating. What it cannot do is confirm: there is no reply,
and a terminal with clipboard writes off — tmux without `set-clipboard on` is the common one —
swallows the sequence, so what is reported after a copy is what was sent, never that it landed.
The sequence goes out through the terminal writer the TUI already holds; nothing about copying
justifies a second thing writing to stdout.

**D4 — a selection is a rectangle, not a set.** Anything else needs a model of what a
discontiguous selection means when it is copied to CSV, and there is no good answer. A
rectangle has one: the rows it covers, each cut to the columns it covers. It is indexes into a
result the store is free to replace, so sorting clears it: kept across a sort it names rows
`PagedResult::value` no longer has, and the copy comes out short with nothing said.

**D5 — the context menu is data, like the keymap.** An entry is a label, an `IntentKind` and
whether the current selection allows it. Built that way, the menu cannot offer an action the
keyboard has no route to — which is the invariant the coverage test enforces, and the reason
`Target::MenuItem` has to exist before the menu does. Existing is not enough for it to be
enforced, though: the sweep runs one hand-written sample per `Target` against fixture contexts,
and a `MenuItem` those contexts cannot resolve to an intent passes while proving nothing. The
open menu has to reach `InputContext`, and the fixture has to open one carrying every entry.

**D6 — what is copied is the value, not the cell.** The grid shortens, escapes control
characters and writes `∅` for null, and every one of those is wrong in a paste. This is the same
split as `RenderedGrid` against `sqlake-api`'s JSON, one level down: `PagedResult::value` is the
source for copying, and the CSV and JSON writers are the front-end's rendering of it. The JSON
one should not be a second answer to what `sqlake_api::to_json` already answers — and cannot be
reached by depending on it either: design.md §2 makes the two front-ends peers, neither
depending on the other, which `sqlake-tui` importing `sqlake-api` would end. So T5 chooses:
move that writer down to where both front-ends reach it, or accept two of them.

---

## 4. Tasks

Each is one PR, reviewed before the next starts.

| | Task | Done when |
| --- | --- | --- |
| T1 | Paging on scroll, with the view tracking what it has asked for | Scrolling to the end of `public.big` keeps loading; a page in flight is not asked for twice; a sort, a failed page and the end of the relation each leave the view able to ask again exactly once |
| T2 | Sort affordance: header click, direction indicator, sorted-column marker | Clicking a header sorts; clicking it again reverses; which column and way is visible without counting columns; a connection whose `sortable_preview` is false — BigQuery — says so instead of swallowing the click |
| T3 | The cell-detail pane, with nested values as a document | A text value longer than the grid's `MAX_CELL_CHARS` and a `Value::Struct` are both readable in full, which a pane fed from the grid's cell text cannot be; the splitter moves it |
| T4 | Range selection, by drag and by keyboard | Shift-arrow and drag both extend; the status bar says how much is selected; a sort clears it |
| T5 | OSC 52 copy — cell, row, selection, whole result — as CSV and as JSON | Copying works over SSH; a payload past the terminal's limit says so rather than arriving cut |
| T6 | The context menu, `Target::MenuItem`, and the capture hint a busy row hides | Every entry has a key binding, enforced by a sweep that actually opens the menu rather than by inspection |

T5 depends on T4, and T6 on both — the menu's entries are the operations T5 adds, and their
enablement is the selection T4 gives.

---

## 5. Questions M3 has to answer

Left open deliberately; the answers belong in the code that settles them.

1. **What CSV does with a null.** An empty field and the four characters `NULL` are both wrong
   for somebody: the first is indistinguishable from an empty string, the second from a string
   saying NULL. JSON has no such problem, which is an argument for making JSON the default.
2. **Whether the detail pane follows the selection or is opened.** Following is fewer gestures
   and costs a pane of width on every preview; opening is a mode to remember. Neither is
   obviously right until it has been used against a wide table.
3. **How much of a large result copy is allowed to cost.** "The whole result" is what has been
   paged in, not `public.big`'s 200,000 rows — but paging on scroll is what makes a large number
   of them reachable, and a string built from what one long scroll leaves behind is worth being
   deliberate about.
