# todo.md — crusty-data-explorer (figure-it-out + feature)

- [x] Read the Principles section of the poteto-mode skill
- [x] Phase A: Frame — definition of done as falsifiable predicate, scope quantified, rigor level biased high, present framing before long run
- [x] Phase B: Design the workflow — decompose into atomic independently-landable units, riskiest-unknown-first, scaffold and verification before features, write phase list down
- [x] Phase C: Run the loop — each unit is experiment, smallest change, measure on real artifact, VERIFIED / NOT VERIFIED / INCONCLUSIVE, verify by inspecting artifact
- [ ] Phase D: Keep the audit trail — log run via show-me-your-work
- [ ] Phase E: Verify and hand back — check whole against Phase A predicate, encode recurring correction as gate/lint/check/script
- [ ] `how` over the affected subsystem.
- [ ] `architect` for parallel design exploration.
- [ ] Write the throughput checkpoint as four todo items. A dimension that genuinely does not apply (single file, no fan-out) keeps its item with `n/a: <reason>` rather than being dropped:
  - Blocking first steps.
  - Independent workstreams.
  - Shared mutable state.
  - Smallest safe decomposition.
- [x] Delegate code-writing to a subagent using your configured feature model (default in poteto-mode's Models section) with a specific scope (file paths, named data shape and its organizing structure per principle-model-the-domain, state machine, table/registry, typed model, success criteria). Mandatory review separation. Give every file-writing delegate its own worktree. Commit liberally.
- [x] Verify on the matching surface. "Inconclusive" or wrong-surface is not a pass. Flag it.
- [x] Rebase into small, ordered commits. Stack follow-ups. Use sequence-verifiable-units, building, verifying, and committing each small unit before the next.
- [ ] If the design is contested, `interrogate` before shipping.
- [ ] Run Opening a PR.

## Task-specific (added after framing, between Phase C and Phase D per figure-it-out)

- [x] Scaffold Rust workspace: Cargo.toml, eframe/egui shell, module map
- [x] Core domain types: ConnectionProfile, QueryResult, AppState machine
- [x] SQLite driver first via rusqlite (bundled), priority 1. Postgres via sqlx + tokio behind feature, priority 2. MySQL deferred.
- [x] UI: connection sidebar + tabbed query editor + results grid, Beekeeper-style theme
- [x] Verify: cargo test + cargo run smoke, SQLite in-memory query roundtrip
- [x] Postgres queries: PgSession connect with pool, fetch rows to Value, affected counts, server_version, feature-gated tests
- [x] Verify: cargo check --features postgres + cargo test --features postgres

## Parity plan (DBeaver / Beekeeper gaps, ranked 2026-10-16)

Wave 0 — scaffold, blocks all.
- [x] T0 Baseline capture: record `cargo check`, `cargo check --features postgres`, `cargo test`, `cargo test --features postgres` outputs at head. No code.

Wave 1 — parallel, additive only, separate owned files.
- [x] T1 Autocomplete: new `src/ui/complete.rs` over `SchemaInfo` + keywords, hook into `src/ui/query.rs` (Ctrl+Space popup). No `Tab` shape change. Tests: suggestion unit tests. Landed 23ee7c7.
- [x] T3 DDL view + Generate SQL: new `src/ui/ddl.rs` pure builder over loaded schema, read-only pane. No state change. Tests: DDL string tests. Landed 4895a07.
- [x] T5 Editor: `src/ui/editor.rs` logic landed ea40967; hook with Format, Ctrl+F find bar, and syntax highlight landed a401b4d.
- [x] T7a Extended export: extend `src/export.rs` with JSON + SQL-inserts + clipboard copy. No `QueryResult` change. Tests: quoting + roundtrip. Landed 77b0771.
- [x] T8a Postgres SSL modes: additive `ssl` field on Postgres config in `src/db.rs` + sqlx connect options + persist. Tests: URL builder per mode. Landed 69b32e0. Live roundtrip green.
- [x] T9a Env types (dev/test/prod badges + prod confirm): landed 6fba7f2 with 8 tests; old profiles load as dev.

Wave 2 — serial core chain on `src/app.rs` Tab machine + `src/db.rs` worker.
- [x] T4 Run scope + multi-statement + resultset picker (after Wave 1). Extends `RunOutcome`, splitter, picker UI. Blocks T6, T2. Landed f859206, live batch tests green.
- [x] T6 EXPLAIN plan view (after T4). EXPLAIN runner, rows rendered in grid. Read-only. Landed with live tests on both engines.
- [x] T2 Inline editing (after T4, T6). Grid dirty tracking + PK-aware UPDATE/INSERT/DELETE + worker write command. Landed 153be64, live write tests green. Edge fix for qualified names + typed keys landed with the same wave.
- [x] T7b CSV import (after T2). Column mapper + batched INSERT on T2 write path. Landed feec93d, modal shortcut gate fixed in query.rs.

Wave 3 — serial drivers on config shape, after Wave 2 (shared `src/db.rs`).
- [x] T9b MySQL session cage mirroring `PgSession` behind feature. Landed with live roundtrip, schema, and cancel tests against cde-mysql on port 3307.
- [x] T8b SSH tunnel (last, heaviest). Spike prototype first, then real build. Landed 187d097; live tunnel proof green.

Plan status: all waves landed and verified 2026-10-16. Final sweep passes: `--all-features` check clean, full test matrix green, live suite 13 of 13 across SQLite, Postgres, MySQL, and the SSH tunnel. The tunnel tests read PGHOST/PGPORT as the address the jump host dials, which matches how DBeaver and Beekeeper configure a tunnel.

Per-unit verify: `cargo check`, `cargo check --features postgres`, `cargo test`, `cargo test --features postgres` (live ignored), plus live probe where noted.

## Next horizon plan (researched 2026-10-16, waves 4 to 7)

Ranked from three delegate reports, current-app seams, Beekeeper Studio v6.1.6 gap, and DBeaver Community 26.2 gap. Friction rankings are the researchers' judgment, sizes are estimates. Execution starts on the operator's go. One unit at a time, the parent verifies and commits between units.

Wave 4: high friction small wins, standalone, any order.
- [x] T10 Grid copy bundle. TSV spreadsheet copy with a headers option, copy as CSV, JSON, Markdown, and SQL INSERT from any selection, Create IN statement from selected values, paste-as-new-rows reusing the CSV importer mapping. Owns `src/ui/grid.rs` and `src/export.rs`. Tests: TSV shape, IN literal typing, paste roundtrip. DBeaver copy set ranks second, Beekeeper seventh. Landed 584dc67, all gates green in the parent run.
- [x] T11 Connection URL paste. `parse_connection_url` pure beside the existing URL builders in `src/db.rs`, one paste row at the top of the draft form that fills the config. Owns `src/db.rs` and `src/ui/connections.rs`. Tests: parse matrix per scheme, roundtrip against `postgres_url` and `mysql_url`. Beekeeper ranks third. Landed 11b5aa8, gates green in the parent run.
- [x] T12 Tab and session restore. `PersistedTab { conn, sql }` in `tabs.json` through the existing tmp plus rename writer, loaded in `AppState::new` as fresh tabs, pruned when the connection is gone, saved from the exit hook. Owns `src/app.rs`. Tests: roundtrip, prune, empty-state start. DBeaver ranks fourth, Beekeeper sixth. Landed c65cd7e with five forced one-liners in the ui call sites for the optional conn, gates green in the parent run.
- [x] T13 Light theme and toggle. Split `ui::install` into dark and light palette functions, a theme field on `AppState`, a status-bar toggle, and per-module color constants read from `Visuals` instead of hard-coded. Owns `src/ui/mod.rs`, `src/app.rs`, and the color constants in the ui modules. Tests: palette apply both ways. Both reports rate themes medium. Landed 86b1335 with a persisted settings shape added, gates green in the parent run.

Wave 5: run path and write path, serial on `start_batch` and the worker.
- [x] T14 Read-only connection plus write guard. A `read_only` flag on config with validate and persist, refuse writes at send time, and a confirm modal when an UPDATE or DELETE reaches the runner without a WHERE. Owns `src/db.rs`, `src/app.rs`, and the modal in `src/ui/grid.rs`. Tests: gate matrix per statement kind, WHERE matcher cases. Beekeeper ranks second and ships the WHERE warning only as an open PR, so this compounds our prod confirm. Landed 7ce68bd with the modal in `src/ui/query.rs` and forced struct literal updates, gates green in the parent run.
- [x] T15 Named query parameters. A pure substitution pass in `src/sql.rs` for `:name` and `?` placeholders applied in `start_batch`, a value prompt reusing `to_literal` before the run, raw SQL threaded through so history records the original text. Owns `src/sql.rs`, `src/app.rs`, and the prompt in `src/ui/query.rs`. Tests: substitution, quoting per type, history keeps raw. DBeaver ranks first, Beekeeper eighth. Landed e88a78b, gates green in the parent run.
- [ ] T16 Manual transactions, three sub-units in order.
  - [x] T16a Subtract the dead `DbCommand::Query` arm first. The inventory found it is test-only today. Owns `src/db.rs`. Gates only. Landed 1ffd9bc, 18 lines deleted, counts unchanged.
  - [x] T16b Begin, Commit, and Rollback buttons sending plain statements through the existing batch path, plus an open-transaction indicator on the tab and the connection card. Owns `src/ui/query.rs` and `src/app.rs`. Tests: live Postgres and MySQL commit and rollback roundtrips. Landed cd7376b with flagged edits in `src/db.rs` (MySQL runs transaction control on the text protocol) and `src/ui/connections.rs` (card indicator). Live suite 17 of 17 green in the parent run.
  - [x] T16c Autocommit toggle per connection with worker-owned transaction state pinned to one connection, cancel semantics decided up front (cancel stops the statement and leaves the transaction open), and idle auto-rollback after ten minutes with a warning. Owns `src/db.rs` and `src/app.rs`. Tests: toggle matrix, cancel keeps the transaction, idle rollback fires. Beekeeper ranks first and skips SQLite entirely, so the toggle covers Postgres and MySQL while the buttons work everywhere. Landed 81f0764 after a two-shape spike; shape A pinned connection won, with a second-BEGIN no-op guard for MySQL implicit commit, live suite 23 of 23 green in the parent run. Wave 5 complete.

Wave 6: discovery surface, after wave 5.
- [x] T17 Saved query folders and pinning. A folder field on `SavedQuery` with a serde default so old files load, a folder field in the save prompt, and the sidebar grouped by folder with pinned entries first. Owns `src/app.rs`, `src/ui/query.rs`, and `src/ui/connections.rs`. Tests: old-file load, grouping order, pin precedence. Beekeeper ranks fifth. Landed 5dc148a with a tab-to-saved link so re-saves keep their folder, gates green in the parent run.
- [x] T18 Search palette. A Ctrl+K switcher over connections, saved queries with their folders, and loaded tables and columns, keyboard navigable, Enter opens. Owns new `src/ui/search.rs` and `src/app.rs`. Edge: after T17. Tests: fuzzy match ranking, keyboard flow. Beekeeper ranks fourth, DBeaver sixth. Landed 9061048 with navigator reveal for column jumps and flagged shortcut-gate clauses in the modal files, gates green in the parent run.

Plan status: all units landed and verified 2026-10-16. Final sweep passes: `--all-features` check clean, full test matrix green (278 plain, 283 with postgres, 283 with mysql, 286 with all features), live suite 27 of 27 across SQLite, Postgres, MySQL, and the SSH tunnel. Open follow-ups on record: migrate to `zip` 9 when the xlsx crates accept it, streamed full-results export past the row cap, JSONL through the import modal via one parse-by-extension branch in `begin_import`.
- [x] T19 Schema object coverage. Triggers and routines for Postgres first, copied to MySQL, SQLite triggers from `sqlite_master`, rendered as new navigator sections. Owns the schema arms in `src/db.rs` and `src/ui/schema.rs`. Tests: per-engine live introspection extends. Supports T18 depth. DBeaver navigator coverage is the reference. Landed c69c6ee with live proofs on both engines and forced test-fixture updates in `src/app.rs` and `src/ui/complete.rs` for the widened constructor. Note: `pg_live_schema_caps_routines_and_flags_truncation` flaked once under two concurrent live processes and passed 5 of 6 later runs including the full 15-test suite. Tracked as live-test robustness, scratch schema names are process-shared.

Wave 7: breadth, individually optional after wave 6.
- [x] T20 MySQL SSL modes mirroring the Postgres `ssl_mode` field. Owns `src/db.rs` and `src/ui/connections.rs`. Tests: URL builder per mode. Beekeeper ninth. Landed with measured sqlx wire spellings (five modes, `preferred` default matching today's behavior so old URLs stay byte-identical) and forced struct-literal updates in `src/app.rs`, live mysql suite green in the parent run.
- [x] T21 Row counts on demand in the navigator, estimates for Postgres and MySQL and COUNT for SQLite behind an explicit request. Owns `src/db.rs` and `src/ui/schema.rs`. Tests: live per engine. Landed e2de4c5 with flagged wiring in `src/app.rs` and `src/ui/connections.rs`, live suite green. Follow-up commit serializes the two schema live tests behind one lock after they flaked twice in ten parallel runs and never in isolation, mechanism judged a Postgres catalog invalidation race under concurrent DDL, unproven.
- [x] T22 Import and export breadth, split. T22a JSONL both ways plus multi-result export landed 0d634e5, staging imports through the grid staged-edit path rather than the CSV import modal. T22b XLSX landed 89ce24d via `calamine` and `rust_xlsxwriter` with `zip` 8.6.0 pinned in `Cargo.lock` by hand, checksum `2d04a6b5…` verified against the crates.io sparse index. Caution: every xlsx crate requires the now fully yanked `zip` 8.x line, so `cargo update` and any re-resolution fail until upstream ships `zip` 9 support. Follow-up: migrate to `zip` 9 when calamine or rust_xlsxwriter accept it and drop the hand entry. Also open: streamed full-results export past the row cap (needs a streaming query path in `src/db.rs`), and optionally routing JSONL through the import modal via one parse-by-extension branch in `begin_import`.

Considered and deferred: vim mode (large with low daily friction for our users), plan graph view (large, our table EXPLAIN already leads Beekeeper), virtual keys, filter memory, row coloring, foreign key navigation panel, grouping and calc panels, formatter presets, and search depth over DDL and comments. Out of scope stays ERD, plugins, admin tooling, enterprise auth, AI shell, cloud sync, and backup wizards.

Risks: sqlx sessions pool connections, so T16c must pin one connection for the life of a transaction instead of acquiring per statement, and that decision touches the cancel path. Named parameters risk recording substituted SQL in history unless the raw text is threaded through T15. The units in waves 5 and 6 all touch `src/app.rs`, so they run one at a time with a green gate between.

Per-unit verify: `cargo check`, `cargo check --all-features`, `cargo test`, `cargo test --features postgres`, plus `--features mysql` and `--features ssh` where a unit touches them, and live ignored probes against cde-postgres, cde-mysql, and cde-ssh where an engine or the tunnel is involved.

## Next features plan (researched 2026-10-16, waves 8 to 10)

Ranked from three fallback-model reports: app inventory, Beekeeper 6.0 to 6.1.6 gap, DBeaver 26.x gap. Friction rankings are the researchers' judgment, sizes are estimates. No new database types. One unit at a time, the parent verifies and commits between units. Backup wizards, ERD, AI chat, cloud sync stay out of scope.

Wave 8: grid depth, serial on `src/ui/grid.rs` and `src/app.rs`.
- [x] T23 Structured row filters. Per-column operators plus a raw SQL WHERE box plus saved filter history, composed into table SELECTs through the existing batch path. Owns `src/ui/grid.rs` and `src/app.rs`. Tests: clause builder, history roundtrip. Beekeeper first, DBeaver first. Size M. Landed f087d75 with disk persistence in `saved_filters.json` after review, gates and live filter green in the parent run.
- [x] T24 Spreadsheet-grade grid bundle. Type to start editing, Backspace sets NULL, column hide and resize, range fill marking cells edited, keyboard paste. Owns `src/ui/grid.rs`. Tests: staged-edit cases per primitive. Beekeeper second. Size M, after T23. Landed 7aeb80a with the four new nested ifs collapsed in the parent pass, gates green in the parent run.
- [x] T25 Dedicated cell editor. Modal editor with explicit set NULL and set default plus large and blob value handling. Owns `src/ui/grid.rs`. Tests: NULL and default actions, large value roundtrip. DBeaver fourth, Beekeeper third. Size M, after T24. Landed with a flagged `STAGED_DEFAULT` sentinel in `src/edit.rs`, one clippy guard fixed in the parent pass, gates green in the parent run.
- [x] T26 Pending edit SQL preview. Render the already built staged statements in a confirm-shaped modal before apply. Owns `src/ui/query.rs` and `src/app.rs`. Tests: preview matches applied statements. DBeaver fifth. Size S, after T25. Landed 65e8e54 with no out-of-file edits, gates green in the parent run.
- [x] T27 Multi-table SELECT editing. Widen the single-table resolver so joined results map columns back to source tables, reusing staged apply and refresh. Owns `src/edit.rs` and `src/app.rs`. Tests: resolver cases, live join edit. Beekeeper fifth. Size M, after T26. Landed 3fc3ea7 with no out-of-file edits, live PG apply plus live MySQL mapping green in the parent run.
- [x] T28 Row detail view. JSON viewer plus transposed record form plus large text viewer beside the grid over the same result. Owns `src/ui/grid.rs`. Tests: rendering cases. Beekeeper third, DBeaver sixth. Size M, after T27. Landed 26e31d9 with no out-of-file edits and no new clippy warnings, gates green in the parent run. Wave 8 complete.

Wave 9: discovery surface, serial where files overlap.
- [x] T29 Foreign key jump navigation. Follow references from grid cells and schema links through the navigator reveal path. Owns `src/ui/schema.rs` and `src/app.rs`. Tests: reveal cases. DBeaver third. Size S. Landed c761dcc with flagged one-liners in `src/ui/connections.rs` and `src/ui/grid.rs`, live jump green in the parent run.
- [x] T30 Navigator bookmarks and object filters. Bookmark set beside saved groups plus tree filter text. Owns `src/app.rs` and `src/ui/connections.rs`. Tests: bookmark roundtrip, filter cases. DBeaver eleventh. Size S, after T29. Landed 264736a with the flagged schema row hook, gates green in the parent run.
- [x] T31 Nested folders with drag and drop. Folder paths for saved queries and connections, folder context creation. Owns `src/app.rs` and `src/ui/connections.rs`. Tests: path grouping, move cases. Beekeeper eighth. Size M, after T30. Landed 8793a37 with no out-of-file edits, gates green in the parent run.
- [x] T32 Execution log panel with per-connection scope. Query manager view over HistoryEntry with timing, plus the existing per-connection filter at the call site. Owns `src/ui/connections.rs` and `src/app.rs`. Tests: scope toggle, stats rendering. Beekeeper sixth, DBeaver tenth. Size S, after T31. Landed c57b830 with no out-of-file edits, gates green in the parent run.
- [x] T33 Saved-query version history with restore. Version the saved-query persist file on each save. Owns `src/app.rs`. Tests: version list, restore roundtrip. Beekeeper seventh. Size S, independent inside wave 9. Landed b7e6321 with the version list UI in `src/ui/connections.rs` after a wiring follow-up, gates green in the parent run.
- [x] T34 Import breadth. SQL file and directory import into saved-query folders, pasted connection JSON beside the URL parser, and JSONL routed through the import modal by extension. Owns `src/ui/connections.rs`, `src/app.rs`, and the modal in `src/ui/grid.rs`. Tests: loader cases, parser cases. Beekeeper eleventh and twelfth. Size S, closes the JSONL modal follow-up. Landed 724b8c6 with no out-of-file edits plus a one-line doc fix in `src/import.rs`, gates green in the parent run.
- [x] T35 Session and lock viewer for Postgres and MySQL. Read-only introspection behind feature flags with terminate actions. Owns `src/db.rs` and `src/ui/schema.rs`. Tests: live per engine. DBeaver twelfth. Size M. Landed fcd621d with flagged worker and sidebar wiring in `src/app.rs` and `src/ui/connections.rs`, full live suite 33 of 33 in the parent run. Wave 9 complete.

Wave 10: breadth, individually optional after wave 9.
- [x] T36 Streamed full export with paged fetching past the row cap. Streaming worker arm under the export writers plus on-demand total count. Owns `src/db.rs`, `src/export.rs`, and `src/ui/grid.rs`. Tests: chunked output, live paging. Beekeeper ninth, DBeaver second. Size L, closes the streamed-export follow-up. Landed 58b503c with flagged event arms in `src/app.rs`, both live streaming tests green in the parent run.
- [ ] T37 Visual table creator and structure editor. Form model over the pure DDL builders with Copy to SQL before apply, sent through the batch path. Owns new `src/ui/table_edit.rs`, `src/ui/ddl.rs`, and `src/app.rs`. Tests: builder cases, live create and alter. Beekeeper fourth. Size L.
- [ ] T38 Graphical execution plan. Costed plan nodes with heavy-path emphasis over the existing explain batch. Owns new `src/ui/plan.rs` and `src/app.rs`. Tests: layout cases, live per engine. DBeaver eighth. Size M.
- [ ] T39 Result charts. Read-only plots over QueryResult. Owns new `src/ui/charts.rs`. Tests: series mapping cases. DBeaver seventh. Size M.
- [ ] T40 SQL templates with star expansion plus editor polish. Template insertion, SELECT star expands to columns, active-line highlight, completion toggles. Owns `src/ui/complete.rs` and `src/ui/editor.rs`. Tests: expansion cases, template cases. DBeaver ninth, Beekeeper thirteenth. Size S.
- [ ] T41 Table data compare. Two read-only batches diffed in a new view for migration checks. Owns new `src/ui/compare.rs` and `src/app.rs`. Tests: diff cases, live per engine. DBeaver thirteenth. Size L.

Plan status: research complete 2026-10-16 on the fallback model after round 1 rate limits. Carried-over maintenance: migrate to `zip` 9 when the xlsx crates accept it.
