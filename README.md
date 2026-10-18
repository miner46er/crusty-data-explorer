<img src="assets/icon.svg" width="64" height="64" alt="">

# crusty-data-explorer

A native, local-first desktop SQL client for SQLite, Postgres, and MySQL, written in Rust with egui. It ships as a single binary, with no accounts, no telemetry, and no cloud dependency. It runs on macOS and Linux.

## Features

- **Connections:** SQLite files, Postgres, and MySQL, optionally over an SSH tunnel. You can paste a connection URL, choose an SSL mode per engine, organize saved connections in folders, and mark connections read-only.
- **Production safety:** dev/test/prod environment badges, a confirmation before running on prod, guards on destructive statements, explicit begin/commit/rollback with a per-connection autocommit toggle, and automatic rollback of idle transactions.
- **Editor:** syntax highlighting, schema-aware completion, templates, find, and formatting.
- **Result grid:** structured row filters, staged spreadsheet-style editing with an SQL preview, a cell editor, row detail, paging, and row counts.
- **Schema navigator:** tables, columns, triggers, and routines, with foreign key jumps, bookmarks, and a session and lock view that can terminate sessions.
- **Saved queries:** folders, pinning, and version history with restore.
- **Import and export:** CSV, JSONL, XLSX, SQL, and clipboard. Full exports are streamed.
- **Also included:** execution plan view, result charts, table data compare, a visual table creator, a search palette (Ctrl/Cmd+K), an execution log, and query history.

## Building

This project needs Rust 1.99 or newer (edition 2024). The default build supports SQLite only. Postgres, MySQL, and SSH are optional Cargo features.

```sh
cargo run --release                       # SQLite only
cargo run --release --all-features        # SQLite, Postgres, MySQL, SSH tunnels
cargo run --release --features postgres   # or pick individual features: postgres, mysql, ssh
```

Do not run `cargo update`. `Cargo.lock` pins `zip` 8.6.0 by hand because the XLSX crates still require the yanked 8.x line, and re-resolving the lockfile fails. See the comment in `Cargo.toml`.

## Data

Connections, tabs, history, saved queries, and settings are stored as JSON in `$XDG_CONFIG_HOME/crusty-data-explorer/` (`~/.config/crusty-data-explorer/` by default). Passwords are kept in memory only and are never written to disk.

## Testing

```sh
cargo test                                # unit and integration tests
cargo test --all-features                 # including Postgres, MySQL, and SSH code paths
cargo test --test frames -- --nocapture   # render UI frames to target/frames/*.png
cargo test --all-features -- --ignored    # live tests against real servers
```

The live tests need running Postgres, MySQL, and SSH servers. They read their connection details from `PGHOST`/`PGPORT`/`PGDATABASE`/`PGUSER`/`PGPASSWORD`, `MYSQLHOST`/`MYSQLPORT`/`MYSQLDATABASE`/`MYSQLUSER`/`MYSQLPASSWORD`, and `SSH_HOST`/`SSH_PORT`/`SSH_USER`/`SSH_PASSWORD`.
