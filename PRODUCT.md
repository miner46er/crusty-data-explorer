# Product

<!-- impeccable:product-schema 1 -->

## Platform

desktop

## Users

One user: the operator (GitHub: miner46er), a developer who uses this tool daily against real work databases, including production-adjacent systems. The workflow is keyboard-first and expert-paced, with Beekeeper Studio and DBeaver habits as the reference vocabulary; density and speed beat hand-holding. The repository is public, but strangers-cloning-it is not a design target and no onboarding, install story, or docs investment has been requested.

## Product Purpose

A local-first desktop SQL client for the operator's real daily work: connect (SQLite, Postgres, MySQL, optionally over an SSH tunnel), inspect schemas, run and edit queries, move data in and out, compare tables for migration checks, and understand plans, with production safety wired into the places where destructive actions happen. Success means it is the tool the operator actually reaches for on work databases, and its guards make touching production-adjacent systems routine instead of risky.

## Positioning

Two claims a neighboring product could not truthfully copy:

1. **Native single binary, local-first.** Rust and egui, no Electron or JVM. No accounts, no telemetry, no cloud dependency; it works offline against databases the user already has access to. Fast startup and modest memory are part of the claim, not a side effect.
2. **Production safety as a first-class feature.** Read-only connections, environment badges with a prod confirm, guarded destructive statements, and explicit transaction control with idle rollback are designed into the run and write paths, not bolted on.

Feature-parity research against Beekeeper Studio and DBeaver is a ranking checklist, never the mandate.

## Operating Context

- Daily use against real databases; environment tiers are fact, not theory (dev/test/prod badges and prod confirmations exist because of this).
- Engines in play: SQLite files, Postgres, MySQL, plus SSH tunnels for remote reach.
- Evaluation happens on real infrastructure: live probe containers (cde-postgres, cde-mysql, cde-ssh) and per-unit gates of `cargo check` (plain and `--all-features`), `cargo test`, and the ignored live suite against those probes.
- Development runs in planned waves of T-numbered units recorded in `todo.md`; each unit names its files, tests, and evidence, lands one at a time, and is verified before the next. Parity-gap research runs as delegated reports ranked by daily friction.
- Table data compare exists because migration verification is a recurring real job.

## Capabilities and Constraints

Capabilities on hand, grouped: connections (URL paste, per-engine SSL modes, SSH tunnels, saved and foldered connections, environment badges, read-only flag); editor (syntax highlighting, schema completion, templates with star expansion, find, formatting); result grid (structured row filters, spreadsheet-style staged editing with SQL preview, dedicated cell editor, row detail, row counts); schema navigator (tables, columns, triggers, routines, foreign key jumps, bookmarks, filters, sessions and locks with terminate); saved queries (folders, pinning, version history with restore); import and export (CSV, JSONL, XLSX, SQL, clipboard bundles, streamed full export); transactions (begin/commit/rollback buttons, per-connection autocommit toggle); execution plan view; result charts; table data compare; visual table creator; search palette (Ctrl+K); execution log and query history.

Constraints and phase scope:

- Stack: Rust edition 2024, eframe/egui 0.36, rusqlite (bundled), sqlx behind the `postgres` and `mysql` features, ssh2 behind `ssh`. The default build is SQLite-only with engines feature-gated; this layout is the current phase's choice, not a permanent rule.
- The engine set is SQLite, Postgres, and MySQL. The no-new-engine rule held through waves 0 to 10 as a phase decision and is explicitly revisitable by the operator, as is the out-of-scope list (ERD, plugins, admin tooling, enterprise auth, AI shell, cloud sync, backup wizards, vim mode). Nothing there is permanently forbidden; nothing there ships without an explicit operator decision.
- Desktop-only is permanent: a native window application, no web, server, or mobile edition. macOS and Linux are both first-class (operator, 2026-10-17). Windows support is undecided.
- `zip` stays pinned at 8.6.0 in `Cargo.lock` (checksum-verified, hand-entered) because every xlsx crate still requires the yanked 8.x line; `cargo update` and re-resolution fail until calamine or rust_xlsxwriter accept zip 9.
- UI modules under `src/ui/` are rendering-only: they read state through accessors and change it through transitions on the single `AppState` mutation point, with worker events folded into one state machine. Future UI work keeps this boundary.
- Every feature lands with tests plus, where an engine is involved, a live probe against the cde containers. Recorded matrix at wave 10 close: 440 plain / 445 postgres / 445 mysql / 448 all-features tests, live suite 41 of 41.

## Brand Commitments

- Name: crusty-data-explorer. Public repository under miner46er. Icon asset: `assets/icon.svg`, a red rounded square with a white "CD" monogram.
- No voice, tone, or identity guidance beyond the name; the operator has volunteered no binding visual constraints.
- An incumbent visual world exists in code (dark default, light toggle, "Beekeeper-ish" palette). Recording or extending it belongs to DESIGN.md and new-work, not init.

## Evidence on Hand

- Test suites and recorded gate results across waves 0 to 10 in `todo.md`, with live proofs per engine.
- Live probe containers (cde-postgres, cde-mysql, cde-ssh) as the evaluation environment.
- Real usage: the operator's own databases; there is no shipped sample data set.
- Absences future work must not fabricate: the README is a stub, the GitHub repository has no description or users beyond the operator, and there are no testimonials, benchmarks, screenshots, or marketing claims anywhere.

## Product Principles

1. The operator is the user. Optimize one expert's daily workflow; density and keyboard speed outrank onboarding and hand-holding.
2. Production safety is a feature, not a checkbox. Guards live at the exact point of destructive action, and their behavior is tested like any other feature.
3. Native and local-first. The binary is the product: no accounts, no telemetry, no cloud, fast startup, modest footprint.
4. Scope moves by explicit operator decision. Waves rank candidates by daily friction and fit to existing seams; new engines, new database types, and out-of-scope entries enter only when the operator says so.
5. Verified on the real artifact. A unit lands only when its gates and, where applicable, live probes pass against actual engines; "compiles clean" and "inconclusive" are not passes.
