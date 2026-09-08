# M5 — Table definitions

What a relation *is*, next to what is in it. Architecture lives in [design.md](design.md);
this document holds only what is specific to M5, and is deleted when M5 is finished — the
crates it produces are the record after that.

The shape is already decided: design.md §3.1 has `TableDetail` and says why `sections` is a
list of `ResultSet` rather than a field per kind of thing. What M5 has to answer is where each
driver's answers come from, what to do about DDL, and what the pane looks like.

---

## 1. Definition of done

| | |
| --- | --- |
| 1 | A relation's columns, with types, nullability, defaults and comments |
| 2 | Indexes, triggers, constraints and partitioning — each in the same grid the preview uses |
| 3 | A driver fills in only what it has, and the pane shows only what was filled |
| 4 | DDL for both drivers, and never a claim it is what somebody typed |
| 5 | `sqlake table describe` and an MCP tool for it, from the same `TableDetail` |
| 6 | The definition tab lives beside preview and SQL tabs, closes like them, and reuses the grid |

Against the mock in CI, and against both real drivers by hand.

---

## 2. Scope

**In:** `TableDetail` and `Session::describe`; PostgreSQL's catalogue queries; BigQuery's
`tables.get` and `INFORMATION_SCHEMA`; the definition tab; `table_describe` on the agent
surface and as an MCP tool.

**Out**, with the milestone that picks it up: editing anything (there is no milestone for it —
sqlake reads); `pg_dump`-grade DDL (§3, D4); history (M8).

Not in scope because it is already built: the grid renders a `ResultSet` and does not care
where it came from — `datagrid::render_rows` was split out in M4 for exactly this; the tab bar
holds more than one kind of tab; `Capabilities` already has `indexes`, `triggers`,
`constraints` and `partitioning`, which have been claims nothing checked since M0 and become
promises here.

---

## 3. Decisions

**D1 — a section is a `ResultSet`, and the driver names it.** design.md §3.1 settles the shape;
what follows from it is that the *titles* are the driver's too. "Indexes" and "Clustering" are
not the same list under different names, and a fixed set of section titles in the core would
make BigQuery answer an empty "Triggers" table rather than not having one.

**D2 — `Capabilities` says which sections to expect, and the answer says which arrived.** Both,
because they answer different questions: the capability is what lets the tab draw its section
list before the driver has answered, and the answer is what stops it drawing a tab for a
section the relation happens not to have. A view has no partitioning on a driver that
partitions.

**D3 — nullability and defaults come from the catalogue, not from a page.** `preview` builds its
columns from a statement's result and says so: it marks everything nullable because the wire
does not carry the answer. `describe` is the call that knows, and it is the only one that
should be believed — which is why `ColumnDef` is its own type rather than `Column` with more
fields on it.

**D4 — DDL is generated, and labelled as generated.** BigQuery has real DDL in
`INFORMATION_SCHEMA.TABLES.ddl` and PostgreSQL has nothing: `pg_dump` is a subprocess this
client will not spawn, and there is no `SHOW CREATE TABLE`. So PostgreSQL's is reconstructed
from the catalogue.

Reconstruction that is quietly wrong is worse than none — somebody copies it, runs it, and gets
a different table. So it is never presented as what was typed: the pane says it was built from
the catalogue, and the known gaps are listed with it rather than left to be discovered. What it
covers is columns, types, defaults, nullability, primary keys, unique and foreign keys, checks,
and indexes as separate statements. What it does not is storage parameters, collations,
partition bounds, inheritance and anything a rule or an extension added.

**D5 — the tab shows one section at a time.** Stacking five grids in one pane gives each of them
four rows. A section list on the left of the pane and one grid beside it is the same
arrangement the explorer and the preview already have, and it means the grid, its scrolling,
its selection and its copy are the ones that already exist.

**D6 — describing is a read like any other, and is cached like one.** `TableDetail` goes in the
snapshot beside `PreviewView`, keyed by `(ConnId, TableRef)`, because opening the tab twice
should not ask the server twice — and because the agent surface asking for it must not fetch a
second copy while a person has it open.

---

## 4. Tasks

Each is one PR, reviewed before the next starts.

| | Task | Done when |
| --- | --- | --- |
| T1 | `TableDetail`, `Session::describe`, the mock's answer, the conformance case | The suite holds every driver to describing the relation it previews, and to `Capabilities` |
| T2 | PostgreSQL: columns, indexes, triggers, constraints, partitioning | Each section matches `psql \d` against the conformance container |
| T3 | BigQuery: columns, partitioning, clustering, and its own DDL | The fixture answers `tables.get` and `INFORMATION_SCHEMA`, and the driver fills only what it has |
| T4 | The definition tab: section list, grid, and the tab bar | A definition tab opens from the explorer, closes like a preview, and the grid in it scrolls and copies |
| T5 | PostgreSQL's generated DDL, and the label on it | A table with a default, a check and two indexes comes back as SQL that recreates it, said to be generated |
| T6 | `table_describe` on the socket and as an MCP tool | One `TableDetail`, wired through both surfaces from the same answer |

T2 and T3 are independent of each other and both depend on T1. T4 depends on T1 and is worth
more once T2 has something to draw. T6 depends only on T1.

---

## 5. Questions M5 has to answer

Left open deliberately; the answers belong in the code that settles them.

1. **Whether a column's comment is worth a round trip.** PostgreSQL keeps them in
   `pg_description`, which is another join; BigQuery has one per field for free. Cheap on one
   driver and not the other is the shape that usually ends in a capability flag, and a flag for
   something this small may cost more than it saves.
2. **What `describe` does with a routine.** `RelationKind::Routine` exists and a function has no
   columns, so either the sections carry its arguments and body or `describe` refuses it — and
   refusing something the tree offers is a dead end a user finds by clicking.
3. **Whether the DDL belongs in the section list or under the columns.** As a section it is one
   more thing to click for the answer people most often want; under the columns it is a second
   scrolling region in a pane that has one.
