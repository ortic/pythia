# Pythia – Oracle PL/SQL IDE in Rust

A native, fast, cross-platform alternative to Allround Automations' PL/SQL Developer,
written in Rust with `egui` on the `wgpu` renderer.

---

## 1. Vision & Goals

- **Fast**: starts in under a second, stays responsive with large schemas (100k+ objects) and large result sets (millions of rows, virtualized).
- **Native & cross-platform**: Linux, Windows, macOS from one codebase. No JVM, no Electron.
- **Keyboard-first**: everything reachable via shortcuts and a command palette, with familiar PL/SQL Developer keybindings as the default.
- **Safe by default**: clear transaction state, production-connection warnings, confirmation for destructive DDL/DML.
- **Scriptable & extensible**: plugin/scripting hooks for custom tools later on.

### Non-Goals (for now)

- Supporting databases other than Oracle (keep the DB layer abstract enough to add them later).
- A full DBA console (RMAN, Data Guard, ASM management).
- A web or remote UI.

---

## 2. Technology Stack

| Area | Choice | Notes |
|---|---|---|
| Language | Rust (stable, edition 2024) | Cargo workspace |
| UI | `egui` + `eframe` | Immediate mode |
| Renderer | `wgpu` (via `eframe` wgpu backend) | Vulkan / Metal / DX12 / GL fallback |
| Docking | `egui_dock` | Tabbed, dockable panels |
| Tables | `egui_extras::TableBuilder` or a custom virtualized grid | Must handle millions of rows |
| Oracle driver | `oracle` crate (ODPI-C, needs Instant Client) | Look at `sibyl` for async. Thin-mode driver as a long-term option |
| Async runtime | `tokio` | DB work runs off the UI thread, results come back over channels |
| Text buffer | `ropey` | For large files and cheap edits |
| Lexing/parsing | Custom PL/SQL lexer + incremental parser (`logos` for lexing) | Or evaluate a tree-sitter PL/SQL grammar |
| Diffing | `similar` | Code and DDL compare |
| Search | `regex`, `grep-searcher`/`ignore`, `tantivy` (optional) | Local code index |
| Export formats | `csv`, `rust_xlsxwriter`, `calamine` (xlsx read), `serde_json`, `quick-xml` | |
| Config | `serde` + `toml`, `directories` | |
| Secrets | `keyring` | OS keychain for passwords |
| Logging | `tracing` + `tracing-subscriber` | |
| Testing | `cargo test`, `insta` (snapshots), Oracle Free in Docker (`gvenzl/oracle-free`) for integration tests | |

---

## 3. Architecture

```
pythia/
├── crates/
│   ├── pythia-app          # eframe entry point, docking layout, documents, commands, dialogs, code and data search
│   ├── pythia-ui           # reusable widgets: code editor, data grid, icon set
│   ├── pythia-db           # connection mgmt, sessions, query execution, metadata queries
│   ├── pythia-plsql        # lexer, parser, AST, symbol tables, formatter
│   ├── pythia-lsp-core     # completion, hover, go-to-definition, diagnostics (no UI)
│   ├── pythia-debugger     # DBMS_DEBUG_JDWP client, debug session state machine, panel
│   ├── pythia-data         # import/export
│   ├── pythia-compare      # schema/DDL and data compare, sync script generation
│   ├── pythia-monitor      # dashboard, sessions, AWR/ASH, trace analyzer
│   ├── pythia-ai           # AI backends, prompts, data policy
│   └── pythia-config       # settings, profiles, keybindings, themes
```

