# M1 — Connection management

The milestone that makes sqlake talk to a real database. Architecture lives in
[design.md](design.md); this document holds only what is specific to M1, and is deleted when
M1 is finished — the crates it produces are the record after that.

M0 hardcoded one mock connection in `main.rs`. M1 replaces it with profiles read from a file,
secrets that are never in plaintext, and a PostgreSQL driver behind them.

---

## 1. Definition of done

| | |
| --- | --- |
| 1 | A profile in `connections.toml` opens a real PostgreSQL connection, and its schemas and tables are browsable |
| 2 | No secret is ever in a config file, in the log, or in a `Debug` output |
| 3 | Testing a connection succeeds or fails without leaving a half-open session behind |
| 4 | Several profiles are open at once, and a production profile is visibly marked |
| 5 | `cargo test` still passes with no database and no network; the conformance suite runs against a container when one is available |
| 6 | Still no `if driver == Postgres` anywhere in `sqlake-tui` |

---

## 2. Scope

**In:** `sqlake-config` (files, profiles, settings), `Profile → ResolvedProfile`,
`sqlake-driver-postgres`, connection UI, and the first shared driver conformance suite.

**Out**, with the milestone that picks it up: BigQuery (M2), filter search (M2), tunnels and
proxies (M6), `$EDITOR` and query execution (M4), history (M8). **Editing a profile from
inside the client is out too** — M1 reads the file the user wrote; a settings screen is worth
building only once there is something more interesting than a form to put in it.

---

## 3. Decisions

**D1 — `ResolvedProfile` lives in `sqlake-core`, not in `sqlake-config`.** A driver takes one
to connect, and a driver depends only on `sqlake-core`. Putting it in `sqlake-config` would
make every driver depend on the *file format*, which is a decision about TOML, not about
databases. `sqlake-config` owns `Profile` — the shape on disk — and owns the conversion.

**D2 — a driver is per kind; a connection is per profile.** The registry stays
`DriverKind → Arc<dyn Driver>`, because that is what a driver is: one PostgreSQL driver serves
every PostgreSQL profile there is. What could not express two connections was
`Driver::connect()` taking no parameters and `Action::Connect` naming a *kind* — so the
parameters move into `connect(&ResolvedProfile)` and the action carries a `ProfileId`, as
`action.rs` always said it would. Two connections may even name the same profile: that is a
second window onto one database, not a collision.

**D3 — the PostgreSQL driver implements `children` and `preview` in M1, not M2.** They are the
only way to know a connection actually works: the store calls `children` during `Connect`, so
a driver that answers `Unsupported` cannot even reach `Ready`. M2 is then BigQuery, a second
answer for `Capabilities`, and search over a tree that already fills.

**D4 — testing a connection is the connect path, not a second one.** A separate "test" that
opens a socket and closes it proves the host is reachable, not that this profile works. Test
means connect, ask the session one question, and close.

**D5 — resolution order is keyring, then command, then environment.** Each profile names one
of them; there is no fallback chain at runtime, because a password silently coming from
somewhere else is how the wrong database gets written to.

---

## 4. Tasks

Each is one PR, reviewed before the next starts.

| | Task | Done when |
| --- | --- | --- |
| T1 | `sqlake-config`: file layout, `Settings` and `Profile`, TOML parsing, XDG paths | A fixture file parses; a malformed one names the file, the key and the reason |
| T2 | `ResolvedProfile` in `sqlake-core`; resolution and `zeroize` in `sqlake-config`; masking | A resolved profile's `Debug` and `Display` carry no secret, and a test asserts it |
| T3 | `Driver::connect(&ResolvedProfile)`; `Drivers` keyed by `ProfileId`; `Action::Connect(ProfileId)` | Two mock profiles are connected at once, with separate trees |
| T4 | `sqlake-driver-postgres`: connect, TLS, `Capabilities`, `close`, `RawValue` decoding | Connects to a container; an unknown OID arrives as `Value::Opaque` |
| T5 | PostgreSQL metadata: `children` from `pg_catalog`, `preview` through `QuotedIdent` | Schemas, tables and views appear; a preview pages and sorts |
| T6 | Driver conformance suite in `tests/`, run against the mock and against a container | The same cases pass for both drivers, and skip cleanly with no Docker |
| T7 | TUI and binary: profile list, connect, test, connection tabs, profile colour | `cargo run` connects to a profile the user wrote, and `--mock` is what selects the mock |

---

## 5. Questions M1 has to answer

Left open deliberately; the answers belong in the code that settles them.

1. ~~**What a connection tab is.**~~ Answered by building it: there is no second bar. The
   explorer holds every connection as a row with its objects underneath, because the tree
   already has indentation, selection, scrolling and hit targets — and at 60 columns a second
   bar costs a row that the grid needs more.
2. **Where the page size lives.** A global setting, or per profile? BigQuery will want a
   different answer from PostgreSQL, which is an argument for the profile.
3. **What happens when a connection drops.** Reconnecting silently is friendly right up until
   it reconnects to production; M1 has to at least decide whether the status goes back to
   `Connecting` or to `Failed`.
