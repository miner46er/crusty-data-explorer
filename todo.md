# todo.md — crusty-data-explorer (figure-it-out + feature)

- [x] Read the Principles section of the poteto-mode skill
- [x] Phase A: Frame — definition of done as falsifiable predicate, scope quantified, rigor level biased high, present framing before long run
- [x] Phase B: Design the workflow — decompose into atomic independently-landable units, riskiest-unknown-first, scaffold and verification before features, write phase list down
- [ ] Phase C: Run the loop — each unit is experiment, smallest change, measure on real artifact, VERIFIED / NOT VERIFIED / INCONCLUSIVE, verify by inspecting artifact
- [ ] Phase D: Keep the audit trail — log run via show-me-your-work
- [ ] Phase E: Verify and hand back — check whole against Phase A predicate, encode recurring correction as gate/lint/check/script
- [ ] `how` over the affected subsystem.
- [ ] `architect` for parallel design exploration.
- [ ] Write the throughput checkpoint as four todo items. A dimension that genuinely does not apply (single file, no fan-out) keeps its item with `n/a: <reason>` rather than being dropped:
  - Blocking first steps.
  - Independent workstreams.
  - Shared mutable state.
  - Smallest safe decomposition.
- [ ] Delegate code-writing to a subagent using your configured feature model (default in poteto-mode's Models section) with a specific scope (file paths, named data shape and its organizing structure per principle-model-the-domain, state machine, table/registry, typed model, success criteria). Mandatory review separation. Give every file-writing delegate its own worktree. Commit liberally.
- [ ] Verify on the matching surface. "Inconclusive" or wrong-surface is not a pass. Flag it.
- [ ] Rebase into small, ordered commits. Stack follow-ups. Use sequence-verifiable-units, building, verifying, and committing each small unit before the next.
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
- [ ] T6 EXPLAIN plan view (after T4). EXPLAIN runner, rows rendered in grid. Read-only. Tests + live EXPLAIN on both engines. In flight on deepseek max.
- [ ] T2 Inline editing (after T4, T6). Grid dirty tracking + PK-aware UPDATE/INSERT/DELETE + worker write command. Tests + live write + cancel regression.
- [ ] T7b CSV import (after T2). Column mapper + batched INSERT on T2 write path. Tests + live import.

Wave 3 — serial drivers on config shape, after Wave 2 (shared `src/db.rs`).
- [ ] T9b MySQL session cage mirroring `PgSession` behind feature. Tests: gated roundtrip.
- [ ] T8b SSH tunnel (last, heaviest). Spike prototype first, then real build. Tests: forward + connect.

Per-unit verify: `cargo check`, `cargo check --features postgres`, `cargo test`, `cargo test --features postgres` (live ignored), plus live probe where noted.