Integration tests against an Oracle Free container live with the crates
(`crates/*/tests`, and modules' own tests); they skip without `PYTHIA_TEST_DB`.

### Core principles

- [x] UI thread never blocks: every DB call runs in a worker task, the UI gets messages over channels (`DbRequest` / `DbEvent`)
- [ ] Each editor tab can own a **dedicated session** or share one (like PL/SQL Developer's session modes: multi-session, dual-session, single-session) (each window has a session of its own; no sharing)
- [x] Every long-running operation can be cancelled (`OCIBreak` / `Connection::break_execution`)
- [ ] Metadata cache per connection with incremental refresh (based on `LAST_DDL_TIME`)
- [x] Command pattern for all user actions, so we get keybindings, the command palette and macros from one place

---

## 4. Milestones

| Milestone | Scope |
|---|---|
| **M0 – Skeleton** | Workspace, eframe+wgpu window, docking, settings, logging |
| **M1 – Connect & Query** | Connection manager, SQL window, result grid, transactions |
| **M2 – Editor** | Custom code editor, syntax highlighting, basic completion |
| **M3 – Object Browser** | Object tree, view/edit DDL, compile, errors |
| **M4 – Program Units** | Program window, compile with errors, PL/SQL parser and outline |
| **M5 – Data Tools** | Export, import, data search, editable grid |
| **M6 – Debugger** | Breakpoints, stepping, variable inspection |
| **M7 – Compare** | Schema compare, data compare, sync scripts |
| **M8 – Productivity** | Code search, refactoring, formatter, templates, explain plan |
| **M9 – Polish & Release** | Packaging, installers, docs, performance work |

---

## 5. Features

### 5.1 Application Shell (M0)

- [x] Cargo workspace with crates listed above
- [x] `eframe` app with the `wgpu` backend, GL fallback
- [x] Dockable layout (`egui_dock`): object browser, editors, output, results, debugger panels (each a tab of its own: controls and call stack, variables, watches, breakpoints, SQL plan)
- [x] Save/restore layout and open tabs per workspace
- [x] Main menu, toolbar, status bar (connection, session, transaction state, cursor pos, encoding)
- [x] Command palette (Ctrl+Shift+P) with fuzzy search over all commands
- [x] Configurable keybindings; ship a PL/SQL Developer compatible preset
- [x] Light/dark themes, custom theme files (TOML) (no TOML theme files yet)
- [x] Font selection with ligatures off by default, HiDPI scaling (code font from a font file; egui has no ligatures)
- [x] Settings dialog (global, per connection, per project) (global settings; per-connection options live in the connection dialog; no per-project settings)
- [x] Crash handler and panic logging
- [ ] Single-instance mode with file open forwarding (optional)

### 5.2 Connection Management (M1)

- [x] Connection dialog: user/password, TNS alias, EZConnect, full descriptor
- [x] Read `tnsnames.ora` / `sqlnet.ora` (`TNS_ADMIN`, `ORACLE_HOME`) (aliases from `tnsnames.ora` incl. `IFILE`; `sqlnet.ora` is not read)
- [x] Wallet / external authentication (`/@alias`)
- [x] Connect as SYSDBA / SYSOPER / SYSBACKUP
- [x] Proxy user connect (`user[proxy]`)
- [x] Saved connections organized in folders, with colors and tags (e.g. red = production) (no tags; a production flag instead)
- [x] Passwords in the OS keychain (`keyring`), never in plain text
- [x] Production safety flags: read-only mode, confirm on commit, confirm DML/DDL
- [x] Multiple simultaneous connections
- [ ] Session modes: multi-session / dual-session / single-session (multi-session only)
- [x] Connection health check, auto-reconnect prompt (lost sessions are detected on errors and replaced on the next statement; no periodic ping or prompt)
- [x] Set NLS settings, `CURRENT_SCHEMA`, module/action/client info on connect (`CURRENT_SCHEMA` per connection, NLS via the login script, module set to `pythia`; no action/client info)
- [x] Login script per connection (like `login.sql`)
- [x] Detect Instant Client and give a helpful message if it's missing

### 5.3 Code Editor Widget (M2)

Custom widget, because egui's `TextEdit` doesn't scale to large files or IDE features.

- [x] Rope-backed buffer (`ropey`) with undo/redo history (grouped edits)
- [x] Virtualized rendering (only visible lines laid out), smooth scrolling
- [x] Line numbers, gutter with markers (breakpoints, errors, bookmarks, folding)
- [x] Multi-cursor and column/block selection (Alt+drag)
- [x] Code folding (blocks, `BEGIN/END`, `IF`, `LOOP`, `CASE`, comments)
- [x] Bracket and keyword matching (`BEGIN`↔`END`, `IF`↔`END IF`)
- [x] Find/replace in file with regex, whole word, case sensitivity, in-selection
- [x] Go to line, bookmarks (numbered like PL/SQL Developer: Ctrl+Shift+0..9) (Ctrl+G; numbered bookmarks 1..9, no 0)
- [x] Auto-indent and smart tab
- [x] Keyword case conversion (upper/lower/init cap) live or on demand (on demand: Ctrl+U / Ctrl+Shift+U on selection or word, formatter keyword case; no init cap, no live conversion)
- [x] Comment/uncomment selection
- [ ] Minimap (optional)
- [x] Whitespace / EOL visualization (spaces and tabs; no EOL markers, the line ending is in the status bar)
- [x] Encoding support (UTF-8, Windows-1252, ...) and line ending detection
- [ ] Large file mode (>50MB): reduced features, still responsive
- [ ] Split view of the same buffer
- [ ] Clipboard with rich and plain text
- [x] IME / Unicode input support

### 5.4 Syntax Highlighting & Language Intelligence (M2/M4)

- [x] PL/SQL + SQL lexer (`logos`): keywords, identifiers, quoted identifiers, strings incl. `q'[...]'`, numbers, comments, hints `/*+ */`, bind variables, substitution variables `&var`
- [x] Highlighting for SQL*Plus commands (`SET`, `PROMPT`, `@`, `DEFINE`, `/`)
- [x] Incremental re-lexing on edit (only dirty lines)
- [x] Recursive-descent PL/SQL parser producing an AST with error recovery
- [x] Statement splitting (handles `/`, `;`, PL/SQL blocks, `CREATE ... AS` units)
- [x] Code outline / structure view (package specs, bodies, procedures, functions, types, cursors, variables)
- [x] Code completion:
  - [x] Keywords
  - [x] Schema objects (tables, views, packages, synonyms) from metadata cache
  - [x] Columns after `alias.` with alias resolution from `FROM`/`JOIN`
  - [x] Package members after `pkg.`
  - [x] Local variables, parameters, record fields
  - [x] Procedure parameter hints (signature help)
- [x] Hover info: object type, column datatype, parameter list, comments
- [x] Go to definition (Ctrl+click), including into package bodies on the DB
- [x] Find references within the file and across the schema (via `ALL_IDENTIFIERS` / PL/Scope when enabled) (within the file and across a program window's spec and body, Shift+F12; no schema-wide search via PL/Scope)
- [x] Live diagnostics: parse errors, unknown identifiers (optional) (parse errors only; no unknown identifiers)
- [x] Hyperlinks for object names (open in object browser) (Ctrl+hover link, Ctrl+click opens the object's window; the context menu's Show in object browser reveals it)
- [x] Code templates / snippets with placeholders (`sel` → `SELECT * FROM ...`) (built-in snippets with tab stops; no user-defined templates)
- [x] Code formatter (PL/SQL Beautifier equivalent) with configurable rules
- [x] Code assistant settings: delay, case, min chars (no case setting; completions follow the typed case)

### 5.5 SQL Window (M1)

- [x] Execute statement under cursor / selection / whole script (F8 / Ctrl+Enter)
- [x] Multiple result sets in tabs
- [x] Bind variable prompt dialog (remember last values)
- [x] Substitution variable prompt (`&var`, `&&var`)
- [x] Execution timing, rows affected, DBMS_OUTPUT capture
- [x] Cancel running statement
- [x] Auto-commit setting (never on production connections), commit/rollback buttons, pending transaction indicator
- [x] Warn on closing with an open transaction
- [x] Query history per connection (searchable, persisted)
- [x] Fetch modes: first N rows, fetch next page, fetch all
- [x] "Execute to file" for huge results
- [x] Error highlighting: jump to the error position in the statement
- [x] Explain plan for current statement (F5)

### 5.6 Result Grid / Data Grid (M1/M5)

- [x] Virtualized grid for millions of rows (row and column virtualization) (rows only; columns are not virtualized)
- [x] Column resize, reorder, hide, freeze
- [x] Sort and filter client-side; option to push sort/filter into SQL (no push of sort/filter into SQL)
- [x] Data type aware rendering: NULL distinct, dates per NLS format, numbers aligned right
- [x] LOB viewers: CLOB text, BLOB hex/image, XMLType, JSON pretty view (no XML pretty view)
- [x] Single record view (transposed)
- [x] Editable grid (`SELECT ... FOR UPDATE` / ROWID based): insert, update, delete rows
- [x] Cell editors per type (date picker, number, LOB editor dialog) (dialog with calendar for dates and multi-line text; numbers inline)
- [x] Copy as: TSV, CSV, INSERT statements, Markdown, JSON, with headers
- [x] Paste from clipboard into grid (Excel ranges)
- [x] Cell, row, and column selection; aggregates of selection (sum, count, avg, min, max) in the status bar (no whole-column selection)
- [x] Find in grid (quick filter shows matching rows; no find next/previous)
- [x] Highlight changed cells before commit
- [x] Foreign key navigation: jump to parent/child rows

### 5.7 Program Window (M4)

- [x] Open/edit packages, procedures, functions, types, triggers, Java sources
- [x] Spec and body in one window with tabs
- [x] Compile (F8) with error list, click error → jump to line
- [x] Compile with debug info (`PLSQL_OPTIMIZE_LEVEL`, `ALTER ... COMPILE DEBUG`)
- [x] Show compiler warnings (`PLSQL_WARNINGS`) (warnings Oracle reports are shown; no setting to enable `PLSQL_WARNINGS` categories)
- [x] Detect when the DB version changed since it was opened (conflict warning)
- [x] Save to file / load from file (save only; files open in SQL windows)
- [x] Recompile invalid objects (schema-wide, dependency order) (Invalid objects window: dependency order over several sessions, progress, cancel; `UTL_RECOMP` for SYSDBA)

### 5.8 Test Window (M4/M6)

- [x] Generate a test script for a procedure/function (anonymous block with bind variables for params)
- [ ] Variable grid: name, type, value, in/out
- [x] Run, run with debugger
- [x] Show DBMS_OUTPUT and out parameters, ref cursors as result grids (OUT parameters and results printed via DBMS_OUTPUT, collections, records and objects included; ref cursors printed row by row, not as grids)
- [ ] Save/load test scripts (`.tst`), compatible with PL/SQL Developer format if feasible

### 5.9 Command Window (M5)

- [ ] SQL*Plus-like command window
- [x] Support common SQL*Plus commands: `SET`, `SHOW`, `DESC`, `SPOOL`, `PROMPT`, `DEFINE`, `@`, `@@`, `EXEC`, `VAR`, `PRINT`, `CONNECT`, `WHENEVER` (in SQL windows: `@`, `@@`, `EXEC`, `PROMPT`, `DEFINE`, `DESC`, `SET DEFINE`, `WHENEVER SQLERROR`; no `SHOW`, `SPOOL`, `VAR`, `PRINT`, `CONNECT`)
- [x] Run deployment scripts with progress and stop-on-error (in SQL windows; no dedicated command window)
- [ ] Spool output to file

### 5.10 Object Browser / Code Browser (M3)

- [x] Tree: connection → schema → object type → objects
- [x] Object types: tables, views, materialized views, sequences, synonyms, packages, procedures, functions, types, triggers, indexes, constraints, DB links, jobs/scheduler, queues, directories, users, roles, profiles (no constraints or queues folders)
- [x] "My objects" / all schemas / filter by schema
- [x] Filters: name pattern, status (invalid), modified since, custom saved filters (no saved filters)
- [x] Invalid objects highlighted
- [x] Context menu: view, edit, compile, drop, rename, describe, query data, generate DDL, copy name, grant (no rename)
- [x] New objects from the context menu (folders, objects, a table's indexes/constraints/triggers, users, roles, profiles and directories) and File → New Object
- [x] Expand table → columns, indexes, constraints, triggers, FKs, references-by
- [x] Expand package → procedures/functions with parameters (members from the dictionary)
- [x] Dependencies view (uses / used by)
- [x] Drag object name into the editor (with options: name, `SELECT`, column list, call template) (names of objects and columns; templates via Generate)
- [x] Recent objects and favorites
- [x] Quick object finder (Ctrl+Shift+O style fuzzy search)
- [ ] Metadata cache with background refresh

### 5.11 Object Editors (M3/M5)

- [x] View and generate DDL (`DBMS_METADATA`) with options (storage, tablespace, constraints) (no options: always without storage and tablespace clauses)
- [x] Table editor: columns, keys, indexes, checks, comments, partitions → generates `ALTER` script (partitions: add, truncate, drop; no split, merge or conversion to partitioned)
- [x] Table builder: new tables (columns, primary/unique/foreign keys, checks, indexes, identity columns, partitioning, temporary tables, copy of a table) → `CREATE TABLE` script, applied in the window
- [x] Sequence, synonym, view, materialized view, index, database link, user, role editors
- [x] New triggers, procedures, functions, packages and types from a form that writes the first source for a program window
- [x] Grants editor
- [x] Preview generated DDL before applying

### 5.12 Debugger (M6)

- [x] Research: implement `DBMS_DEBUG_JDWP` (JDWP protocol client in Rust, supported going forward) and/or `DBMS_DEBUG` (deprecated, but what PL/SQL Developer uses) – decide on a primary backend
- [x] Privilege check and setup assistant (`DEBUG CONNECT SESSION`, `DEBUG ANY PROCEDURE`, ACL for JDWP): *Debugger Setup…* tries every host end to end, tells ACL, name resolution, refused and dropped connections apart, runs the grants and ACL entry (as the user, a saved DBA connection or a user typed in, in the user's PDB) and adds a local firewall rule with administrator rights (Windows Firewall, ufw, firewalld, macOS); opens on its own when a run can't connect
- [x] Debug session lifecycle: target session + debugger session
- [x] Breakpoints in gutter: line, conditional, hit count, enable/disable
- [x] Breakpoints persisted per object (with the workspace: enabled state, conditions, hit counts; also watches and Break on exceptions)
- [x] Step into / over / out, run to cursor, continue, abort (run to cursor in an anonymous block steps there: the agent sets no breakpoints in blocks)
- [x] Break on exception (all / specific ORA codes) (all exceptions only)
- [x] Call stack panel with navigation
- [x] Variables panel: locals, parameters, package globals (package globals through watches)
- [x] Inspect records, collections, objects (tree view)
- [x] Watches with expressions (variable paths: `rec.field`, `coll(n)`, `pkg.g_var`; no arithmetic)
- [x] Modify variable values while paused (scalars: locals, IN OUT/OUT parameters, fields, elements, package globals; numbers checked against precision and range first, as the agent crashes the session on some; not IN parameters or LOBs)
- [x] Hover over variable shows value while paused
- [x] DBMS_OUTPUT during debugging (shown when the run ends)
- [x] SQL panel: the statements the stopped session ran, by unit and line, with the selected one's plan and the last execution's actual rows (`STATISTICS_LEVEL = ALL` in the debugged session), misestimates marked, Ask AI; closed until a step's statement is off by 10× or more, which is flagged in the gutter and opens it
- [x] Auto-compile with debug info before starting
- [ ] Debug triggers and code called from other sessions (attach mode)

### 5.13 Code Search (M8)

- [x] Search source across schemas (`ALL_SOURCE`) with regex, case, whole word (no whole word option)
- [x] Filter by object type, schema, name pattern
- [x] Also search views (`ALL_VIEWS.TEXT`), triggers, job definitions
- [x] Results grouped by object with line context; click → open at line
- [ ] Optional local index (`tantivy`) for instant search across large schemas
- [ ] Search in local files / project folders
- [ ] Replace across files (local files only, with preview)

### 5.14 Data Search (M5)

- [x] Search a value across all tables/columns in selected schemas (one schema at a time)
- [x] Filter by column data type (strings, numbers, dates), table name patterns (column types follow from the value; no explicit type filter)
- [x] Match modes: exact, contains, regex (`REGEXP_LIKE`), case-insensitive
- [x] Run in parallel with throttling, show progress, cancellable (sequential, not parallel)
- [x] Results: table, column, matching row count; click → open query with filter
- [x] Find columns by name across schemas (one schema at a time)

### 5.15 Data Compare (M7)

- [x] Compare tables between two connections/schemas (same or different DB)
- [x] Key selection: PK, unique key, or user-defined columns (PK or typed columns; no unique key picker)
- [ ] Column mapping and exclusion
- [x] Row-level diff: only in source, only in target, different (cell-level highlight) (changed cells listed as key, column, source and target)
- [x] Filters (WHERE clause per table)
- [x] Streamed comparison of sorted sets for large tables (avoid loading everything into memory)
- [x] Generate sync script (INSERT/UPDATE/DELETE) or apply directly (opens in a SQL window to run; no direct apply)
- [ ] Save compare profiles

### 5.16 Schema / Object Compare (M7)

- [x] Compare object definitions between two schemas/DBs
- [x] Normalize DDL before compare (whitespace, storage clauses, schema prefix)
- [x] Side-by-side diff view with inline highlighting
- [x] Generate migration script in dependency order
- [x] Compare DB object vs local file (version control workflow)

### 5.17 Data Export (M5)

- [x] Export query result or tables to CSV, TSV, XLSX, JSON, XML, HTML, Markdown, SQL INSERT statements (no XML)
- [x] Export tables tool: multiple tables, WHERE per table, include DDL
- [x] Options: delimiter, quoting, header, date/number formats, NULL representation, encoding (delimiter, header, NULL text; UTF-8 only)
- [x] Streaming export (no full result in memory) (Execute to file)
- [ ] LOB handling: inline, separate files, skip
- [x] Export user objects (DDL for a schema) to files, one file per object
- [x] Progress and cancel (Execute to file, schema export)

### 5.18 Data Import (M5)

- [x] Import from CSV/TSV, XLSX, JSON (also XLS and ODS; no JSON)
- [x] Preview with detected types; column mapping to target table
- [x] Create target table from file structure
- [x] Transformations: trim, date format, constant values, SQL expressions
- [x] Batch inserts via array DML, configurable commit interval (one commit at the end, optional; no interval)
- [x] Error handling: stop, skip row, log bad rows to file
- [ ] Run SQL scripts as import (large `INSERT` scripts)
- [ ] Optional: SQL*Loader control file generation

### 5.19 Explain Plan & Performance (M8)

- [x] Explain plan as a tree grid (cost, cardinality, bytes, predicates)
- [x] Actual execution plan via `DBMS_XPLAN.DISPLAY_CURSOR` with `GATHER_PLAN_STATISTICS`, as a tree with E-Rows next to A-Rows (E-Rows × Starts against A-Rows, misestimates of 10× and more marked)
- [x] SQL Monitor report viewer (if licensed): monitored executions of a session or statement, report as text or HTML
- [x] Statistics of the last execution (`V$MYSTAT` deltas: consistent gets, physical reads, ...)
- [x] PL/SQL profiler (`DBMS_HPROF`) with result view, hot lines in the editor (per subprogram and statement; double-click opens the line; no editor markers)
- [x] Trace file helper (enable 10046 for session)

### 5.20 Session & Database Monitoring (M8)

- [x] Session browser (`V$SESSION`): filters, current SQL, waits, locks, open cursors
- [x] Kill session / cancel SQL (with confirmation)
- [x] Lock tree / blocking sessions
- [x] Long operations (`V$SESSION_LONGOPS`)
- [x] Drill-down from the dashboard: tiles, wait classes, users, programs, machines, cursor holders, long operations and wait events lead to the sessions behind them
- [x] Session details: overview (client, connection, transaction, PGA, temp, PL/SQL, time model, waits), captured binds, the session's own plan (as a tree with actual rows when the cursor collected statistics), statistic deltas, all columns
- [x] SQL details: totals, child cursors, plans, binds, why children aren't shared, sessions running it
- [x] With the Diagnostics Pack: ASH activity of a session or statement, filtered ASH reports, plan history from AWR
- [x] Tablespace usage overview
- [x] DBMS_SCHEDULER jobs: list, run, enable/disable, history, create and edit (form with repeat interval presets and the next runs)

### 5.21 Refactoring & Productivity (M8)

- [x] Rename local variable / parameter (live multi-cursor rename, Shift+F6)
- [ ] Extract procedure from selection
- [x] Generate: column list, `INSERT`/`UPDATE` template for table, record type, `%ROWTYPE` declarations (column list and SELECT/INSERT/UPDATE from the object browser; no record type or `%ROWTYPE` declarations)
- [x] Generate package body stubs from spec
- [x] Synchronize spec and body signatures (body → spec for changed signatures; Generate body for the other direction)
- [ ] Macro recorder (replay command sequences)
- [x] Editor tabs: pin, reorder, close others, reopen closed (no pinning)
- [ ] Project / workspace: group files, objects, and connections

### 5.22 Version Control Integration (M8/M9)

- [x] Projects map to folders, objects saved as files (`.pks`, `.pkb`, `.sql`, ...)
- [x] Git status in file tree (via the git command line, not `gix`)
- [x] Diff working copy vs HEAD, vs DB version
- [x] Deploy changed files to a connection (generated `@file` script)

### 5.23 Reports (M9)

- [x] Saved parameterized SQL reports
- [x] Built-in reports (invalid objects, object sizes, grants, ...)
- [x] Export report to HTML/XLSX (through the result grid's export)
- [x] Simple charts for query results (bar/line) – optional

### 5.24 Extensibility (Later)

- [ ] Plugin API (WASM via `wasmtime` or Lua via `mlua`)
- [ ] User-defined tools (external commands with variables like `$OBJECT`, `$FILE`)
- [ ] Custom menu entries and toolbar buttons
- [x] Optional AI assistant integration (explain SQL, generate queries) – opt-in, never sends data without consent (result rows and bind values only when allowed in Settings → AI)

---

## 6. Non-Functional Requirements

### Performance
- [ ] Cold start < 1s (excluding DB connect)
- [ ] Typing latency < 16ms in a 50k line package body
- [ ] Scroll through 1M rows at 60fps
- [x] Object browser for a 100k object schema loads lazily
- [ ] Benchmarks in CI (`criterion`) for lexer, parser, grid rendering

### Reliability & Safety
- [x] Never lose unsaved editor content: autosave to a recovery folder (the workspace, including unsaved editor text, is saved every 30 s; no separate recovery folder)
- [x] Transaction state always visible; warn before disconnect/exit with uncommitted work
- [x] All destructive actions confirm, with extra confirm on production-flagged connections
- [x] No passwords in logs, history, or crash reports

### Quality
- [x] Unit tests for lexer, parser, formatter (snapshot tests with `insta`)
- [x] Integration tests against Oracle Free 23ai in Docker
- [x] CI: build + test on Linux, Windows, macOS
- [x] `clippy` and `rustfmt` enforced

### Packaging
- [x] Windows MSI/installer, macOS `.dmg` (signed/notarized), Linux AppImage / `.deb` / Flatpak (no Flatpak; signing/notarization only when the secrets are configured)
- [x] Bundled or guided Instant Client installation (guided: steps shown when the client is missing; not bundled for licensing reasons)
- [ ] Auto-update check (optional, opt-in)

### Accessibility & i18n
- [ ] Full keyboard navigation
- [x] AccessKit support (egui has it built in) (eframe's `accesskit` feature is enabled; not audited)
- [ ] Translatable UI strings (English first, German second)

---

## 7. Risks & Open Questions

- [x] **Debugger protocol**: `DBMS_DEBUG` is deprecated, `DBMS_DEBUG_JDWP` needs a JDWP client implementation and network ACLs. Spike both early (before M6) to check feasibility
- [ ] **Instant Client dependency**: `oracle` crate needs ODPI-C + Instant Client. Is a pure-Rust thin driver (TNS protocol) worth it long-term? (spiked: `--features pythia-db/thin` runs on the pure-Rust `oracledb` crate; no cancel, external authentication or proxy yet)
- [x] **Editor widget effort**: a custom high-quality code editor in egui is a big task. Evaluate existing crates (`egui_code_editor`, `egui-textedit` extensions) vs building our own
- [x] **PL/SQL grammar coverage**: the grammar is huge; aim for a "good enough" error-tolerant parser for outline, completion, and navigation rather than a full validating parser
- [ ] **egui text rendering**: check font fallback, CJK, and subpixel quality on all platforms
- [x] **Licensing**: choose a license (MIT/Apache-2.0 vs commercial); check Oracle Instant Client redistribution terms (MIT OR Apache-2.0; Instant Client may not be bundled)
- [ ] **File format compatibility**: should we import PL/SQL Developer `.tst`, `.sql`, preferences, and connection lists?

---

## 8. First Steps

- [x] Set up the Cargo workspace and CI
- [x] eframe + wgpu window with egui_dock layout
- [x] Spike: connect with the `oracle` crate, run a query on a worker thread, show the result in a virtualized grid
- [x] Spike: minimal custom code editor with ropey and a PL/SQL lexer for highlighting
- [x] Spike: `DBMS_DEBUG` and `DBMS_DEBUG_JDWP` proof of concept (set breakpoint, step, read a variable)
- [x] Docker compose file with Oracle Free for local dev and tests
