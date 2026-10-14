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
