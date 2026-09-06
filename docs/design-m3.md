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
| 2 | Clicking a column header sorts by it, and which column and direction is visible without counting |
| 3 | A cell whose value the grid had to shorten can be read in full, including a nested document |
| 4 | A range of cells can be selected, and copied as CSV or JSON — over SSH, where the clipboard is not ours |
| 5 | Right-click offers what the selection allows, and **every entry has a key binding**, enforced by the coverage test |
| 6 | The mouse-capture hint is on screen, so somebody who wants the terminal's own selection knows how |

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
`Toast` exist. M3 is the front-end over all of it.

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

**D2 — the detail pane is a pane, not a popover.** The layout in design.md §7 already has it as
the third pane, and a popover over the grid hides the rows around the cell being read — which
is exactly the context that makes a value make sense. It is also where a nested `Value::Struct`
or `Value::Array` is rendered as a document, which needs height rather than a hover's worth of
space.

**D3 — copy is OSC 52, and never a clipboard crate.** The terminal may be at the other end of
an SSH connection, and a clipboard crate would put the text on the clipboard of the machine the
database is near rather than the one the person is at. OSC 52 goes through the terminal, which
is the thing that knows where the person is. Its size limit is real and is handled by saying so
(§4, T5) rather than by silently truncating.

**D4 — a selection is a rectangle, not a set.** Anything else needs a model of what a
discontiguous selection means when it is copied to CSV, and there is no good answer. A
rectangle has one: the rows it covers, each cut to the columns it covers.

**D5 — the context menu is data, like the keymap.** An entry is a label, an `IntentKind` and
whether the current selection allows it. Built that way, the menu cannot offer an action the
keyboard has no route to — which is the invariant the coverage test enforces, and the reason
`Target::MenuItem` has to exist before the menu does.

**D6 — what is copied is the value, not the cell.** The grid shortens, escapes control
characters and writes `∅` for null, and every one of those is wrong in a paste. This is the same
split as `RenderedGrid` against `sqlake-api`'s JSON, one level down: `PagedResult::value` is the
source for copying, and the CSV and JSON writers are the front-end's rendering of it — which
means the JSON one is `sqlake_api::to_json` rather than a second answer to the same question.

---

## 4. Tasks

Each is one PR, reviewed before the next starts.

| | Task | Done when |
| --- | --- | --- |
| T1 | Paging on scroll, with the view tracking what it has asked for | Scrolling to the end of `public.big` keeps loading, and a page in flight is not asked for twice |
| T2 | Sort affordance: header click, direction indicator, sorted-column marker | Clicking a header sorts; clicking it again reverses; which column and way is visible without counting columns |
| T3 | The cell-detail pane, with nested values as a document | A 400-character text cell and a `Value::Struct` are both readable in full; the splitter moves it |
| T4 | Range selection, by drag and by keyboard | Shift-arrow and drag both extend; the status bar says how much is selected |
| T5 | OSC 52 copy — cell, row, selection, whole result — as CSV and as JSON | Copying works over SSH; a payload past the terminal's limit says so rather than arriving cut |
| T6 | The context menu, `Target::MenuItem`, and the capture hint | Every entry has a key binding, enforced by the coverage test rather than by inspection |

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
3. **How much of a large result copy is allowed to cost.** `public.big` is 200,000 rows, and
   "copy the whole result" on it builds a string that is worth being deliberate about.
