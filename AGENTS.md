# AGENTS.md

Guidance for coding agents working in this repository. `PRODUCT.md` (scope, users, principles) and `DESIGN.md` (palette, typography, visual rules) are the source of truth for product and design decisions; read them before UI or scope work.

## What this is

A native desktop SQL client written in Rust with eframe/egui 0.36. It supports SQLite, Postgres, and MySQL, with optional SSH tunnels. It is a single binary, local-first, with no accounts or telemetry, and it targets macOS and Linux (Windows support is undecided). It has one user, the operator, who works keyboard-first against real databases, some of them production-adjacent. Production safety (read-only connections, environment badges with a prod confirm, guarded destructive statements, transaction idle rollback) is a core feature, so test it like any other feature.

## Commands

Engines are feature-gated. The default build is SQLite-only.

```sh
cargo run                                   # SQLite-only build
cargo run --all-features                    # + postgres, mysql, ssh
cargo check && cargo check --all-features   # both must be clean
cargo test                                  # plain suite
cargo test --features postgres              # also: --features mysql, --all-features
cargo test --all-features <name_substring>  # single test
cargo test --test frames -- --nocapture     # rendered-frame captures -> target/frames/*.png
cargo test --all-features -- --ignored      # live suite against the probe containers
```

The gate for each unit of work: `cargo check` plain and with `--all-features`, `cargo test` (plain and with features), and, when an engine is involved, the ignored live suite. "Compiles clean" does not count as verified.

**Live tests** are `#[ignore]` and need the probe containers `cde-postgres`, `cde-mysql` (port 3307), and `cde-ssh`. They read `PGHOST/PGPORT/PGDATABASE/PGUSER/PGPASSWORD`, `MYSQLHOST/MYSQLPORT/MYSQLDATABASE/MYSQLUSER/MYSQLPASSWORD`, and `SSH_HOST/SSH_PORT/SSH_USER/SSH_PASSWORD`. In the tunnel tests, `PGHOST/PGPORT` is the address the jump host dials, not the address the local machine dials.

**Frame captures** (`tests/frames.rs`) use `egui_kittest` to render real frames to PNG so a person or agent can review them. They are not golden-image tests: each one asserts on text nodes and sampled pixels instead. `CDE_FRAME_SIZE=WxH` changes the render size (default 1440x900). Use them to check UI changes visually.

**Do not run `cargo update`.** `Cargo.lock` hand-pins `zip` 8.6.0 because calamine and rust_xlsxwriter still require the yanked 8.x line. Re-resolving the lockfile fails. See the comment in `Cargo.toml`.

## Architecture

`src/lib.rs` exposes all modules as the `crusty_data_explorer` library, which the integration tests use. `src/main.rs` only calls `ui::install` and builds `AppState`.

- **`app.rs`: the single state owner.** `AppState` holds connections, tabs, history, saved queries, bookmarks, the pending-confirm modals, and so on. All its fields are private. Every mutation goes through `AppState` methods (transitions), and the UI reads through accessors. Worker results come back as `DbEvent`s on an mpsc channel, and one `match` folds them into state (search for `DbEvent::Connected`). Persistence is JSON under `$XDG_CONFIG_HOME/crusty-data-explorer/` (`connections.json`, `tabs.json`, `history.json`, `saved_queries.json`, `settings.json`, and others). Tests that touch persistence redirect `XDG_CONFIG_HOME` to a scratch dir and serialize on an `ENV_LOCK` mutex. Follow that pattern.
- **`db.rs`: engines and workers.** `spawn_session` starts one OS thread per connection (`worker_loop`). That thread owns a `Session` enum (`Sqlite`/`Postgres`/`Mysql`; the latter two are `#[cfg(feature)]`), so execution is ordered per connection and needs no locks. The UI sends `DbCommand`s and gets `DbEvent`s back. Each `emit` also requests a repaint on the egui context. Postgres and MySQL use sqlx on a per-worker tokio runtime. This file also holds the domain types (`ConnectionConfig`, `Value`, `QueryResult`, `SchemaInfo`/`TableInfo` and friends), statement classification (`is_write_statement`, `unqualified_write`, used by the guards), autocommit/manual transaction handling with `TxIdle` warn/rollback, paging (`paged_sql`, `QueryPage`), and `test_handle()` for driving `AppState` without a real worker.
- **`ui/`: rendering only.** `ui::root` lays out the frame. Each submodule (grid, query editor, schema navigator, connections sidebar, compare, charts, plan, ddl, table_edit, search palette, complete) reads state through accessors and requests changes through `AppState` transitions. Keep that boundary: no business logic or direct state mutation in `ui/`. `ui/mod.rs` defines `Theme`/`Palette`. Colors must match `DESIGN.md`.
- **Pure helpers:** `sql.rs` (statement splitting, params, formatting), `edit.rs` (staged grid edits into PK-aware UPDATE/INSERT/DELETE), `export.rs`/`import.rs` (CSV, JSONL, XLSX, SQL, clipboard), `ssh.rs` (ssh2 tunnel behind the `ssh` feature).

Unit tests live in `#[cfg(test)] mod tests` blocks inside each source file. `tests/` holds integration tests (`sqlite_roundtrip.rs`, `glyph_set.rs`, `frames.rs`).

## Conventions

- Commit messages and pull request titles use Conventional Commits: `type(scope): subject`, lowercase subject, no trailing period. Types are `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `perf`. Example: `fix(ui): keep the small-window chrome inside the frame`.
- Pull requests open ready, never as drafts, with `gh` (there is no `origin` CLI installed). The body follows Why / What changed / Scope / Tradeoffs / Blast Radius / Verification and embeds screenshots or recordings whenever the change has a visual surface. GitHub cannot render files under the gitignored `target/`, so push captures on a throwaway `captures/<branch>` branch and embed `https://raw.githubusercontent.com/<owner>/<repo>/<branch>/<path>`.
- Work is planned in waves of T-numbered units in `todo.md`. Each unit lists its owned files, tests, and evidence, and lands only after it is verified. `.audit/` holds findings and critique backlogs.
- Scope changes (a new engine, or anything on PRODUCT.md's out-of-scope list such as ERD, plugins, or vim mode) need an explicit operator decision. Do not add them on your own initiative.
- App shortcuts bind to Cmd on macOS and Ctrl elsewhere. Keep both platforms working.
- `tools/design-sidecar/build_sidecar.py` regenerates `.impeccable/design.json` from its own color table and `ramps.json`. Run it from the repo root. It does not read `DESIGN.md`, so when the palette changes, update the script's table along with `DESIGN.md`.
