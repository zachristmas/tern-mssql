<div align="center">

# tern-mssql

**A SQL Server client that lives inside [Tern](https://stencil.so/tern).**

Object explorer, T-SQL editor with completion, and a spreadsheet-style results grid, all drawn as native Tern surfaces. One Rust binary per OS, no runtime to install.

[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![rust 2024](https://img.shields.io/badge/rust-2024_edition-orange.svg)](https://www.rust-lang.org)
[![tern plugin](https://img.shields.io/badge/tern-plugin-6f42c1.svg)](https://stencil.so/tern)
[![sql server](https://img.shields.io/badge/sql_server-TDS_7.3%2B-cc2927.svg)](https://learn.microsoft.com/sql/sql-server)

<img src="docs/explorer-results.png" alt="The object explorer with a table's columns expanded, a T-SQL query, and its results in the grid" width="900">

</div>

Built on [`tern-sdk`](https://crates.io/crates/tern-sdk) for the UI and [`tiberius`](https://github.com/prisma/tiberius) for TDS. The screenshots show a made-up sample database.

## Features

- Object explorer styled like Tern's file explorer: server, databases, tables, views, programmability, and columns with type and key icons. Objects group by schema (press `g` for a flat list). Columns load on expand, and each database loads its own catalog lazily.
- Query tabs with T-SQL highlighting, undo, word editing keys, and scripts split at `GO`. Run the whole script or just the statement at the caret, and cancel at any time.
- Completion from the live catalog: tables after `FROM`/`JOIN`, columns after `alias.`, databases after `USE`, plus schemas, functions and keywords.
- Results grid that works like a spreadsheet. Select cells, rows or columns, then copy as tab-separated text that pastes straight into Excel. Sort, filter, and save the current view as JSON or `INSERT` statements. Multiple result sets, row counts, and SSMS-style messages (`Msg 208, Level 16, State 1, Line 3`).
- Script as `SELECT`, `INSERT`, `UPDATE`, `DELETE` or `CREATE` from any table or view.
- CSV export of a statement's full result, streamed to disk.
- Connection manager to add, edit, test and delete saved connections, as separate fields or a pasted connection string.
- Passwords live in the OS credential store (Windows Credential Manager, macOS Keychain, Secret Service on Linux).
- Open tabs autosave per launch directory and connection, then come back on the next start.
- Read-only by default: a guard refuses statements that look like writes until you press `F8`.
- Split layout with the explorer and editor in separate Tern panes linked over loopback.
- Several environments at once: the explorer keeps an environments box above the tree, so `dev`, `qas` and `prod` can each have their own root, each with its databases, schemas and tables. Pick one and press Enter to connect, `x` to disconnect, `q` for a new query on it; a table opened from an environment goes to that environment's editor pane, and later editor panes stack under the first so the explorer keeps its column. The set of connected environments is remembered per directory and comes back on the next start.

<p align="center"><img src="docs/completion.png" alt="Column completion after a table alias, with each column's type, nullability and key" width="900"></p>

## Install

You need [Tern](https://stencil.so/tern) and a stable Rust toolchain (2024 edition, Rust 1.85 or newer).

```sh
git clone https://github.com/zachristmas/tern-mssql
cd tern-mssql
cargo install --path .
tern plugin link plugin
```

Then open the Tern command palette:

| Command | What it does |
|---|---|
| SQL Server: Open Tab | Opens the client in a new tab |
| SQL Server: Open Beside Current Pane | Opens it in a split to the right |
| SQL Server: New Connection | Opens a blank connection form |
| SQL Server: Manage Connections | Opens the connection manager |

The status line shows `native edit` or `key edit` depending on how the terminal delivers typing.

## Command line

```text
tern-mssql [PROFILE] [FILE.sql...]   query window (connection manager if no PROFILE)
tern-mssql --new                     connection manager on a blank form
tern-mssql --edit PROFILE            connection manager on that profile's form
tern-mssql query PROFILE "SQL"       run one batch and print it as a table
tern-mssql profiles                  list saved profiles
```

## Connections

<p align="center"><img src="docs/connection.png" alt="The connection form: name, server, port, user, a password read from an environment variable, database, and the trust-certificate option" width="900"></p>

Saved connections live in `%APPDATA%\tern-mssql\profiles.json` on Windows and `~/.config/tern-mssql/profiles.json` elsewhere. The file holds no secrets. If you paste a connection string with a password in it, the password is stripped out and stored in the credential store.

For scripts and CI, a profile can name an environment variable to read the password from instead:

```json
[{ "name": "qas", "server": "host", "user": "sa", "password_env": "QAS_DB_PW", "trust_cert": true }]
```

## Keys

### Editor

| Key | Action |
|---|---|
| `F5` | Run the script (split at `GO`) |
| `Ctrl+Enter` | Run the statement at the caret (highlighting does not change what runs; an empty spot runs nothing and says so) |
| `Esc` / `Ctrl+X` | Cancel the running query |
| `Ctrl+Space` | Complete (`Tab` or `Enter` accepts, arrows move) |
| `Ctrl+V` | Paste at the caret (line endings become `\n`) |
| `Ctrl+E` | Export the statement's full result to CSV |
| `Ctrl+N` / `Ctrl+W` | New / close tab |
| `Ctrl+S` / `Ctrl+Shift+S` | Save / Save As |
| `F8` | Toggle write mode |
| `F6` | Switch between explorer and editor |
| `Alt+Left` / `Alt+Right` | Narrow / widen the explorer |
| `Ctrl+Q` | Quit |

### Results grid

| Key | Action |
|---|---|
| Click, `Shift`+click, arrows, `Shift`+arrows | Select and extend |
| Column header / row number | Select the whole column / row |
| `Ctrl+A` | Select all |
| `Ctrl+C` / `Ctrl+Shift+C` | Copy as TSV / copy with headers |
| `s` | Sort by the selected column (again to reverse) |
| `/` or `Ctrl+F` | Filter rows |
| `j` / `i` | Save the current view as JSON / `INSERT` statements |
| `Esc` | Reset sort and filter, then back to the editor |

### Explorer

| Key | Action |
|---|---|
| `F6` | Switch between the environments box and the tree |
| `Enter` on an environment | Connect, or show its tree when it is connected |
| `x` on an environment | Disconnect (its editor panes keep their own connection) |
| `n` / `e` on an environment | New connection / edit it, in a form pane below |
| `q` on an environment | New empty query in that environment's editor pane |
| Arrows | Move through the tree (`Left` jumps to the parent) |
| `Enter` / double-click on a table | Open `SELECT TOP (1000)` in a new tab |
| Double-click on a database | Make it the editor's database |
| `g` | Toggle grouping by schema |

## A note on safety

Read-only mode is a seat belt. It isn't a security boundary. When it matters, connect with a read-only login.

## Platform support

| OS | Status |
|---|---|
| Windows | Built and tested against live servers |
| macOS (arm64) | Builds and passes tests; not yet run against a server |
| Linux | Should build (CI workflow included); not tested by hand |

## Roadmap

In rough order:

- [ ] Query history
- [ ] Execution plans
- [ ] Microsoft Entra sign-in
- [ ] Security explorer (users, roles, permissions)
- [ ] SQL Agent jobs
- [ ] Table designer

## Related

- [tern-azdo](https://github.com/zachristmas/tern-azdo): Azure DevOps pull requests in Tern, built the same way.

## License

[MIT](LICENSE)
