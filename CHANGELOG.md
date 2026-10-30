# Changelog

All notable changes to Pythia are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.10.0] - 2026-10-30

### Added
- A filter for the outline of a program window (#173), for large
  packages. Typing part of a name lists the declarations that contain it,
  with the package or subprogram around them, and says how many match.
  Enter goes to the first one in the code, Escape clears the filter.

### Changed
- The debugger's SQL plan no longer opens by itself (#169). On a
  misestimate it came up beside the code in the middle of stepping and
  halved the code's width. The toolbar's *SQL* is now a plan icon. It
  blinks for a few seconds when a statement's estimates are off by 10×
  or more, and then shows ⚠ with the number of such statements; opened,
  the plan shows the flagged statement. The gutter still marks its line.
  The setting *Open the SQL panel on a misestimate* is gone.

### Fixed
- Conditional compilation (`$IF … $THEN … $ELSE … $END`) showed errors
  that aren't (#171): `$ELSE` as "ELSE outside of IF or CASE", `$END`
  before an IF as "END IF without matching IF", and the real END IF after
  it the same. The editor reads each `$IF`'s first branch as the code, as
  if its condition held, and reports only directives that don't pair up
  (`$IF` without `$END`, `$END` without `$IF`). Folding no longer closes
  an IF at `$END`, and the directives, `$$PLSQL_UNIT` too, show as
  keywords instead of a red `$`.
- *Format* broke conditional compilation (#172): `$IF x $THEN` became
  `$ if x $ then`, which Oracle doesn't compile. Statements with `$IF`
  are kept as written (their branches are often half a construct);
  `$$PLSQL_UNIT` and the other inquiry directives format like names.
- A program window the debugger opens, at a stop or from a line of the
  SQL plan, comes without the outline (#170): it took the code's width
  while stepping. *Outline* shows it; a window that was already open
  keeps it as it was.
- The SQL plan's note without row source statistics said to turn on
  *Plan statistics* in the debugger toolbar; it is in Settings →
  Debugger since #162, and the note says so (#174).
- Pressing Shift, Ctrl or Alt in an editor scrolled back to the caret
  (#175), so a Shift+click (select to there), Ctrl+click (go to
  definition) or Alt+drag on lines scrolled away from the caret landed
  somewhere else. egui sends these keys as presses of their own since
  0.36; the editor no longer reacts to them.
- Settings → Keyboard recorded a shortcut's first modifier as the
  shortcut (#176): pressing Ctrl+Shift+K saved "Ctrl+ControlLeft". It
  waits for the key after the modifiers now.
- *View → Variables (SQL window)* did nothing in a SQL window without
  bind (`:name`) or substitution (`&name`) variables, and switched the
  panel off unseen: it then didn't open for variables typed later
  (#168). The item is disabled there, with a hover text saying why, and
  ticked while the panel shows; from the command palette it says the
  window has no variables.

## [0.9.0] - 2026-10-29

### Changed
- The debugger's panels are tabs of their own (#163): *Debugger* (the
  stepping buttons, the state and the call stack), *Variables*,
  *Watches*, *Breakpoints* and *SQL plan*. Drag them into tabs, splits or
  floating windows, or close the ones you don't need; the arrangement is
  kept across restarts, and *Show Debugger* brings back a closed panel.
  By default they sit side by side below the code, breakpoints behind
  watches, and the SQL plan opens right of the code. Its tab follows the
  toolbar's *SQL* (closing it hides the plan, the checks go on) and comes
  to the front on a misestimate. The toolbar wraps in a narrow tab; the
  shortcuts are in the buttons' hover text.
- The debugger's SQL panel stays closed until it's needed (#162). Every
  stop checks the plans of the statements that ran in the step; one whose
  estimates are off by 10× or more (with at least 200 rows on one side)
  gets a warning in the gutter on its line, with the note as hover text,
  and the toolbar's *SQL* shows ⚠ and their number. The panel opens on
  such a statement by itself the first time it is flagged (*Open the SQL
  panel on a misestimate*, Settings → Debugger). *Plan statistics* moved
  from the debugger's toolbar to Settings → Debugger.
- INSERT statements of a result go into the table the query reads
  (#158): *Copy as → SQL INSERT*, *Export* and *Execute to file* (`.sql`)
  wrote `insert into my_table` whatever the query. For `select * from
  hr.orders where …` they write `insert into hr.orders`, the name as the
  query has it. Results of joins and other queries over several tables
  keep the placeholder `my_table`.
- Hovering a selection in the debugger shows what it reads (#165):
  selecting `art_(1)` and hovering it showed `art_`, the whole
  collection. When the mouse rests in a selection that reads as a path
  (`art_(1)`, `l_rec.items(2).name`), the hover shows that element or
  field. In hovers and watches a subscript may name a variable of the
  frame: `l_order.lines(i)` is `lines(3)` while `i` is 3, and a text
  variable is a key of an `INDEX BY VARCHAR2` table. Hovering `amount` in
  `l_order.lines(i).amount` shows the whole path, no longer `i.amount`.
- The debugger says why an OPAQUE value has no contents (#166) when its
  type is declared in the package spec of the debugged body (debugging
  compiles only the body with debug information) or is a SQL type: the
  unit owner's, another owner's (`sys.odcivarchar2list`) or one reached
  by a synonym. Oracle's debugger can't read into an OPAQUE value at all;
  a watch or hover on one of its elements or fields says "art_ is OPAQUE:
  its contents aren't available" instead of "art_ has no (1)".

### Fixed
- A `&` in an exported value turned into a substitution variable when
  the INSERT statements ran (#159): `'Missing (H&L)'` asked for `L` in a
  SQL window (and in SQL*Plus) and inserted other text. The `&` is
  written outside the quotes now: `'Missing (H' || chr(38) || 'L)'` on
  Oracle, `N'…' + NCHAR(38) + N'…'` on SQL Server, `E'…\x26…'` on
  PostgreSQL and a hex literal on MySQL.
- Completion offered wrong names in large schemas (#160). The popup asked
  once, after two letters, for the best 300 names, then only filtered
  those while typing on: names past the first 300 for those letters (in
  IFS, `mxn_it…` after hundreds of `mxn_a…`) never showed, and names that
  merely contained the typed letters somewhere (`mxn_access_mgmt_hist`
  for `mxn_it`) filled the list. A list cut short is asked for again
  while typing on, and names match by prefix, word starts (`mxn_it` →
  `mxn_inventory_transaction_tab`) or substring, no longer by letters
  scattered through them.
- Completion could lack objects of a large schema (#161): the schema's
  objects and the public synonyms were read in one query cut at 50,000
  rows, in no order. The schema's objects are read in full now, the public
  synonyms after them. A materialized view is offered once, no longer
  also as its table of the same name.
- *New SQL* (toolbar, *File → New SQL Window*, Ctrl+N) opened the window
  without the keyboard focus in its editor: typing went nowhere until a
  click into it (#157).
- Hovering a field in the debugger (`name` in `l_rec.name`) showed its
  value for one frame only, then the popup was gone (#164).

## [0.8.0] - 2026-10-29

### Added
- The debugger shows the SQL the stopped code ran, with the real plan
  (#152). The *SQL* panel beside the variables lists the statements the
  debugged session ran from PL/SQL since the start, each with the unit
  and line it came from (`V$SQL.PROGRAM_ID` / `PROGRAM_LINE#`, anonymous
  blocks and dynamic SQL included); the line link opens it. At every stop
  it selects the statement at the stopped line, or the closest one before
  it, and shows its plan with the last execution's Starts, E-Rows, A-Rows,
  A-Time and Buffers next to the estimates. Lines where E-Rows × Starts is
  off by 10× or more are marked, the deepest one named above the plan
  ("estimated 25,000 rows, got 200 (125× fewer)"). It says when a
  statement is still running (stopped inside its loop: the figures are its
  last finished execution's, or come when the cursor is closed) and when
  other sessions run it with another plan. *Ask AI* sends the plan with
  the statistics, the code around the line and the variables at the stop
  (as bind values, under the data policy); *Copy plan* copies DBMS_XPLAN's
  report. *Plan statistics* in the debugger toolbar (on by default) runs
  the debugged session with `STATISTICS_LEVEL = ALL`, which gives its
  statements child cursors of their own. Needs SELECT_CATALOG_ROLE (or
  SELECT on V$OPEN_CURSOR, V$SQL and V$SQL_PLAN_STATISTICS_ALL).
- Users and roles of MySQL/MariaDB, PostgreSQL and SQL Server can be
  created and edited (#142), like Oracle's. *New user…* / *New role…* on
  the Users and Roles sections (and in *File → New Object*), and *Edit
  user…* on one, open a form that shows the statements before *Apply*.
  - MySQL/MariaDB: user and host, password, expired, locked; the roles
    granted, with admin option and whether they are enabled at login
    (MariaDB enables one); privileges on the server (`*.*`), a database
    or a table, and the grant option for each.
  - PostgreSQL: password, valid until, connection limit and the role's
    options (superuser, create databases, inherit, …); the roles it is a
    member of, with admin option; privileges on databases, schemas,
    tables and sequences, each with or without grant option.
  - SQL Server: a user for an existing or a new login, without login, or
    (contained databases) with a password; default schema, the login's
    password and whether it is disabled; role memberships; database,
    schema and object permissions granted, grantable or denied.

  Without the rights to read everything (on MySQL, `SELECT` on the
  `mysql` database), the form says so, grants what you add, and revokes
  nothing it couldn't read.
- Maintenance of a table, materialized view or index (#144): *Maintenance…*
  on its right-click menu opens a window with its state first, as evidence
  for what it needs: when its statistics were gathered, whether they are
  stale or locked, and unusable indexes and partitions (Oracle); dead rows,
  the last (auto)vacuum and analyze, changes since, transaction ID age and
  invalid indexes (PostgreSQL); reclaimable space (MySQL); statistics age
  and index fragmentation (SQL Server). Then the database's tasks, each
  with its options and what it locks or commits:
  - Oracle: gather, delete, lock or unlock statistics; rebuild unusable
    indexes and (sub)partitions, or an index; shrink space (row movement
    only for as long as it takes); move, online from 12.2, with the
    indexes rebuilt after an offline move.
  - PostgreSQL: `VACUUM` (analyze, freeze, full, verbose), `ANALYZE`,
    `REINDEX` (concurrently from 12).
  - MySQL: `ANALYZE`, `OPTIMIZE` and `CHECK TABLE`, with their answers.
  - SQL Server: `UPDATE STATISTICS` (sampled or full scan), rebuild
    (online on Enterprise) or reorganize the indexes.

  The statements show before they run, one by one, on the window's own
  session with Break; it commits each, so it never commits another
  window's changes. On a production connection it asks first, on a
  read-only one it doesn't run.
- A **maintenance scan** of a schema (Oracle, MySQL) or database
  (PostgreSQL, SQL Server) lists what its tables need, each finding with
  its evidence and the statements that fix it, and runs the picked ones as
  a batch (#145). *Tools → Maintenance Scan*, or *Maintenance scan…* on the
  Tables folder's right-click menu.
  - Oracle: missing and stale statistics (locked ones too, not picked:
    gathering them takes force), unusable indexes and index
    (sub)partitions, rebuilt first; for who may read the DBA views,
    whether the automatic statistics task is enabled and how its last runs
    went.
  - PostgreSQL: dead rows, tables never vacuumed or analyzed, statistics
    behind, transaction ID age past `autovacuum_freeze_max_age` (VACUUM
    FREEZE), and indexes a failed `CREATE INDEX CONCURRENTLY` left invalid
    (`REINDEX INDEX CONCURRENTLY`).
  - MySQL: reclaimable space (`OPTIMIZE TABLE`), statistics older than the
    table's last change (`ANALYZE TABLE`; reading their age takes `SELECT`
    on `mysql.innodb_table_stats`).
  - SQL Server: fragmented indexes of 1,000 pages or more, reorganized
    (5–30 %) or rebuilt (over 30 %, online on Enterprise); statistics with
    many changes; the last good `DBCC CHECKDB` (offered, not picked).

  The batch goes object by object on the window's own session: a failing
  statement skips the rest of its object's, Break stops it all, and the
  scan runs again afterwards. While a statement runs, both maintenance
  windows show the progress the database reports (Oracle's long
  operations, PostgreSQL's `pg_stat_progress_*`, SQL Server's percent
  complete).
- The dashboards' findings offer their fix (#145): PostgreSQL's table with
  many dead rows opens its maintenance on VACUUM (ANALYZE), and transaction
  ID wraparound in the connected database opens the scan. MySQL's dashboard
  names the table with the most reclaimable space, with OPTIMIZE TABLE.

### Fixed
- On SQL Server, `UPDATE STATISTICS` in a SQL window without auto-commit
  left a transaction open, holding its locks until Commit (#150).
- On MySQL/MariaDB, PostgreSQL and SQL Server, *New user…* on the Users
  section opened a form that only said "This tool isn't available for
  MySQL connections" (#142).
- Result grids lost the keys one frame after a click (#143): the arrows,
  Enter/F2, typing, Delete and Ctrl+C/V did nothing unless pressed right
  away, and moving the mouse was enough to lose them. The grid keeps them
  now until something else is clicked.
- The single record view couldn't edit (#141). It showed the fetched
  values instead of the pending edits, couldn't reach inserted rows, and
  stepped past the row filter. Its fields now edit like the grid's cells:
  click one and type, press Enter or F2, or double-click; the context
  menu has *Edit cell*, *Edit in dialog…* and *Set NULL*. Changed, new
  and deleted values are tinted as in the grid. An inline edit open when
  you switch views or records is kept.
- A MariaDB session's details failed on the plan (#146). Before 10.9 it
  was a syntax error: MariaDB only has `SHOW EXPLAIN`, which is used now.
  From 10.9 on, a session waiting for a row lock held up the monitor for
  30 s before failing. MariaDB can't explain a statement while it waits
  for a row lock, so the details now say so right away.
- A MariaDB session's details never showed which session it waits for,
  and listed no row locks (#147). With performance_schema on, the Locks
  section failed with *Table 'performance_schema.data_locks' doesn't
  exist*. On MariaDB and MySQL 5.7, the details now read them from
  information_schema, which lists the locks involved in lock waits.
- The MySQL dashboard showed *Table
  'performance_schema.replication_connection_status' doesn't exist* on
  MariaDB, and never showed a MariaDB replica's connections (#148). They
  now come from `SHOW ALL SLAVES STATUS`: each connection's source, its
  IO and SQL threads, lag and last error.
- The MySQL dashboard's lock waits named the table as
  `` `schema`.`table` `` on MariaDB and MySQL 5.7 (#149). They show
  `schema.table` now, as on MySQL 8.

### Changed
- *Run with statistics* and the monitor's session and SQL details show
  the actual plan as a tree with Starts, E-Rows, A-Rows, A-Time, Buffers
  and Reads, the worst misestimate named above it, and DBMS_XPLAN's
  report a click away (#152). The session details' plan has the last
  execution's statistics too (ALLSTATS LAST) when the cursor collected
  them, and so does the plan the assistant gets for a cursor.


## [0.7.0] - 2026-10-28

### Added
- On MySQL/MariaDB the object browser remembers the database you last
  picked for each connection (#133). When the connection comes up without
  one, it opens that database again, and SQL windows run in it. A database
  set in the profile (in the address, as default schema, or by the login
  script) still wins. If the remembered database is gone, the log says so
  and it is forgotten.
- **Monitor → Sessions** and **Blocking Sessions** for MySQL/MariaDB,
  PostgreSQL and SQL Server (#131). Every session is listed, idle ones too
  (the server's own with *Background*), with its user, host, database,
  program, status, wait, time, open transaction, blocker and statement;
  filter, sort, and *Active only* as on Oracle. The blocking tree shows who
  waits for whose locks, idle blockers with an open transaction marked.
  Double-click opens the session's details; the menu cancels its statement
  or ends the session after asking, and opens the statement in a SQL
  window. The dashboard's "connections by …" bars open the sessions they
  count. Without `PROCESS`, `pg_read_all_stats` or `VIEW SERVER STATE`,
  the list says what is missing.
- The object browser lists users and roles on MySQL/MariaDB, PostgreSQL
  and SQL Server too (#130), not only on Oracle. MySQL's users show as
  `user@host`; without `SELECT` on `mysql.user` the browser shows your own
  user and the roles you may use. PostgreSQL's roles that can log in are
  its users, superusers say so; SQL Server's are the database's users and
  roles. Their menu has *Copy name* and *Drop…* (and *Show schema* for a
  PostgreSQL user with a schema of the same name); editing and creating
  them stays on Oracle.
- The outline of a program window has a context menu (#124). Right-click
  a procedure or function to **Test…** or **Debug…** it: the standalone
  unit, or a subprogram the package specification declares, from the
  spec's or the body's outline (overloads included). Private, local and
  object type subprograms show the entries disabled and say why, as does a
  unit not compiled yet. **Go to body** / **Go to specification** shows a
  package subprogram in the other part (Back returns), **Find references**
  works on every declaration, and **Copy name** copies it. With unsaved
  changes, Test… and Debug… (here and in the editor's menu) say on hover
  that they run the version compiled in the database.
- **Debugger setup** (#122): *Debug → Debugger Setup…*, *Setup…* in the
  Debugger panel and Settings → Debugger, and on its own when a debug run
  can't connect. To debug, the database connects back to Pythia, which
  takes `DEBUG CONNECT SESSION`, a network ACL entry for the host it
  connects to, and a way through firewalls. The setup tries each host the
  database might reach this computer by for real (the address the
  database sees for it, its full and short name, one typed in) and says
  why one fails: no ACL entry, a name the server can't resolve, refused,
  or dropped on the way. It offers what's missing: *Use* a host that
  works; the grants and the ACL entry (this host, its subnet or domain,
  or any), run as yourself, through a saved DBA connection or a user and
  password (in your PDB when the DBA logs in to the root), or copied for a
  DBA, with the script to undo it; and a rule for Windows Firewall
  (also removing rules that block Pythia, left by answering *Cancel* to
  Windows' prompt), ufw, firewalld or the macOS firewall, run with
  administrator rights after asking. New setting *Debugger ports*
  (Settings → Debugger) fixes the ports the debugger listens on, for
  firewall rules and ACL entries limited to a few; when the ACL allows
  only some ports, the setup offers to listen there. While the session
  is slow to connect, the panel says what may be wrong.
- Debugger: change values while stopped (#117). Double-click a value in
  Variables or Watches, or right-click it → *Set value…* or *Set to
  NULL*, to change a local variable, an IN OUT or OUT parameter, a record
  field, a collection element or a package global; the code goes on with
  the new value. Numbers are checked against their precision and range
  first (some wrong values would end the debugged session), dates and
  timestamps are written as shown (the session's NLS format), and a value
  the database refuses says why and stays as it was. IN parameters and
  LOBs can't be changed.
- Debugger: **Run to Cursor** (Ctrl+F7, *Debug → Run to Cursor*, or the
  editor's context menu while stopped) continues to the cursor line of a
  program window, or of the block being debugged in a SQL window, and
  stops there once, also in a unit that isn't loaded yet. A breakpoint or
  an exception on the way stops first and ends it (#117).
- Breakpoints, with whether they're enabled, their conditions and hit
  counts, the watches and *Break on exceptions* are kept across restarts
  of Pythia (#117).
- Errors lead to the code they were raised in. In Messages, each line of a
  PL/SQL error stack (`ORA-06512: at "OWNER.NAME", line N`) is a link that
  opens the package body, procedure, function, type body or trigger at that
  line; hovering shows the line's code. **Open where raised**, next to
  *Fix with AI*, opens the innermost one whose source you can see. Lines in
  code you can't see (another schema's body without the privilege, wrapped
  code) say why on hover, and the line of the block that ran selects it in
  the editor. A program window with unsaved changes notes that the line is
  the one in the database (#112).

### Changed
- Pythia opens the connect dialog when it starts (#132), on the profile
  used last with its saved password, so connecting again is one Enter.
  On a first start it opens empty, for a new connection. *Cancel* leaves
  Pythia without a connection as before.
- SQL windows show their bind and substitution variables in a Variables
  panel next to the editor instead of asking in a dialog before each run
  (#110). The values (or NULL) stay visible and can be changed at any
  time; runs use them as they are. Only a statement with a variable that
  has no value yet waits, with that variable focused in the panel, until
  Execute, Enter or F8 (Break cancels). Debug works the same way. The
  panel shows while the text has variables; *Variables* below the editor
  or *View → Variables* hides or shows it. `&var` keeps its value for
  the window like `&&var`, instead of being asked for every run.

### Fixed
- Disconnecting the connection the object browser showed, or connecting
  its profile again, cleared the favorites and recent objects of every
  connection, and the next workspace save made it permanent (#136). They
  stay now.
- The connect dialog didn't connect on Enter when the profile's password
  is saved (#134): nothing had the focus. *Connect* has it now whenever
  user and password are filled in and no field has it.
- Explain plan failed on MariaDB with *Unknown EXPLAIN format name:
  'tree'* (#128). MariaDB has no tree format; it gets the classic plan
  table, as MySQL before 8.0.16 does.
- The *Uses* tab of a view and the *Used by* tab of a table failed on
  MariaDB with *Unknown table 'view_table_usage' in information_schema*
  (#129). Without that table (MariaDB, MySQL before 8.0.13), they are
  found in the view definitions.
- The status bar's invalid objects count didn't follow compiles in a
  program window, changes made with an object or table editor, or PL/SQL
  blocks run in a SQL window (`exec dbms_utility.compile_schema(user)` and
  the like): it showed the old number until the next periodic refresh
  (#127). It is counted again after each of them, and once more when
  objects changed while a count was running. A count lost with a closed or
  reconnected session no longer keeps the status bar from updating.
- *Switch Spec/Body* jumped to the wrong overload when the body implements
  a package's overloads in another order than the spec declares them
  (#125). It now goes to the one with the same parameters.
- *Test…* and *Debug…* generated a block that didn't compile when the
  function returned, or an OUT parameter was, a collection, record,
  `%ROWTYPE`, object, BLOB, XMLTYPE or ref cursor (#120). The block now
  prints those too: collections element by element (the first 100),
  records and objects field by field, ref cursors row by row, LOBs and
  XML as text. IN parameters a text bind can't carry are locals with a
  `-- TODO` to fill them in; BOOLEAN, BLOB (hex) and XMLTYPE binds are
  converted. Signatures no longer show `PUBLIC.XMLTYPE` or a strong
  cursor as its row type.
- *Test…* and *Debug…* on a package, procedure or function with a quoted
  mixed-case name (`"MyPkg"`) called the name lowercased, which doesn't
  exist (#121).
- Debugger: when the debugged session couldn't connect (ORA-24247 and the
  like), the panel kept saying *Waiting for the session…* until Stop. The
  run now ends and the panel says why (#123).
- The object browser's favorites and recently opened objects, and the open
  project, were lost when Pythia was closed with no document open (#118).
- Debugger: *Watch* on an element of a collection with text keys
  (`l_map('Key')`) said the collection "has no ('key')": the key was
  lowercased, also in the element's label (#119).
- Trigger compile errors were marked too high, by the number of lines of
  the trigger's header: Oracle counts their lines from the trigger's body.
  Program windows, SQL windows and the invalid objects window now show them
  on the right line (#114).
- The fields of the Variables prompt (binds and `&substitutions`, before
  running or debugging) were only a few characters wide, so what you
  typed couldn't be read (#109).
- Light theme: selected text was almost invisible, in the editor and in
  every text field (#111). The selection color added to the white
  background instead of tinting it.
- Settings: a long hint under a setting was one line, so *Switch
  connection per window* on Queries made the window wider than the
  screen, with the page list off its left edge, and it stayed that wide
  (#113). Hints wrap now, controls that don't fit continue on the next
  line, and the window is never wider than the screen.
- Settings → Formatter: the *New profile* row didn't line up with the
  other rows at a bigger interface font (#116).
- Read-only connections ran statements that aren't queries in some cases
  (#115). A script went on after its first statement (`select …; delete
  …;` ran the delete), as did a statement after its variables were
  entered, and Profile and Debug ran the block at the cursor. Every
  statement is checked now, and Profile and Debug are refused.

## [0.6.0] - 2026-10-27

### Added
- Settings → Appearance: the interface's font (a TrueType/OpenType file)
  and size, apart from the zoom, which still scales everything. A Compact
  density shrinks padding, gaps, margins and tree indent, which can also be
  set one by one; rows of the object browser, lists and tables, the
  toolbar, the status bar and the tabs follow. An accent color, square
  corners, shadows, scroll bars that are always visible, animations off,
  and object names in the browser in the interface font. The result grid
  gets its own font and size, striped rows on or off, lines between rows
  and the text and color of NULL; the editor gets line spacing and a
  block or underline cursor that need not blink (#105).
- PostgreSQL and SQL Server connections (experimental): *Type: PostgreSQL*
  and *Type: SQL Server*, with the same tools as MySQL except the SSH
  tunnel and the dashboard (#36). Scripts split the database's way:
  dollar-quoted routine bodies and `::` casts on PostgreSQL, `GO` batches
  and `BEGIN … END` blocks on SQL Server.
- PostgreSQL and SQL Server: a monitoring dashboard each (Monitor →
  Dashboard), like MySQL's: findings first, KPI tiles and trend charts from
  the server's counters, what runs and waits now, connections by user,
  application or program, client and database, lock waits and blocking
  with the blocker's statement (or that it sits idle in a transaction),
  open transactions, storage and top statements with *Analyze*. On
  PostgreSQL from `pg_stat_activity`, `pg_stat_database`, `pg_locks`, the
  replication views and `pg_stat_statements`, with findings for idle
  transactions, replication lag and inactive slots, transaction ID
  wraparound and dead rows. On SQL Server from the DMVs and performance
  counters, with top waits since the refresh before, log use and page
  life expectancy. Without `pg_read_all_stats` or `VIEW SERVER STATE` the
  dashboard shows what it can and says what's missing (#92).
- MySQL: results can be edited in place, as on Oracle (#101). MySQL has
  no ROWID, so the table's primary key (or a unique key on NOT NULL
  columns) finds a row again: *Edit* adds its columns, read-only, to the
  end of the result, also when the query doesn't select them. Applying
  checks the values that were fetched, so a row another session changed
  is a conflict; FLOAT values, which MySQL shows rounded, and JSON
  compare as such. Text with a backslash is written as hex, the same with
  or without `NO_BACKSLASH_ESCAPES`. Tables without such a key stay
  read-only and say why. MySQL now counts the rows an UPDATE matched, as
  Oracle does, not only those it changed.
- SQL windows: *Edit* isn't offered for DISTINCT, GROUP BY or aggregate
  queries, whose rows aren't the table's. Backtick-quoted names are
  identifiers to the editor and the statement splitter (a `;` in one
  doesn't end the statement).
- PostgreSQL and SQL Server: session details from the dashboard. A
  backend's or session's id (running statements, open transactions, lock
  waits and blocking) opens how it connected, the statement it runs (on
  SQL Server with its cached plan as an outline, and the showplan XML to
  copy) or ran last, its transaction, its locks with the tables named,
  and the sessions it waits for and blocks. *Cancel query* and
  *Terminate* (PostgreSQL) and *Kill* (SQL Server) ask first and say what
  is rolled back (#96).
- PostgreSQL and SQL Server: *Connect through SSH*, as on MySQL. The
  address is the database as the SSH server sees it
  (`localhost:5432/database`); on PostgreSQL TLS still checks the
  database's own host name, on SQL Server the login stays encrypted. A
  named SQL Server instance (`host\instance`) can't be found through a
  tunnel: give its port instead (#97).
- SQL Server: `PRINT` output and informational messages (`RAISERROR` up to
  severity 10, "Changed database context to …") show as messages, in
  order with the batch's result. Pythia uses our fork of tiberius for it
  (#98).

### Changed
- Building: a `fast` profile (`cargo run --profile fast -p pythia-app`)
  for running an optimized build locally; a change rebuilds in 5–15 s,
  where the release profile takes about two minutes for its link-time
  optimization. Clean builds no longer compile aws-lc (about 90 s): TLS
  runs on ring in all drivers (#99).

### Fixed
- Read-only connections no longer let results be edited (#107): *Edit*
  isn't offered, and the changes of a result can't be applied. Before,
  only statements run from the editor were held back.
- *Export → SQL INSERT* and *Copy as → INSERT* wrote Oracle SQL on every
  connection (#106). On MySQL, column names went into double quotes (a
  syntax error), dates into `to_date(…)`, and `VARBINARY` values into
  quoted hex text that was stored as characters. SQL Server's
  `datetime` swapped month and day under `DATEFORMAT dmy`. The statements
  now use the connection's quoting and literals: `X'…'` and backtick names
  on MySQL, `E'…'` for backslashes on PostgreSQL, and `N'…'`, `0x…` and
  ISO 8601 dates on SQL Server. A numeric cell like `NaN` or `Inf` is
  quoted instead of written as a name.
- Oracle over TLS (TCPS) no longer panics on connecting: with two crypto
  providers compiled in (ring and aws-lc), rustls couldn't pick the one
  the driver asks for. Pythia installs ring as the default first (#104).
- Connect dialog: *Connect* no longer stays disabled after *Connect through
  SSH* was ticked for MySQL and the type changed to one without tunnels;
  the hidden SSH settings neither hold it up nor go along (#95).
- Editing another user no longer needs SELECT_CATALOG_ROLE: `ALTER USER`
  and `GRANT` don't need the DBA views, and the form failed with
  *reading users and roles needs SELECT_CATALOG_ROLE* (also when only
  `DBA_USERS` could be read, e.g. with AUDIT_ADMIN). It opens with what
  the session can read. Fields it couldn't read start empty and stay as
  they are unless filled in; lock and unlock are an explicit choice. Roles
  and privileges it couldn't read can be added but aren't revoked, and
  object privileges show those the session granted or on its own objects.
  A role not enabled in the session shows its grants as unknown instead
  of none, and a user that doesn't exist says so instead of *no data
  found* (#89).
- Monitor: one view that isn't granted no longer takes down what doesn't
  depend on it. With `V$SESSION` but not `V$SQL`, the session list, the
  blocking tree and session details failed with *needs
  SELECT_CATALOG_ROLE*; they show the sessions without their SQL text.
  Without `V$OSSTAT` the dashboard lost its whole instance header; it
  shows without the CPU count, and the load findings that need the count
  are left out instead of judging against one CPU (#90).
- Monitor: a statement's plan history from AWR lists its snapshots in
  time order; in a PDB it interleaved the root's and the PDB's snapshots
  by their ids, which each count on their own. A row's plan is also looked
  up in the other repository when the first one has none for it; when
  neither kept one, it says so instead of showing an empty plan (#93).
- Monitor: the session list's filter chip ends in an × rather than a box;
  its `✕` has no glyph in Pythia's fonts. A test now checks every character
  the UI writes against the fonts (#91).
- MySQL: the database picked in the object browser becomes the
  connection's database, as 0.5.0 meant (#81). The browser listed its
  objects, but SQL windows still failed with *No database selected*, and
  the toolbar and status bar kept saying "No database" (#102).
- A connection reports the schema its login statements leave it in. A
  MySQL profile with a *Default schema* and no database in its address
  said "No database" in the toolbar and status bar, and the object browser
  asked for one, while SQL windows ran in that schema. The same happened
  when the main session reconnected after a database was picked in the
  browser. On Oracle and PostgreSQL, the object browser and completion
  start in the profile's default schema, or the one the login script
  sets, instead of the user's own (#103).

## [0.5.0] - 2026-10-26

### Added
- Monitor: the dashboard leads to what it counts. The *Sessions* tile opens
  the session list, *Blocked sessions* the blocking tree; a wait class in
  *Active sessions by wait class* and the bars of *Sessions by user /
  program / machine* open the session list filtered to them (a chip shows
  the filter); a *Most open cursors* row opens that session's open cursors,
  a long operation its session, and a blocking finding has a *Details*
  link to the blocker. Session details offer the session list's actions in
  their header (*Session ⏷*): cancel its SQL, kill it, the new *Disconnect
  after transaction*, and SQL trace on or off as it is now. *Blocked by*
  and the sessions it blocks are links, and the kill dialog says how much
  of an open transaction it rolls back (#74).
- Monitor: session details open on a new *Overview*: OS user, machine,
  client process and driver, service, network encryption, the open
  transaction, PGA, temp space and active sorts, long operations, the
  PL/SQL being run (a link opens it at the subprogram), where its time went
  (time model) and its waits since logon. The SQL tab shows captured bind
  values and opens a statement in a SQL window; the plan is the session's
  own child cursor with peeked binds; statistics show the change since the
  read before; open cursors group by SQL_ID; locked tables and the row a
  session waits for open in the IDE; *All columns* lists the `V$SESSION`
  row. Each tab is read when it is first shown (#74).
- Monitor: *SQL details*, a tab per statement like a session's: its text
  (copy, open in a SQL window), totals and per execution since it was
  loaded, its child cursors with plan hash, executions, bind awareness and
  SQL profile / baseline / patch; for the child picked, its plan (with
  actual rows when row source statistics were gathered), captured binds
  and why it isn't shared with the others; and the sessions running it.
  Top SQL, the session list (SQL_ID column and menu) and session details
  open it (#74).
- Monitor: *Top wait events* on the dashboard: the events waited on most
  since the refresh before (from `V$SYSTEM_EVENT`), as sessions waiting,
  waits per second and average wait. An event opens the sessions waiting
  on it now, and its latency histogram (`V$EVENT_HISTOGRAM`) unfolds below
  it (#74).
- Monitor: *History* of a session (a tab of its details) or a statement (a
  section of its details), from the management packs. With the
  Diagnostics Pack: its activity in the last hour from Active Session
  History (per minute and wait class, events, objects, the statements or
  sessions involved, who blocked it), an ASH report of just it, and a
  statement's plan history from AWR with the plans AWR kept. With the
  Tuning Pack: the executions SQL Monitor watched, with their report as
  text or in the browser. A pack that isn't enabled says so instead, and
  its views are no longer even probed (#74).
- Monitor: a tablespace on the dashboard unfolds its files and largest
  segments, or for a temporary one the sessions using it; the PGA bar
  unfolds the sessions using the most PGA, each opening its details
  (#74).
- Monitor on MySQL: *thread details*, opened from the thread ids of the
  dashboard's running statements, open transactions and lock waits: how it
  connected (program, client library and version), the statement it runs
  or ran last (rows examined, temp tables, no index used; open in a SQL
  window) with `EXPLAIN FOR CONNECTION`, its transaction, the data and
  metadata locks it holds or waits for and the thread it waits for, and
  the statements before. *Kill query* and *Kill connection* ask first,
  saying what a kill rolls back (#87).
- Program windows: *Compile all* has a tooltip saying that it compiles the
  specification, then the body, and stops at the first error (#78).
- **Debug…** next to **Test…** on procedures, functions and package
  members: it opens the test block in a SQL window and debugs it right away,
  stopping on entry to the subprogram (any overload) without a breakpoint;
  its unit is compiled with debug information first. In a program window,
  right-clicking a subprogram's name in the spec, or a public one in the
  body, offers Test… and Debug… too (#71).

### Changed
- SQL and program windows: the editor fills its tab to the edges instead
  of sitting in a 22 px gray frame, which leaves more room for code. The
  rows above it (connection picker, Compile…) have a strip of their own,
  like the results below (#86).
- One connection for the whole app: with several connections open, the
  toolbar, object browser, status bar, new SQL windows and the open SQL
  windows all use the *active connection*, so a statement no longer runs on
  another database than the object browser shows. Connecting, the object
  browser's connection picker and the toolbar's connection menu switch it;
  SQL windows with a transaction or a running statement ask first (commit
  or roll back). Other connections stay open, to switch back to. Program
  windows and object views keep their connection and name it in their tab
  when it isn't the active one. Disconnecting the active connection hands
  over to another open one. With *Switch connection per window* on, SQL
  windows keep their own connections as before and the object browser
  follows the focused window (#80).
- MySQL: the database picked in the object browser is the connection's
  database: the open SQL windows switch to it (after what they're running)
  and new ones start in it, instead of failing with "No database selected"
  when the connection's address and profile name none. The toolbar and
  status bar show the current database, or "No database", and the object
  browser asks to pick one (#81).

### Fixed
- WSL: Pythia no longer takes down WSLg's compositor, which closed every
  Linux GUI app's window, often soon after Pythia started (e.g. on opening
  the MySQL dashboard). WSLg's Weston crashes when a Wayland app grows a
  shared-memory pool it draws from, as Pythia did for each new mouse cursor
  shape (microsoft/wslg#1386). Under WSLg Pythia now opens its window
  through X11 (Xwayland); `PYTHIA_WAYLAND=1` keeps Wayland (#88).
- Windows: `pythia.exe` carries Pythia's icon and version, so the taskbar,
  the Start menu and desktop shortcuts, Explorer, Installed apps and
  associated files show Pythia's icon rather than the default one, and Task
  Manager and "Open with" list it as Pythia (#77).
- Test… on an overloaded package subprogram tests that overload, not
  always the first one: in the object browser, and in a program window,
  where the overload is the declaration right-clicked in the spec, or the
  one with the same parameters as the implementation in the body (#85).
- Clicking an entry in the completion list inserts it. The click closed the
  list without inserting anything and left the editor without focus; only
  the keyboard worked (#72).
- Session details: open cursors show their whole statement, not the first
  60 characters V$OPEN_CURSOR keeps; selecting one shows it in full below
  the list (#73).
- Session details of a session that has ended no longer show the waits,
  statistics, open cursors and locks of the session that got its SID since.
  A blocker missing from the session list (shown with its SID only in the
  blocking tree) offers no details, kill or trace with a guessed serial#,
  and isn't labelled idle (#75).
- Monitor: the object a session waits on, the row it waits for and the
  tables of TM locks are named for every schema, not only for tables the
  monitoring user has privileges on (they were looked up in ALL_OBJECTS)
  (#76).
- Trace analyzer: statements run again through a cached cursor are counted
  each time. Executions, fetches and rows after the first were dropped
  once the session cached the cursor (#79).
- MySQL errors read `ERROR 1046 (3D000): …` without an Oracle error number
  in front (`ORA-01046: ERROR 1046 …`) in SQL windows, program windows,
  object views and the table editor (#82).
- MySQL: the object browser's database picker no longer lists a `PUBLIC`
  database; only Oracle has one, for public synonyms and database links
  (#83).
- After disconnecting one of several connections, Execute in a SQL window
  that was on it runs on the connection the toolbar shows instead of
  opening the Connect dialog: its windows are no longer bound to the closed
  connection (#84).

## [0.4.1] - 2026-10-26

### Added
- Instant Client packages: each release also has `pythia-instantclient_…`
  packages, built on the Oracle Instant Client, for servers that require
  Native Network Encryption, Oracle 11.2 and OS or wallet logins. They need
  the Instant Client (or an Oracle Client) on the library path; see the
  README. *Help → About Pythia* shows which driver a build uses, and the
  pure-Rust build's error for such a server points to them (#66).

### Fixed
- Net service names: tnsnames.ora is also found where Oracle's own clients
  find it: `TNS_ADMIN` and the Oracle homes in the Windows registry, and
  `network/admin` of a full client (`ORACLE_HOME\bin` or `$ORACLE_HOME/lib`
  on the library path). A name Pythia can't look up says what it searched,
  not "no configuration directory specified" (#67).

## [0.4.0] - 2026-10-26

### Added
- MySQL connections (experimental): pick *Type: MySQL* when connecting.
  SQL windows, explain plan, completion, the object browser and object views
  work; tools that rely on Oracle's dictionary or PL/SQL are hidden on
  MySQL connections. `pythia_db::Dialect` says what a connection's database
  supports, and `DbRequest::Run` runs tool code on any database.
- MySQL connections through SSH, as with HeidiSQL's SSH tunnel: *Connect
  through SSH* in the connect dialog, with a password, a key file (OpenSSH
  or PuTTY `.ppk`) or the SSH agent (Pageant on Windows). All sessions of a
  connection share one SSH connection, which logs in again when it was
  lost. A server's host key has to be in `~/.ssh/known_hosts` or trusted
  once in the dialog; a changed key fails.
- MySQL monitoring dashboard (Monitor → Dashboard): findings (lock waits
  and their blocker, metadata lock waits, connections near
  `max_connections`, buffer pool hit ratio, long transactions, replication
  stopped or lagging), KPI tiles and trend charts from `SHOW GLOBAL
  STATUS`, connections by user, host and database, running statements,
  open transactions, the largest databases and tables, and the top
  statements from `performance_schema` with *Analyze*. What needs the
  `PROCESS` privilege or `SELECT` on `performance_schema` says so.

### Fixed
- When the window system went away mid-session (WSLg's compositor
  crashed), Pythia started again with OpenGL and then reported that
  neither Vulkan nor OpenGL works. It now says the window system ended
  the connection (#60).
- Settings: the Formatter page was taller than an 800 px window, which
  pushed the close button off screen. Pages scroll inside a window that
  fits the screen (#62).
- Light theme: black showed outside the tabs' rounded corners and in the
  gaps between the dock's areas and the bars; it's the theme's background
  now (#64).

## [0.3.0] - 2026-10-23

### Added
- Directories (#43), listed with their paths in the object browser below
  profiles, with New directory…, Edit directory…, Grants… (read, write and
  execute) and Drop…. The form takes the name and the path on the server
  (`CREATE OR REPLACE DIRECTORY`, which keeps the grants) and warns
  without CREATE ANY DIRECTORY. New: Directory… joins File → New Object.
- Scheduler job form (#42), from New job… on the object browser's
  Scheduler jobs folder, File → New Object, or New job… and Edit… in the
  Scheduler Jobs window. It sets what the job runs (a PL/SQL block or a
  stored procedure, picked from the schema), its start, a repeat interval
  from presets (every few minutes or hours, daily, weekly on some days,
  monthly) or typed as a calendar expression, its end, enabled, drop after
  its last run and comments, and shows the next five runs as the database
  computes them. A new job is DBMS_SCHEDULER.CREATE_JOB, a change sets the
  changed attributes; Open in SQL window ends such blocks with `/`.
- Profiles (#41), listed in the object browser below users and roles,
  with New profile…, Edit profile… and Drop…. The form sets each password
  and resource limit to default, unlimited or a value, says what default
  stands for and lists the users with the profile; a change alters the
  limits that changed. Dropping a profile gives its users DEFAULT.
- Type form: methods (member and static procedures and functions,
  constructors, a map or order method) and "not instantiable". A type with
  methods opens with its body too, a stub per method (#40).
- User and role form: quotas on any number of tablespaces, the admin
  option of granted roles and system privileges, a user's default roles,
  and object privileges (with grant option for users) to add and revoke
  (#39).
- Table editor: a Partitions tab (#37). A new table can be partitioned by
  range, interval, list or hash, with its key, partitions (and a MAXVALUE
  or DEFAULT one) and tablespaces; a copy of a partitioned table is
  partitioned the same way. An existing table shows its partitions and
  takes new ones; truncating and dropping partitions run only with Allow
  dropping on the Script tab.
- Creating objects without writing DDL (#29). The object browser's
  right-click menu offers **New …** on every folder, a **New** submenu on
  objects (a table like this one, an index, trigger or view on a table, a
  synonym for an object) and **New index… / Add constraint… / New
  trigger…** on a table's groups; File → New Object and the command palette
  have the same, for the schema the browser shows.
  - **Table builder**: the table editor starts empty (or as a copy of a
    table). Columns with a type picker, primary key and identity column
    (12c+) checkboxes, not null, default and comment; primary, unique and
    foreign keys (referenced table picked, its key filled in) and checks
    with names generated from the table's; indexes, with one click for
    unindexed foreign keys; tablespace or a global temporary table. The
    script is `CREATE TABLE` and what follows it.
  - Forms for **materialized views** (build, refresh method and schedule,
    query rewrite), **database links** (password hidden in the preview; a
    change recreates the link, as `ALTER DATABASE LINK` needs a privilege
    of its own), **users** (authentication, tablespaces, quota, profile,
    lock, roles and system privileges) and **roles**, new and existing.
    Users and roles are listed in the object browser.
  - **Triggers, procedures, functions, packages and types** start from a
    form (events and timing, parameters, return type, `AUTHID`, attributes
    or collection type); **Open in editor** hands the source to a program
    window, where compiling creates the unit. A package's body gets a stub
    per subprogram. The form warns when the name exists.
  - What was created shows up selected in the object browser.
- Result grid: drag a header onto another to move its column, and freeze
  the row numbers and the columns up to one (right-click its header) so
  they stay in place when scrolling sideways (#33). The header menu's
  Columns submenu shows or hides each column and resets the order or the
  widths. Selection, arrow keys, paste, copy and export follow the order
  shown.
- Toolbar: a Debug button next to Execute (Ctrl+F9). It is enabled only
  with the cursor in an anonymous block of a connected SQL window, and when
  disabled says why; in a program window it points to Test… for a block
  that calls the unit.
- Debugger: a record whose type is declared in a package compiled without
  debug information comes without contents (Oracle's agent hands it over
  as an opaque value). The variables view (and the value on hover) now
  says which package it is and how to get the contents.

### Changed
- **No Oracle Instant Client needed any more.** Pythia connects with a
  pure-Rust driver (our fork of rust-oracledb, pinned: upstream fixes not
  released yet, proxy logins, statements starting with a `--` comment,
  cancelling a running statement, also on TLS connections (#53), and LOB
  queries that failed once no longer failing with ORA-01008 afterwards,
  #48). Packages no longer depend on libaio.
  JSON and VECTOR columns are shown (the Instant Client driver couldn't
  fetch them). `TIMESTAMP WITH TIME ZONE` values with a region show the
  region's name (`… Europe/Zurich`, #50), and SQL literals of such values
  keep it. For TLS, the connect string can name the wallet folder with a
  private CA's certificate (`?wallet_location=`, #54). Not covered (#13): 11.2 databases, servers that require Native
  Network Encryption, and OS and wallet logins; for those, build with
  `--features pythia-db/thick` against the Instant Client.
- Table editor: renaming a column of an existing table renames it
  (`ALTER TABLE … RENAME COLUMN`), so it keeps its data, keys and indexes,
  instead of adding a new column next to the old one (#38).
- The table editor applies its script in its window (like the other
  forms) and says which statement failed; Open in SQL window is still
  there. It can add foreign keys, and its Null column is now Not null.
- Auto-commit is a setting (Settings → Query, off by default) instead of a
  toolbar toggle per window. It applies to all SQL windows, including those
  already open, but never to production connections, where Commit asks
  first. Session → Toggle Auto-Commit switches the setting and shows its
  state; the status bar still says when it's on.
- SQL windows stay on the connection they were opened on. Moving one to
  another open connection (the picker above the editor and the toolbar's
  connection menu) is off by default and can be turned on in Settings →
  Query. A window without an open connection can still pick one.

### Fixed
- Object browser: another schema's private database links were missing
  on 18c and 21c although their details showed; they're read from
  DBA_DB_LINKS where the user may (#51).
- The grants editor read ALL_TAB_PRIVS only, so a DBA saw none or some
  of the grants on another schema's object. It reads DBA_TAB_PRIVS when
  it can (#47).
- Drop… on a scheduler job in the object browser failed with ORA-00950
  (there is no DROP JOB); it drops the job through DBMS_SCHEDULER (#46).
- A window learned whether a transaction is open, and got the statement's
  DBMS_OUTPUT, only after the statement's result. For a moment after an
  INSERT it showed no transaction (#44).
- Closing a SQL window with an uncommitted transaction (its tab's ×,
  Close Window, Close Other Windows) rolled the transaction back without
  asking. It now asks: Commit and close, Roll back and close, or Cancel
  (#35).
- The object browser, project tree and saved connections list didn't
  highlight the selected row unless the mouse was over it (#31).
- Hiding or showing a result grid column reset the widths of all the
  others; the grid now remembers each column's width.
- Result grid columns could stay as narrow as their header when the grid
  was drawn before the first rows arrived.
- Generate body on a package spec with a schema (`create package hr.pkg`)
  ended the body with `end hr.pkg;`, which doesn't compile.
- The connection picker above a SQL window switched connections with a
  pending transaction, dropping the window's session without asking. Like
  the toolbar's menu, it now offers other connections only after a commit
  or rollback.
- The object browser's Compile and the invalid objects window's recompile
  ran a plain `ALTER … COMPILE`, which takes the session's compiler
  settings and so dropped debug information. They keep each unit's settings
  now (`REUSE SETTINGS`), like `UTL_RECOMP` and automatic recompilation
  already did. The browser's Compile also uses the right statement for
  public synonyms and Java classes.
- Result toolbar in narrow SQL windows: the row count no longer runs into
  the buttons. The buttons get the room they need first; the filter and the
  row count share what's left (truncated, hidden when there's no room, the
  status bar has the count too), and without room for them the buttons show
  icons only, labels as tooltips.
- Charts: with many categories in a narrow chart, the x axis labelled only
  the first one. Labels now thin out evenly (every 2nd, 3rd, … category),
  as many as fit.
- Oracle 11.2 to 21c (#16, tested against 11.2 XE, 18c XE, 21c XE and
  23ai Free). Queries no longer use `FETCH FIRST` or dictionary columns
  that came with 12.1, so find object, completion and go to definition,
  find in source, the reports, the Scheduler jobs and invalid objects
  windows, the Constraints tab, table parts in the browser, schema and
  data compare, the AI's table lookups and the monitor (instance,
  sessions, top SQL, AWR) work on 11.2. What needs 18c says so before:
  *Cancel current SQL* in the monitor, the profiler, restarting a
  sequence. The monitor shows the full release from 18c on.
- Parameter types: `%ROWTYPE` parameters name their table (18c and later
  showed `ROWTYPE`), package record and collection types keep the type
  name, and on 11g a package with a parameterless procedure no longer
  breaks member completion.
- Killing a session that is busy in a call no longer drops the monitor's
  own connection (ORA-00031).
- Import: new column names are cut at 128 bytes, not characters.
- README: which Instant Client supports which database version.

## [0.2.0] - 2026-10-22

### Changed
- Toolbar: follows the focused window. Commit and Rollback are only enabled
  with a pending transaction (and then stand out), never open a session,
  and say when auto-commit is on; Execute becomes Compile in program windows
  and is disabled where nothing runs; disabled buttons say why. New: a
  labelled New SQL button with Open file/Reopen closed, Save, a connection
  selector (switch this window's connection, connect, disconnect), an
  auto-commit toggle and Format.
- Object views: views and code objects open on their DDL (tables still on
  their columns), and the next object of a type opens on the tab you chose
  last. Only the shown tab loads, on a metadata session of its own, so
  views and edit forms no longer wait behind the object browser,
  completion or compiles. A view's columns skip the primary key lookup,
  and its edit form takes WITH READ ONLY / WITH CHECK OPTION from the
  view's text where the database keeps it there (23ai) instead of
  searching all constraints. `RUST_LOG=pythia_db=debug` logs how long each
  metadata query took.
- Completion: the list shows up to 15 suggestions (Settings → Code
  assistance → Visible rows), as many as fit below the line, and opens
  above the line when there is more room there instead of covering it. It
  is as wide as its entries need, up to a limit; long names and details are
  cut with "…" instead of running into each other. A pane next to the list
  shows what hover shows for the selected entry: a column's type,
  nullability and comment, a subprogram's signatures and doc comment, a
  snippet's text.

### Added
- AI assistant (#1). Settings → AI configures several models at once, one
  of them the default, with a Test button: the Anthropic Messages API,
  OpenAI-compatible chat completions, local model servers (Ollama,
  llama.cpp, LM Studio; no key) and command line tools (prompt on stdin,
  answer on stdout). API keys are kept in the OS keychain. A side panel
  (View → AI Assistant) holds the conversation; questions take the focused
  window's connection and statement along. From editors (AI menu, context
  menu, palette): generate SQL from a description with the schema's real
  tables, columns and foreign keys; explain, fix (with the errors) or
  rewrite the statement under the cursor or the selection, then replace it
  or insert the answer's code; analyze the execution plan. "Analyze with
  AI" / "Fix with AI" buttons on plans, run statistics, errors, compile
  errors, top SQL, session details, AWR/ASH reports and traces. Requests
  carry table definitions, keys, indexes, statistics, plans and source;
  result rows and bind values only when allowed in Settings → AI, and each
  answer shows what was sent and what was held back. On the Claude API, a
  request Claude declines is answered by the fallback model Anthropic
  recommends for it.
- Editors: a context menu. On a database object it offers what the object
  browser offers (query, edit, test, describe, edit table, grants, generate,
  compile, favorites, copy name) and Show in object browser; on a local
  name, go to declaration and find references; plus cut, copy, paste,
  select all, toggle comment and format. On a package member (`pkg.proc`)
  Edit and Ctrl+click open the package at that subprogram.
- Invalid objects: the status bar shows how many there are (current schema
  or all user schemas, refreshed on connect, after compiles and DDL and on
  an interval). Clicking it opens a window listing them with their first
  error; it recompiles selected or all objects in dependency order over
  several sessions (Settings → Invalid objects), with progress, cancel and
  what is still invalid afterwards, and marks objects you may not compile.
  SYSDBA connections can use UTL_RECOMP. The browser's recompile button
  uses it too.
- Navigation in packages: in a program window the body knows the spec's
  declarations and the other way round (go to definition, hover,
  completion, references), Ctrl+click on a call picks the overload its
  arguments fit, a named argument (`p_id =>`) leads to the callee's
  parameter, labels (`exit outer_loop`, `goto done`) are navigable, and
  hover shows the comment above a declaration. New commands (palette,
  Settings → Keyboard, editor menu): Back/Forward (Alt+Left/Right, mouse
  buttons), Switch Spec/Body (Ctrl+Shift+B), Go to Symbol (Ctrl+F12), Peek
  Definition (Alt+F12) and Find References (Shift+F12) as a list to step
  through.
- Debugger: step through the debugged block itself (it now has debug
  information), and stepping out of the outermost unit continues there; the
  SQL window marks the block's line.
- Debugger: the call stack names the subprogram (`orders_api.load_order:31`)
  and the package initialization section.
- Formatter: profiles (named rule sets to switch between, export and import
  as TOML to share a team style), learning the rules from the focused
  editor or the current schema's stored source (the differences are shown
  with the preview before you apply them), and formatting automatically on
  save and compile, on paste and when a statement's `;` is typed (only
  what changes is replaced, as one undo step, and the caret stays put).
  New rules: CASE arms, subqueries, parameter lists and INSERT column and
  VALUES lists each fit on one line or always break, and THEN/LOOP on their
  own line. AI: Format Unsupported Statements hands the statements the
  formatter keeps as written (unsupported constructs, failed safety checks)
  to the AI with the same rules; an answer is applied only if it changes
  nothing but whitespace and case, as one undo step.
- Database links: the object view has a Details tab (connects as, host,
  created; another schema's private link from DBA_DB_LINKS where you may
  read it) and Test link, also in the context menu, which queries over the
  link and says whether it works.
- Debugger: selecting a frame in the call stack shows its variables, and
  watches and hover values in its unit come from that frame (its package's
  globals included). Every stop selects the innermost frame again;
  breakpoint conditions are always evaluated where the breakpoint is.
- Debugger: Settings → Debugger → Compile with debug information makes
  program windows compile with debug information, so Step into enters
  those units without a breakpoint in them. Off by default, as Oracle runs
  such code less optimized. Oracle's notes about the option (PLW-06015,
  PLW-06013) aren't listed with the compile messages. Units that already
  have debug information are no longer recompiled before a debug run.
- Debugger: when Step into goes over a unit without debug information, the
  panel says so (for the first call in the session, when the unit is
  loaded; later calls can't be told apart from code without calls).

### Fixed
- Object browser: public database links and public synonyms are listed,
  under a PUBLIC entry in the schema list. Database links are also read
  from ALL_DB_LINKS, for versions whose ALL_OBJECTS leaves them out. Drop
  works for public synonyms and links and for links with dots in their
  name.
- Windows: no console window with log output next to the app (also none for
  `git` and other helper programs). Started from a terminal, the logs still
  show there. Problems at startup are shown in a message box, and About has
  an "Open log folder" button.
- Connect dialog: without the Oracle Instant Client it lists the steps for
  this platform, with the download page as a working link (copying the old
  one picked up a trailing comma and led to a 404) and the Visual C++
  redistributable on Windows.
- Debugger: a step no longer stops again on the line it started from.
- Debugger: a step that ended at a breakpoint no longer stops later, after
  continuing, where the step would have ended.
- Debugger: breakpoints reached while stepping count their hits; stepping
  from a breakpoint doesn't count it twice.
- Debugger: records in index-by tables show their fields, elements are named
  by their key (`('L1')`), and watches accept text keys.
- Debugger: SQL collection types show as not readable instead of NULL.
- Debugger: after pausing in code without debug information (e.g.
  `DBMS_SESSION.SLEEP`), the editor shows the nearest frame with source,
  and the variables and watches are that frame's.
- Debugger: clicking a frame in the call stack didn't open its source.
- Debugger: globals of a package the session hasn't instantiated yet (e.g.
  a spec loaded for its types, whose constants the compiler inlined where
  they are read) read as NULL. Oracle's agent has no values for them until
  the package is first used; the watch now says so. NULL globals of an
  instantiated package still read as NULL.
- Completion: after a short list (e.g. filtered down to three matches),
  later lists no longer stay that small until the app restarts.

## [0.1.0] - 2026-10-22

First release.

### Added

**Application**
- Native egui/wgpu application with an OpenGL fallback, dockable layout,
  command palette (Ctrl+Shift+P), PL/SQL Developer compatible keys, dark and
  light themes, settings window, workspace and editor contents restored on
  start, crash logging.
- Multiple simultaneous connections; every window has its own session and
  transaction. Connection manager with folders, colors and production
  markers, read-only connections, passwords in the OS keychain,
  `tnsnames.ora` aliases.
- Open and save SQL/PL/SQL files, drag and drop files, reopen closed windows.

**Editor**
- Rope-backed code editor: virtualized rendering, multi-cursor, block
  selection, find/replace with regular expressions, bracket and keyword
  matching, auto-indent, comment toggling, case conversion, line moves.
- Error-tolerant PL/SQL parser and language engine: completion of columns
  (aliases, joins, CTEs, inline views), package members, records, locals and
  dictionary objects; signature help, hover, go to definition, references,
  live syntax diagnostics and an outline.
- Code formatter (Ctrl+Shift+F) with configurable rules and a live preview.
- Code folding, numbered bookmarks and snippets with placeholders; folds
  and bookmarks are saved with the workspace.
- Rename a local symbol in place (Shift+F6).
- SQL\*Plus commands get their own highlighting.
- Files keep their encoding (UTF-8, UTF-8 BOM, Windows-1252) and line
  endings.
- Configurable keyboard shortcuts (Settings → Keyboard) and a code font
  from a font file.

**SQL windows**
- Execute the statement under the cursor, the selection or a script;
  SQL\*Plus `exec`, `prompt`, `define`, `desc`; bind and substitution
  variable prompts; DBMS_OUTPUT; statement history.
- Virtualized result grid with sorting, selection, aggregates, record view,
  value viewer and export to CSV, TSV, JSON, Excel, INSERT, Markdown and HTML.
- In-place editing of results with optimistic concurrency checks.
- Explain plan as a tree and "Run with statistics" with the actual plan and
  session statistics.
- Bar and line charts for any result.
- Foreign key navigation: open a row's parent or child rows.
- Hide columns; paste Excel ranges into editable results; edit long text
  and dates (with a calendar) in a dialog.
- Execute a query straight into a file (CSV, TSV, JSON, INSERT, Markdown)
  with progress and cancel.

**PL/SQL development**
- Object browser, object views (columns, constraints, indexes, grants,
  dependencies, DDL), program windows with spec/body and compile errors,
  generated statements, test scripts, recompile invalid objects.
- Object browser: favorites, recently opened objects, filters for invalid
  or recently changed objects, indexes/constraints/triggers/referencing
  tables under tables, drag names into editors.
- Program windows: Update spec copies changed signatures from the body.
- Form editors for sequences, views, synonyms, indexes and grants.
- PL/SQL debugger over DBMS_DEBUG_JDWP: breakpoints, stepping, call stack and
  variables including records and collections.
- Debugger: watches, values on hover, conditional and hit-count
  breakpoints, enabling/disabling breakpoints, break on exceptions.
- Profiler (DBMS_HPROF) for blocks and calls.
- Find in database source with regular expressions.
- Find in source searches views and job actions and filters by kind and
  name; data search finds columns by name.

**Tools**
- Monitoring: dashboard with findings and live charts, sessions with a
  blocking tree, session details, kill/cancel, AWR/ASH reports (when
  licensed) and a tkprof-style trace analyzer.
- Schema compare with structural table diffs and ordered sync scripts; data
  compare by key.
- Import from CSV and Excel with type inference into new or existing tables.
- Import: per-column trim, date format and SQL expressions; skip and log
  bad rows.
- Export tables tool with a WHERE per table, DDL, delimiter and NULL text.
- Project panel: folders under git with status, diff with the last commit
  or with the database, schema export to one file per object, deployment
  of changed files.
- Scheduler jobs window: run, stop, enable, disable, drop, run history.
- Saved user reports with parameters.

[Unreleased]: https://github.com/ortic/pythia/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/ortic/pythia/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/ortic/pythia/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/ortic/pythia/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/ortic/pythia/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/ortic/pythia/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ortic/pythia/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/ortic/pythia/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/ortic/pythia/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/ortic/pythia/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ortic/pythia/releases/tag/v0.2.0
[0.1.0]: https://github.com/ortic/pythia/releases/tag/v0.1.0
