# M2 — Table list

The milestone that gives `Capabilities` a second answer. Architecture lives in
[design.md](design.md); this document holds only what is specific to M2, and is deleted when
M2 is finished — the crates it produces are the record after that.

M1 shipped one real driver, so every capability the UI reads had exactly one value and the
rule "express driver differences as `Capabilities`, never as UI branches" was untestable in
principle. BigQuery is what tests it.

---

## 1. Definition of done

| | |
| --- | --- |
| 1 | A BigQuery profile connects, and its projects, datasets and tables are browsable |
| 2 | Previewing a BigQuery table issues no query, and nothing in the client can make it |
| 3 | Filter search narrows the explorer to matching nodes, across every open connection |
| 4 | `sqlake-tui` still has no `if driver == …`, now that there is a second answer to disagree with |
| 5 | `cargo test` still passes with no network; the BigQuery conformance leg runs when an emulator is available |

---

## 2. Scope

**In:** `sqlake-driver-bigquery`, the capability that says whether a preview can be sorted,
filter search in the explorer, and the BigQuery leg of the conformance suite.

**Out**, with the milestone that picks it up: the *interactive* preview — paging controls,
cell detail, copying (M3); query execution and the byte budget (M4); table definitions (M5).
`Session::preview` already exists and already pages, which is what M2 needs from it.

Not in scope because M1 already built it: BigQuery profiles parse, validate and resolve
today — `BigQueryAuth` is ADC or a service-account file, and `sqlake-config` refuses a
relative path or a `~` in the latter. M2 consumes that; it does not revisit it.

---

## 3. Decisions

**D1 — whether a preview can be sorted is a capability.** `Session::preview` takes a
`PageRequest` carrying `Option<Sort>`, and the conformance suite asserts that descending is
ascending reversed. BigQuery cannot honour both that and `free_preview`: `tabledata.list` is
not billed and cannot sort, and sorting means `SELECT … ORDER BY`, which is billed and — by
design.md §4.2 — needs an `ApprovedQuery` that does not exist until M4. So the pair
`(free_preview, sortable_preview)` is what a driver answers, the TUI stops offering a sort it
cannot deliver, and the suite runs the reversal case only where it is claimed.

The alternative was to let BigQuery quietly fall back to a billed query when a sort arrives.
That turns a click on a column header into a scan of the whole table — the exact accident
design.md §1 says to prevent structurally rather than by convention. Sorting the page already
fetched was the other alternative, and it is worse than either: it looks like the feature and
answers a different question, silently, for the one driver where the difference is money.

**D2 — filter search matches what has been loaded, and does not expand to find more.** The
tree is lazy, so a search that reached unloaded nodes would fetch on the way. On BigQuery that
is a network round trip per dataset, per keystroke, and a bill attached to a text box. What
the user gets instead is a filter over the rows the explorer already holds, which is a
different feature from "find this table anywhere" — that one belongs with a catalogue search
the driver can answer in one call, and it is not this.

**D3 — the mock keeps advertising a third shape.** Its `Capabilities` is already
configurable per test (`with_capabilities`, `DEEP_HIERARCHY`), which is what has been
standing in for a second driver. That stays: BigQuery is one more real answer, not a
replacement for being able to construct an unreasonable one on demand.

---

## 4. Tasks

Each is one PR, reviewed before the next starts.

| | Task | Done when |
| --- | --- | --- |
| T1 | `sortable_preview` on `Capabilities`; gate the suite's reversal case and the TUI's sort on it | The mock can advertise a preview that does not sort, and clicking its header does nothing |
| T2 | `sqlake-driver-bigquery`: connect, auth, `Capabilities`, `close` | A profile with ADC connects, and the wrong credentials fail with the reason |
| T3 | BigQuery metadata: `children` for project, dataset and table | The tree fills to three levels, with datasets named as datasets |
| T4 | BigQuery preview via `tabledata.list`, and type decoding | A table pages without a query being issued; `RECORD`/`REPEATED` arrive as something the grid can draw |
| T5 | The BigQuery leg of the conformance suite | The same cases pass for three drivers, and skip cleanly with no emulator |
| T6 | Filter search in the explorer | Typing narrows the tree across every open connection, and clearing it restores the tree unchanged |

---

## 5. Questions M2 has to answer

Left open deliberately; the answers belong in the code that settles them.

1. **Whether an emulator is worth it.** The BigQuery emulators are partial, and a conformance
   leg that passes against one proves less than the PostgreSQL leg does against a real server.
   The alternative is a recorded-response fixture, which proves less again but never lies about
   what it covers.
2. **Where `location` comes from when the profile omits it.** Inferring it from the dataset is
   one round trip; requiring it is one more thing to get wrong in a config file.
3. **What search does to the selection.** The explorer's selection is an index into the
   flattened tree, so filtering renumbers every row underneath it. Keeping the same *node*
   selected across a keystroke is a different thing from keeping the same index.
