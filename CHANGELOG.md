# Changelog

All notable changes to Pythia are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed
- SQL Server: `money` and `smallmoney` show every digit (#186): read as
  a double, 922337203685477.5807 showed as 922337203685477.6, and an
  edit of such a value found no row. They show with their four decimals,
  as SQL Server writes them.
- SQL Server: an ADO.NET connection string with a non-ASCII character
  (`Database=Bücher`, a password `Grüße`) connects (#414); it was
  refused whole.
- SQLite: the Triggers tab shows a trigger's event right when the
  trigger's name is an event word itself (#386): `create trigger
  "delete" after insert …` showed as `DELETE AFTER INSERT`. A
  `[bracketed]` name is unquoted too.
- MySQL: a column's default expression with a string in it shows as
  SHOW CREATE TABLE writes it (#387): `default ('it''s')` showed as
  `_utf8mb4\'it\\\'s\'`, escaped once more as `information_schema`
  gives it.
- MySQL and SQL Server: *Edit* works on a result of a temporary table
  (#388), which it refused as having no key: the key came from
  `information_schema` on MySQL, which lists no temporary tables, and on
  SQL Server from the database instead of `tempdb`. On MySQL, a
  temporary table that hides a table of its name got that table's key.
- SQL Server: a `numeric` or `decimal` of scale 0 shows as `123`, no
  longer as `123.0` (#389), in the grid and in exports.
- MySQL: *Edit* on a query whose select list ends with a `#` comment
  deleted the wrong rows (#390). The key columns it adds went into the
  comment, and the result's last column was taken for the key: a Delete
  removed the row whose key was that column's value. The select list is
  now read with the database's comments (and SQL Server's `#temp`
  names), and a result is editable only when it ends with the key
  columns that were added.
- PostgreSQL and SQL Server: block comments nest, as the servers read
  them (#391). In `/* off /* step 2 */ delete from t; */`, Pythia ended
  the comment at the first `*/` and ran the `delete`, which the server
  would have left commented out. The editor highlights it as comment too.
- The value editor shows a binary value the grid holds cut short (its
  first 4000 bytes) read-only (#392), with *Save to file…* and *Load
  from file…*. Editing that text was either dropped without a word or,
  with its `… (N bytes)` ending deleted, written as the whole value,
  truncating it. Apply says how many cut values (pasted, say) it leaves
  out.
- PostgreSQL: `key=value` connect strings are read as libpq reads them
  (#393): a `?` in a value no longer makes the string a URL, quoted
  values keep their spaces, and `sslmode = require` with spaces is
  taken. An `sslmode` that the driver read but Pythia didn't was turned
  into `prefer`, which falls back to a connection without TLS.
- Script files in UTF-16 (with its byte order mark, as SQL Server
  Management Studio and Windows PowerShell write them) open as text and
  are saved as UTF-16 (#394). They were read as Windows-1252, and an
  edit saved single bytes into them. The encoding in the status bar
  saves the file in another one, which a Windows-1252 file needs for a
  character it can't hold.
- Oracle's forms for materialized views, indexes and database links
  drop the object to create it again for some changes (#395). When the
  create failed (an index made unique over duplicates), the object was
  gone and the next Apply tried the drop again. Now the message says it
  was dropped, and Apply creates it. A materialized view's new query is
  checked before the drop.
- PostgreSQL and SQL Server: a table's DDL runs again when a foreign
  key references the table's own unique index (#396). PostgreSQL's left
  that index out, and both put the key in the CREATE TABLE, before the
  index it needs; it is added after the indexes now.
- PostgreSQL: *Edit* of a constraint trigger opens a script that drops
  and creates it in one block (#397); the plain CREATE it opened failed,
  the trigger being there, and PostgreSQL has no OR REPLACE for one.
- PostgreSQL: an array slice's bounds (`a[1:n]`, `a[1:2]`) are no bind
  variables (#398): the window asked for a value of `n`, and Explain
  plan sent `a[1$1]`, a syntax error.
- MySQL and PostgreSQL: a `:name` in a string with `\'` (or `E'…'`) is
  no longer replaced by a bind variable's value (#399), which broke the
  statement; binds are put in as the database reads its strings.
- MySQL: Explain plan of a statement with bind variables says it takes
  none (#400), instead of a syntax error from the server.
- Oracle: *Follow foreign keys* keeps the columns of a key together when
  tables of the same name in two schemas have keys of the same name
  (#401); each became one relation per column, whose link found too many
  rows. A table in another schema is named with it.
- MySQL: the user form sets the password typed when it has a `\` and
  the session's `sql_mode` has `NO_BACKSLASH_ESCAPES` (#402); the
  backslash was doubled, which that mode keeps.
- MariaDB: the user and role forms can grant `DELETE HISTORY` on a
  database or a table (#403): those scopes' privileges add what the
  server lists for tables.
- A `--` comment at the end of a data compare's filter or query, a
  trigger form's WHEN or an import column's expression no longer
  comments out what follows it (#404), which failed with ORA-00907 or
  the like.
- A new row's cell set to NULL (*Set NULL*, Delete) is inserted as NULL
  (#405); it got the column's default, as a cell left alone does.
- MySQL and SQL Server: an address with `user:password@` in it is
  refused with a message saying where they go (#406); it was taken for
  the host, and the connect error showed the password.
- PostgreSQL: `money` sorts by its amount, and the selection's summary
  and charts count it (#407); written as `$1,234.56`, it sorted as text
  and was left out.
- Export tables and Export schema write a `.` in an object's name as
  `%2E` in its file's name (#408): `"X.DDL"`'s rows went to
  `x.ddl.sql`, over the DDL of table `X`.
- PostgreSQL: a foreign table's DDL makes a foreign table, on its server
  with its and its columns' options (#409); it was a plain CREATE TABLE.
- SQL Server: a table's DDL keeps the descriptions (`MS_Description`) of
  the table and its columns (#410), as `sp_addextendedproperty` calls.
- A SQL window's file that changed on disk (a `git pull` while Pythia
  was closed) is read again when Pythia starts, unless the window has
  changes of its own; saving over such a file asks first (#411). The
  window restored its old text, and Save wrote it over the change.
- PostgreSQL: a table's DDL keeps its row level security (#373): `alter
  table … enable row level security` and `force row level security`
  where they are on, and a `create policy` for each policy, with its
  command, roles, `using` and `with check`, restrictive or not. Run
  elsewhere, the copy had neither, so every role that could read it read
  every row, and rows no policy let through could be written to it.
- PostgreSQL: a table's DDL keeps its row level security (#373): `alter
  table … enable row level security` and `force row level security`
  where they are on, and a `create policy` for each policy, with its
  command, roles, `using` and `with check`, restrictive or not. Run
  elsewhere, the copy had neither, so every role that could read it read
  every row, and rows no policy let through could be written to it.
- PostgreSQL: a table's DDL gives each column its collation where it
  isn't its type's (#375), `code text collate "C"`, with its schema
  where it isn't `pg_catalog`. Run elsewhere, the copy's columns took
  their type's collation, mostly the database's, and sorted and compared
  otherwise: the copy of a column collated `"C"` sorted `a` before `B`.
- SQL Server: an `nvarchar`, `nchar` or `ntext` value that isn't valid
  UTF-16 reads with U+FFFD in its place (#371). SQL Server doesn't check
  them, as in the names of incremental statistics' objects in
  `sys.all_objects`: the result failed with "Protocol error: nvarchar:
  invalid UTF-16 sequence", and the connection was opened again, which
  rolled back the transaction.
- SQL Server: an `nvarchar`, `nchar` or `ntext` value that isn't valid
  UTF-16 reads with U+FFFD in its place (#371). SQL Server doesn't check
  them, as in the names of incremental statistics' objects in
  `sys.all_objects`: the result failed with "Protocol error: nvarchar:
  invalid UTF-16 sequence", and the connection was opened again, which
  rolled back the transaction.
- SQL Server: a table's DDL and *Columns* name a user-defined type, an
  alias or a CLR type, with its schema (#372), `[app].[ssn]`. By its name
  alone, the DDL failed with Msg 2715 where the session's default schema
  isn't the type's.
- The program window finds a package's quoted subprograms by their
  names (#359). *Edit "MyProc"* opened the package at the top, saying
  `"MyProc" not found`, and *Test…* and *Debug…* of it from the
  program window called `"""MYPROC"""` without its parameters, so the
  block failed and the debugger didn't stop. `"MyProc"` and `"myproc"`
  were one subprogram: overloads of each other for *Test…*, and *Go to
  body* went from one to the other; `"PLAIN"` in the spec wasn't
  `plain` in the body. *Test…* of `"MyProc"`, from the object browser
  too, also took the parameters of a `"myproc"`.
- The program window finds a package's quoted subprograms by their
  names (#359). *Edit "MyProc"* opened the package at the top, saying
  `"MyProc" not found`, and *Test…* and *Debug…* of it from the
  program window called `"""MYPROC"""` without its parameters, so the
  block failed and the debugger didn't stop. `"MyProc"` and `"myproc"`
  were one subprogram: overloads of each other for *Test…*, and *Go to
  body* went from one to the other; `"PLAIN"` in the spec wasn't
  `plain` in the body. *Test…* of `"MyProc"`, from the object browser
  too, also took the parameters of a `"myproc"`.
- Forms keep the case of a name they pick or read (#365). The column
  `"Mixed"` picked for a trigger's *Update of* read `"mixed"`, and the
  trigger failed with ORA-00904; a job's list of procedures offered
  `"MyPkg"."Run"` as `mypkg.run`, which the job ran as `MYPKG.RUN`; an
  index's tablespace `"my_ts"`, picked or read by *Edit*, became `MY_TS`;
  and *Edit* of a database link made again with its user `"hr_ro"` used
  `HR_RO`.
- The program window finds a package's quoted subprograms by their
  names (#359). *Edit "MyProc"* opened the package at the top, saying
  `"MyProc" not found`, and *Test…* and *Debug…* of it from the
  program window called `"""MYPROC"""` without its parameters, so the
  block failed and the debugger didn't stop. `"MyProc"` and `"myproc"`
  were one subprogram: overloads of each other for *Test…*, and *Go to
  body* went from one to the other; `"PLAIN"` in the spec wasn't
  `plain` in the body. *Test…* of `"MyProc"`, from the object browser
  too, also took the parameters of a `"myproc"`.
- Forms keep the case of a name they pick or read (#365). The column
  `"Mixed"` picked for a trigger's *Update of* read `"mixed"`, and the
  trigger failed with ORA-00904; a job's list of procedures offered
  `"MyPkg"."Run"` as `mypkg.run`, which the job ran as `MYPKG.RUN`; an
  index's tablespace `"my_ts"`, picked or read by *Edit*, became `MY_TS`;
  and *Edit* of a database link made again with its user `"hr_ro"` used
  `HR_RO`.
- Oracle: DESCRIBE, *Compare data* and *Import* take a quoted name as
  written (#369). `desc "MyTable"` described nothing, as it compared
  `"MYTABLE"` with its quotes; a data compare of the table `"my_table"`
  or with the key `"Id"` failed with ORA-01741; and Import into
  `"My.Table"`, a dot in its quotes, failed with ORA-00922. Names
  without quotes are still read in uppercase, and `owner.table` is split
  at the dots outside quotes only.
- The program window finds a package's quoted subprograms by their
  names (#359). *Edit "MyProc"* opened the package at the top, saying
  `"MyProc" not found`, and *Test…* and *Debug…* of it from the
  program window called `"""MYPROC"""` without its parameters, so the
  block failed and the debugger didn't stop. `"MyProc"` and `"myproc"`
  were one subprogram: overloads of each other for *Test…*, and *Go to
  body* went from one to the other; `"PLAIN"` in the spec wasn't
  `plain` in the body. *Test…* of `"MyProc"`, from the object browser
  too, also took the parameters of a `"myproc"`.
- Forms keep the case of a name they pick or read (#365). The column
  `"Mixed"` picked for a trigger's *Update of* read `"mixed"`, and the
  trigger failed with ORA-00904; a job's list of procedures offered
  `"MyPkg"."Run"` as `mypkg.run`, which the job ran as `MYPKG.RUN`; an
  index's tablespace `"my_ts"`, picked or read by *Edit*, became `MY_TS`;
  and *Edit* of a database link made again with its user `"hr_ro"` used
  `HR_RO`.
- Oracle: DESCRIBE, *Compare data* and *Import* take a quoted name as
  written (#369). `desc "MyTable"` described nothing, as it compared
  `"MYTABLE"` with its quotes; a data compare of the table `"my_table"`
  or with the key `"Id"` failed with ORA-01741; and Import into
  `"My.Table"`, a dot in its quotes, failed with ORA-00922. Names
  without quotes are still read in uppercase, and `owner.table` is split
  at the dots outside quotes only.
- Oracle: names are folded to uppercase as Oracle folds them, letter by
  letter (#380). Import made a header `Café` the column `"CAFé"`, which
  `café` doesn't name, and didn't map it to an existing table's `CAFÉ`;
  a *Schema* or *Default schema* typed `straße` named `STRASSE`, where
  Oracle reads `STRAßE`.
- Oracle: a query with PL/SQL declarations, `with function f … end;
  select f from dual`, runs as one statement (#358), also in an INSERT,
  a view and an UPDATE's or DELETE's subquery (`/*+ with_plsql */`).
  The script ended it at the first `;` inside the function, which failed
  with ORA-02000, and ran the rest on its own. It ends at the `;` after
  the query, or at `/` on a line of its own as in SQL*Plus; declarations
  still open at the end of the script, as while they are typed, end at
  that first `;` as before, so the statements after them stay apart.
  Format Code keeps such a query as written.
- Oracle: a query with PL/SQL declarations, `with function f … end;
  select f from dual`, runs as one statement (#358), also in an INSERT,
  a view and an UPDATE's or DELETE's subquery (`/*+ with_plsql */`).
  The script ended it at the first `;` inside the function, which failed
  with ORA-02000, and ran the rest on its own. It ends at the `;` after
  the query, or at `/` on a line of its own as in SQL*Plus; declarations
  still open at the end of the script, as while they are typed, end at
  that first `;` as before, so the statements after them stay apart.
  Format Code keeps such a query as written.
- Search data with Exact finds a value in each column that can hold it
  (#363). A number past NUMBER's range (`1e300`), `inf`, `nan` or a day
  no month has (`2024-02-30`) failed the query of every table with a
  number or date column, and a table with a CLOB or NCLOB failed for any
  value: their hits went unfound, text columns too. `-2e-300` found the
  zeros. Each column now gets a literal of its type: a BINARY_DOUBLE or
  BINARY_FLOAT `1e300d` or `binary_double_nan` as since #339, a NUMBER
  only a number it holds, a date only a day Oracle reads (1500-02-29
  is one, by the Julian calendar); a CLOB is compared with
  `dbms_lob.compare`.
- Oracle: a JSON's dates, timestamps, intervals, binary values, IDs and
  binary numbers show as Oracle's extended objects and keep their types
  (#370): `{"$oracleDate":"2020-01-01T00:00:00"}`,
  `{"$oracleTimestampTZ":"2020-01-01T10:00:00+02:00"}`,
  `{"$intervalDaySecond":"P1DT2H"}`, `{"$numberDouble":1.5}`, a binary
  value in hex as `{"$rawhex":"ABCD"}`. They showed as JSON strings and
  numbers, without a time zone, NaN as `NaN`: changing such a JSON
  reported a change by another session, and its INSERT statements wrote
  the strings and numbers. Written, each such object is the scalar it
  stands for, typed into a new row too; other objects, as MongoDB's
  `{"$oid":…}`, stay objects. A changed JSON keeps the scalars it had
  where they are as they were, also where the type shown is a guess: the
  pure-Rust driver reads a DATE, a TIMESTAMP and a TIMESTAMP WITH TIME
  ZONE in UTC in a JSON alike, so one without a fraction of a second
  shows as `$oracleDate`, one with as `$oracleTimestamp`.
- SQL Server: a ledger table's history table's DDL says that its ledger
  table's DDL makes it (#381). It was a plain table's, which, run before
  the ledger table's DDL, made that fail with Msg 37355, and run alone
  made a table no ledger keeps.
- SQLite: windows of a database in memory (`:memory:`, a `file:` URI of
  memory, `cache=shared`) wait for each other's locks as windows of a
  file do (#374): up to five seconds, then they say which window holds
  it. A SELECT or INSERT of a table another window had changed and not
  committed failed at once with "database table is locked", and while a
  window had uncommitted DDL, the object browser and completion came up
  empty. Lookups answer as they did last while the schema is locked.
- PostgreSQL: a trigger's DDL keeps it disabled, or enabled for replicas
  or always, where it is so (#415), and *Edit* keeps that state:
  replaced or made again, it fired as an ordinary trigger.
- PostgreSQL: *Edit* of a trigger whose name is on several tables
  replaces each of them (#377); the script replaced the first and failed
  at the second, "already exists". With a constraint trigger among them,
  the whole script is one block.
- PostgreSQL: a child table's DDL inherits from its parents, with only
  its own columns and constraints (#376); a copy was a table of its own,
  which a query on its parent's copy didn't read. (Foreign tables: #409.)
- SQL Server: a result of a query with `OUTER APPLY`, `PIVOT` or
  `UNPIVOT` can't be edited (#416); a value typed into a column they
  bring in was written to the table of the query.
- *Edit* no longer finds a row by a floating-point key (#417), whose
  rounded text matched its neighbours: deleting the MySQL `FLOAT` row
  shown as `1` deleted 1.000001 and 1.000005 too. Float columns (and
  SQLite columns of REAL or BLOB affinity) aren't keys now; another
  unique key or SQLite's rowid finds the row, or the result is
  read-only. *Save to file…* refuses a key that finds several rows.
- Editor: Find/Replace *In selection* reads the text around the
  selection (`^`, `$`, `\b`) as `$1` does, and follows the selection
  through replacements (#418). A raw `$1` went in at the selection's
  edges, and *Replace all* that shortened the text could crash Pythia.
- The formatter no longer renames names (#419): on MySQL, tables named
  like keywords (`User`, `Session`) were lowercased with them, another
  table on Linux; and `straße` became `STRASSE` with identifier case
  Upper. Only ASCII letters change case, and on MySQL only reserved
  words.
- MariaDB: *Edit* of a trigger created with `IF NOT EXISTS` opens a
  script that runs (#420); it failed with "Incorrect usage of OR REPLACE
  and IF NOT EXISTS".
- MySQL and MariaDB: a trigger's DDL keeps its place among its table's
  triggers (`PRECEDES`, #421), and a trigger's or routine's the
  `sql_mode` it was made under (#422). *Edit* made it fire last, and
  under the window's mode, where `||` may mean OR.
- PostgreSQL: Commit of a transaction that failed says nothing was
  committed (#423). After a failed savepoint statement (`release
  savepoint` with a typo) PostgreSQL had rolled it back, and Commit
  said "Committed".
- A window whose new session couldn't connect (the server still
  restarting) answers its next statement (#424); it showed "running" for
  ever. A failed attempt to open a lost main session again no longer
  closes the connection with every window's session: the next request
  tries again.
- SQLite: a text with a NUL character can be changed in a result and
  written as INSERT statements (#425); its literal ended at the NUL.
- Oracle: *Follow foreign keys* over a key with a DATE or TIMESTAMP
  column opens the related rows (#426); its query wrote the date as a
  string, ORA-01861.
- Export tables and data compare take a filter that is all comment as
  none (#427); `where -- id = 6` failed with ORA-00936.
- The value editor's calendar keeps a date without a time so (#428):
  it added ` 00:00:00`, which SQLite kept, so the row no longer equalled
  the date.
- PostgreSQL: a table's and a function's grants of one name stay apart,
  and a function's DDL leaves out a procedure of its name (#429).
- The workspace is saved while a window's file has a name that isn't
  UTF-8 (Linux) (#430); it wasn't saved at all then, and every window's
  unsaved text was lost with the next restart.
- A failed statement that took the transaction with it (SQLite after a
  Break of a change, MySQL after a deadlock) says that the earlier
  changes are undone (#431); only the status bar showed it.
- MySQL and MariaDB: an event with a `DO BEGIN … END` body and
  MariaDB's aggregate function are one statement up to `/` (#432); they
  were cut at their first `;`.
- SQLite: a trigger whose body uses columns named `begin` or `end` is
  one statement (#433); it was split at such a name, or swallowed the
  rest of the script.
- An Excel export that passes Excel's limits (32,767 characters a cell,
  1,048,576 rows a sheet) says which row and column, and to export as CSV
  or JSON (#434); it failed with the library's error only.
- Oracle: a numbered substitution variable, `&1`, is substituted in code
  too, not only in strings (#435).
- MySQL: a `VECTOR` is shown as the hex of its bytes, not as garbled
  text, and Edit changes rows that have one (#436).
- Oracle: Find in Source finds text past the first 4000 bytes of a view
  (#437).
- Find in Source searches the text as typed, blanks around it too
  (#438).
- A chart's tooltip shows a value below 0.0001 with its exponent, not as
  0 (#439).
- SQL Server: a trigger's DDL keeps its first or last place among its
  table's triggers, which *Edit* dropped (#440).
- MySQL: Maintenance's facts of a table and the monitor's storage figures
  are current, not as MySQL cached them up to a day ago (#441).
- PostgreSQL: Maintenance's facts of a partitioned table are its
  partitions' rows, size and transaction ID age, not 0 and a false
  wraparound warning (#442).
- Oracle: Export Tables and Data Compare read a table's invisible columns
  too (#444), and leave out its virtual columns where they write
  INSERTs and UPDATEs, which Oracle refused (#443).
- MariaDB: the role editor shows a role's privileges on the server
  (`*.*`), which MariaDB's `information_schema` doesn't list (#445).
- Import reads CSV files in Windows-1252 and UTF-16 (with its byte order
  mark), as the editor opens them, not as UTF-8 that lost their accented
  characters (#446).
- MySQL: completion offers a table's own columns where another table's
  name differs from it only in case, not both tables' (#447).
- SQL Server: `END CONVERSATION` inside a `BEGIN … END` block doesn't end
  the block, which a script was cut at, the rest running outside it
  (#448).
- SQL Server: dropped ledger tables, their history tables and views,
  which SQL Server keeps under names of its own, aren't listed or found
  (#382).
- PostgreSQL: a table's DDL has the comments on its indexes and its
  constraints, a materialized view's on its indexes (#379).
- PostgreSQL: a table's DDL has its rules, its replica identity and its
  columns' storage, compression, statistics targets and options, a
  materialized view's its columns' (#378).
- SQLite: a window with rows left to fetch doesn't keep another from
  committing: it reads the rest ahead, up to 256 MB, which lets its lock
  on the file go (#185).
- SQLite: text that isn't UTF-8 shows, copies and exports as Windows-1252,
  not with replacement characters (#413).
- MySQL: with `NO_BACKSLASH_ESCAPES` in the session's `sql_mode` (the
  server's default, the login script or a `SET`), a SQL window reads a
  `\` in a string as itself: `'C:\'` no longer runs on to the end of the
  script, in splitting, highlighting and binds (#412).
- Edit leaves a query over tables joined with commas read-only also when
  an alias is written with `AS` or is a keyword, or a table has SQL
  Server's `WITH (NOLOCK)`: another table's cells were written to the
  first table (#449).
- SQL Server: a renamed or moved view, routine or trigger has DDL under
  its name of now, and a trigger under its table's: *Edit* made it again
  under its old name, beside the renamed one (#450).
- PostgreSQL: a `SET client_encoding` to anything but UTF8 is undone
  with a notice, and a value that isn't UTF-8 fails its statement: the
  window hung for ever at the first non-ASCII value, and text written
  was stored garbled (#451).
- MySQL: sessions use the server's utf8mb4 collation, not the driver's
  `utf8mb4_general_ci`, which a column's didn't mix with (1267), and
  *Edit* makes a routine or trigger again under its own collation (#452).
- MySQL: `WITH … UPDATE` and `WITH … DELETE` wait for Commit, as UPDATE
  and DELETE do: they were committed at once (#453).
- SQL Server: a batch that starts with a SELECT, PRINT or USE and changes
  rows further on waits for Commit: it was committed at once (#454).
- A read-only connection refuses, and a production connection confirms,
  statements that start like a query but change or lock: `WITH … DELETE`,
  `SELECT … INTO`, a T-SQL batch with a DELETE after its SELECT,
  `SELECT … FOR UPDATE` (#455).
- Import into an existing table's text columns takes the file's values
  as they are: a phone number, `1.10` or a date in text were converted as
  numbers and dates (#456).
- A result's INSERT statements leave out the columns its table computes
  (generated and computed columns, rowversion) and say when an identity
  takes the values written (`OVERRIDING SYSTEM VALUE`, `SET
  IDENTITY_INSERT`); Edit keeps those columns read-only: every database
  refused the values (#457).
- Oracle: the table editor's copy and Schema Compare make a virtual column
  as one (#458), an invisible one too, keep `DEFAULT ON NULL` and an
  identity's `GENERATED ALWAYS` (#459), and write NVARCHAR2 and NCHAR
  without `CHAR`, which Oracle refused (#460).
- SQL Server: a numeric or decimal sequence's DDL keeps its precision,
  without which it failed (Msg 11708) (#461).
- MySQL: taking a default role's admin option away in the user form
  keeps it enabled at login, which MySQL's REVOKE ended (#462).
- MariaDB: a new password in the user form keeps the account's other
  ways to log in (`unix_socket OR mysql_native_password`), which it
  replaced with the password alone (#463).
- A read-only connection refuses changes everywhere, not only in SQL
  windows: the browser's Drop and other actions, object forms, the table
  editor, Import, Maintenance, recompiles, job actions and the monitor's
  kills ran (#464).
- SQLite: Edit of a table without a primary key checks a row's values
  beside its rowid, which VACUUM renumbers: a stale result's Delete took
  another row (#465).
- Oracle: the index form makes an index again for new columns with its
  table's owner, compression, REVERSE, INVISIBLE and LOCAL partitioning,
  which it lost, and doesn't make domain or globally partitioned indexes
  again (#466).
- Oracle: Schema Compare and the table editor keep an index's kind
  (bitmap), compression, REVERSE, visibility and LOCAL partitioning, and
  compare function-based and descending indexes, which they made plain
  or left out (#467).
- PostgreSQL: a dollar quote's tag may have letters past ASCII
  (`$função$`), where a script was split inside its body (#468).
- The editor offers PL/SQL snippets (`func`, `proc`, `begin`, …) in
  Oracle's windows only, not in other databases', which refused them
  (#469).
- PostgreSQL: a partitioned table's DDL makes its partitions too, which a
  copy needed to take any row (#470).
- Generate INSERT and UPDATE leave a table's generated columns out, which
  the database refused, and the Columns tab says which are generated, and
  how, on every database (#471).
- SQL Server: the DDL of a view, routine or trigger has its descriptions,
  and its columns' and parameters', which a copy lacked (#472).
- SQL Server: a lookup chosen as a deadlock's victim (Msg 1205), as the
  catalog deadlocks with other sessions' DDL, runs again (#473).
- MySQL: Edit finds a row by a TIMESTAMP key by its instant: in the hour
  that repeats when the clocks go back, two rows showed the same, and the
  other one was changed (#474).
- Oracle: a TIMESTAMP WITH TIME ZONE in a region shows its abbreviation
  (`Europe/Zurich CEST`) in the hour that repeats when the clocks go back,
  and INSERT statements and Edit write it: read back without it, it was
  an hour off (#481).
- PostgreSQL: sessions show and write dates as ISO, whatever DateStyle a
  server, database or role sets: INSERT statements wrote `04/03/2024`,
  which another server read otherwise (#475).
- MySQL: the Constraints tab names a foreign key's referenced table with
  its database where that isn't the table's (#477).
- Oracle: the Columns tab says how an identity column is generated
  (`generated always as identity`), not its sequence's NEXTVAL (#485).
- Generate INSERT leaves out identity columns that take no value
  (Oracle's and PostgreSQL's ALWAYS, SQL Server's), and Generate UPDATE
  keeps a generated key for its WHERE (#490).
- Oracle 23ai: a VECTOR column keeps its dimensions and format in table
  copies, Schema Compare and the Columns tab, which read it as `VECTOR`
  (#486).
- Oracle: the table builder refuses a foreign key on a new temporary
  table before anything runs; Oracle refused it after the table was made
  (#489).
- MySQL: the Columns tab shows `auto_increment`, `on update
  CURRENT_TIMESTAMP` and a spatial column's SRID (#483).
- The result grid's filter finds what the cells show: NULLs by the text
  the grid writes for them, binary values with their `0x` (#478).
- Pythia starts while a window's file is on a share that doesn't answer:
  the windows' files are checked for changes on a thread of their own,
  after the window is up (#479).
- PostgreSQL: a partitioned table's partitions are listed among the
  tables, in the object browser and completion, and say whose partitions
  they are; a partition's DDL makes it a partition of its table again
  (#488).
- PostgreSQL: a query of text in a SQL_ASCII database that isn't UTF-8
  says why it fails and how to read the text: `column::bytea` shows its
  bytes, `convert_from(convert(column::bytea, 'WIN1252', 'UTF8'),
  'SQL_ASCII')` reads them as Windows-1252 (#487).

## [0.11.0] - 2026-11-02

### Added
- SQLite, experimental (#179): *Type: SQLite* with the database file
  (*…* picks one, *New…* makes one), built in, so there's nothing to
  install. SQL windows with results, fetching on demand, Break, explain
  plan (`EXPLAIN QUERY PLAN`) and completion; transactions as with
  Oracle, each window on its own connection to the file. The object
  browser lists tables, views, indexes and triggers, attached databases
  as schemas, and object views show columns, indexes, constraints,
  triggers, dependencies and the DDL. Results of one table can be edited
  in place (#184), rows found by the primary key, a unique index on NOT
  NULL columns or the `rowid`. Scripts end statements at `;` and keep a
  trigger's `BEGIN … END` whole. Maintenance runs `ANALYZE`, `REINDEX`
  and integrity checks; the scan finds free pages for `VACUUM` and
  indexed tables without statistics. A path that doesn't exist fails
  rather than making an empty database (`?create` makes one, `?ro` opens
  it read-only).
- *Execute to file* on MySQL, PostgreSQL, SQL Server and SQLite (#180):
  the window fetches the result batch by batch into the file, as on
  Oracle with the row count while it runs and Break, which removes the
  incomplete file. INSERT statements are written in the database's SQL.
- PostgreSQL and SQL Server: results can be edited in place, as on MySQL
  (#181). *Edit* adds the table's primary key (or a unique key on NOT NULL
  columns) to the end of the result, read-only, also when the query
  doesn't select it. The key is that of the table the query read, found by
  the search path or the default schema, with quoted names (`"My Table"`,
  `[dbo].[x]`) and, on SQL Server, `db.dbo.x` and `db..x`; a table of the
  same name in another schema isn't taken for it. Views and tables without
  such a key stay read-only and say why. Applying checks the values that
  were fetched, each type the way it can be compared: on PostgreSQL json,
  xml, points, arrays and floats as the text that was shown (also when
  `extra_float_digits` rounds it); on SQL Server `text`, `ntext` and `xml`
  as `nvarchar(max)`, `image` and CLR types (`geometry`, `hierarchyid`) as
  their bytes, and a `real` as a real. A `sql_variant` isn't checked, and
  isn't used as a key. The database fills in an identity or serial left
  empty in a new row. On SQL Server, rows affected are the statement's own
  also with `SET NOCOUNT ON` or a trigger that changes other rows.
- *Save to file…* writes a binary or long text value of a result to a
  file (#293), from the cell's context menu or from *View value…*: the
  bytes as they are, text as UTF-8, on a helper thread. The dialog
  suggests the column's name with `.bin` or `.txt`, or the extension the
  first bytes say (`.png`, `.jpg`, `.gif`, `.pdf`, `.zip`, `.gz`). The
  grid holds a binary value past 4000 bytes cut short (#204): a result
  being edited fetches the value again whole by its row's key, a result
  that isn't saves what the grid holds whole and asks to be edited for
  the rest. A change not applied is saved as it is. The columns: Oracle's
  BLOB, RAW, LONG RAW, CLOB, NCLOB and LONG; MySQL's BLOB, BINARY,
  VARBINARY and TEXT; PostgreSQL's bytea and text; SQL Server's binary,
  varbinary, image, varchar, nvarchar, text and ntext; SQLite's blob and
  text columns.
  *Load from file…*, in the context menu and the value editor of a result
  being edited, makes a file the cell's pending change: its bytes in a
  binary column, its text in a text column, which takes UTF-8 only. The
  cell shows it by its size, Ctrl+Z takes it back, and *Apply* binds it
  whole rather than writing it as a literal, which Oracle refuses past
  4000 characters and which is impractical for megabytes anywhere. Files
  up to 256 MB load. On Oracle, *Apply* also binds a value typed that no
  literal holds: over 2000 bytes in a BLOB, RAW or LONG RAW (ORA-01704),
  over 4000 bytes of text in a LONG (ORA-64203), and a CLOB's or NCLOB's
  over 200,000 characters, near where its literal of parts (#317) fails
  with ORA-01489.

### Changed
- `&name` substitution variables are SQL\*Plus's, so only Oracle's SQL
  windows have them unless `SET DEFINE ON` turns them on (#190). On
  MySQL, PostgreSQL, SQL Server and SQLite, `'A &amp; B'` or a URL with
  `&b=2` asked for a value of `amp` or `b`, and changed the text that
  went to the database. `SET DEFINE ON` and `SET DEFINE OFF` are carried
  out by the window on every database; a window that had `SET DEFINE OFF`
  keeps it.
- On SQL Server, ASCII text for a `varchar` or `char` column is written
  as `'…'` instead of `N'…'`, in INSERT exports and when results are
  edited (#182). Compared with `N'…'`, SQL Server converts the column, and
  under a SQL collation (the default on US English servers) a key lookup
  then scans the index instead of seeking. Other text keeps the `N`.
- The misestimate note of an SQL plan says what it means (#177). "⚠
  Estimate off at 35 INDEX UNIQUE SCAN …: estimated 318 rows (1 per start
  × 318 starts), got 0 (318× fewer)" now reads "⚠ Row estimate off at
  plan line 35 (INDEX UNIQUE SCAN …): estimated 1 row per start × 318
  starts = 318 rows, got none". The 35 is the line's Id in the plan, and
  no rows are no longer called 318× fewer. The details under the plan
  tree are headed "Line 35: INDEX UNIQUE SCAN".
- `WHENEVER SQLERROR EXIT ROLLBACK` rolls back when a statement fails,
  as in SQL\*Plus (#286), on every database: the SQL window stopped the
  script and left its changes pending. `WHENEVER SQLERROR EXIT` without
  `ROLLBACK` still stops without ending the transaction.

### Fixed
- INSERT exports on SQL Server wrote a `float` like 1e300 out in full
  (#182), 301 digits, which SQL Server can't read (a decimal literal has
  at most 38 digits), and a `geometry` or `hierarchyid` value as text.
  Such a float now has an exponent, and CLR types are written as the
  `0x…` they are shown as.
- MySQL scripts are read the way MySQL reads them (#187, #188, #189). A
  backslash escapes the next character in a string, so `'it\'s; here'`
  (as `mysqldump` writes it) no longer splits the script in two, and a
  `:name` or `&` in it is no longer taken for a variable. `#` starts a
  comment, so a `;` or `'` in one no longer splits or swallows the
  script. `--` starts one only before a space, tab or line break:
  `select 1 --comment` showed `--comment` as a comment, while MySQL read
  `1 - -comment` and found no column `comment`. The editor highlights,
  folds and matches brackets the same way, and shows MySQL's `"…"` as
  the string it is there.
- Settings → Keyboard no longer takes a key that typing and editing
  need for a command (#191). A shortcut on Delete made Delete run the
  command everywhere: in a SQL window it opened a new window and deleted
  nothing. The same went for Enter, Tab, the arrows, letters with or
  without Shift, and Ctrl with Backspace, Delete, an arrow, A, C, V, X, Y
  or Z. Recording one now says so and waits for another key; such a
  shortcut in `settings.toml` is ignored. Function keys, Alt and the
  other Ctrl shortcuts are free.
- *Export → Excel (xlsx)* keeps a timestamp's fraction of a second
  (#192): `23:09:48.167` became `23:09:48`. It shows as
  `yyyy-mm-dd hh:mm:ss.000`, and a date without a time as `yyyy-mm-dd`.
  MySQL's `DATETIME`, SQL Server's `datetime`, `datetime2` and
  `smalldatetime`, and SQLite's columns declared so are Excel dates
  now, not text (#193). Types with a time zone, and dates before 1900,
  which Excel has no day for, stay text.
- A wrong passphrase for an SSH key file says so (#194): it said
  "SshKey: cryptographic error" (OpenSSH keys) or "Padding error" (PEM
  RSA keys).
- PostgreSQL: *Drop* of a function or procedure with several argument
  lists failed with "function name is not unique" (#195). The browser
  lists such a name once, its source shows every version, and *Drop*
  now drops every version, as the confirmation says.
- Several Pythias open at once no longer undo each other's changes
  (#196). Each wrote its files from what it read at its start: a
  connection saved in one was gone once the other connected from the
  dialog, saved reports likewise, the history was the last one's, and a
  Pythia that exited later reset the settings the other had changed. A
  change now goes to the file as it is (the connect dialog shows the
  other's connections), the history is merged, and settings write only
  what changed. The open windows are the first Pythia's; one started
  while it runs keeps its own as long as it runs. Two saves at the same
  moment no longer share a temporary file, where one of them failed.
- The Variables panel of a SQL window no longer widens by itself
  (#197). With a bind variable such as `:x` it grew on every frame, at
  each caret blink or mouse move, until the editor was gone: its header
  and NULL boxes were wider than the columns it laid out.
- Completion follows changes made through the object browser (#198):
  after a Drop there it still offered the table and its columns, and a
  column added from outside didn't show up after Refresh. The browser's
  actions and Refresh, the object view's Refresh and the object editors'
  changes now make completion read the dictionary again.
- A table's object view no longer goes on showing it after it's dropped
  (#200). *Drop* in the object browser closes the object's views, and
  *Refresh* in a view whose table was dropped elsewhere says there is no
  such table instead of showing an empty column list.
- SQL Server: a statement that took longer than 30 seconds failed with
  "Lost connection to SQL Server: the server did not send the next part
  of the command result within 30s", and the window's changes that
  weren't committed were rolled back (#199). Waiting longer for a lock
  held by another window ended the same way. Statements now run until
  they end or Break stops them; an ADO.NET connection string can still
  set `Command Timeout=n` (seconds).
- MySQL: a statement larger than the server's `max_allowed_packet` was
  taken for a lost connection (#205): "Lost connection to the MySQL
  server: Packet is larger than max_allowed_packet", and the window's
  changes that weren't committed were rolled back, although Pythia
  hadn't sent it. It now fails alone and says so; the session and its
  transaction stay. Editing a long value in a result ran into this
  already with a quarter of `max_allowed_packet` (4 MB of text with a
  backslash on MariaDB's 16 MB): the UPDATE checked the value fetched by
  writing it out again. Values longer than 4 KB are now checked by their
  SHA-256.
- Break, and closing a window with a statement running, froze Pythia
  while the network to the server was gone, until it was back (#207):
  on MySQL, Break connects to the server to send `KILL QUERY`, and the
  window waited for that. Break now returns at once and the interrupt
  goes on its own; one that gets no answer gives up after a few seconds
  (MySQL, PostgreSQL), so Break can be pressed again.
- MySQL: a query's warnings never showed (#210). `select 1/0` said only
  "1 row selected"; now "Warning 1365: Division by 0" follows among the
  messages once the rows are read, as it did for statements without a
  result.
- MySQL: connecting with `?tls` to a server whose certificate isn't
  trusted, such as a self-signed one, failed with "Lost connection to
  the MySQL server: IO error: `invalid peer certificate: UnknownIssuer'"
  (#211). It now says that the certificate isn't trusted and that
  `?tls=skip-verify` connects to a self-signed one; the connect
  dialog's hint mentions it too.
- *Save password* did nothing on a system without a keychain (such as
  Linux without a Secret Service, or WSL), and nothing said so (#214):
  the next connect had no password. The connect dialog now says under
  *Save password* that passwords aren't saved and why, and the log says
  so for each connection whose password couldn't be saved.
- Folders of the object browser could stay "loading" for good after the
  connection was lost and opened again (#215): what the browser had
  asked for and the lost session hadn't answered yet was never
  answered. Those folders now say that the connection was lost before
  they were loaded, and *Refresh* loads them. The same goes for the
  other lookups on the connection, such as completion's.
- PostgreSQL scripts are read the way PostgreSQL reads them (#201,
  #202). A dollar-quoted body may start with a word: `AS $$BEGIN
  RETURN 3; END$$` was split at its first `;` and failed with
  "unterminated dollar-quoted string", and `SELECT $$abc$$ AS e;` ran
  together with the next statement ("cannot insert multiple commands
  into a prepared statement"). A backslash escapes the next character in
  an `E'…'` string, so `E'it\'s; here'` no longer splits the script in
  two. The editor highlights both the same way.
- MySQL scripts know the `mysql` client's `DELIMITER` (#203). A script
  with `DELIMITER ;;` or `DELIMITER $$` around its routines and
  triggers, as `mysqldump` and most tutorials write them, failed with
  "ERROR 1064 … near 'DELIMITER'", and a routine was split at every `;`
  of its body: under `WHENEVER SQLERROR CONTINUE`, the statements of the
  body ran on their own. After a line `DELIMITER x`, statements now end
  at `x` until `DELIMITER ;`, and the line itself isn't sent. `/` on a
  line of its own still ends a routine.
- SQLite: when a view's, an index's or a trigger's statement ends with
  a line comment, its DDL puts the `;` on a line of its own (#206).
  SQLite keeps the statement as written, and the `;` went into the
  comment: *Edit* on such a view dropped the view, and its create ran
  together with the next statement and failed; a table's indexes were
  lost when its DDL ran again.
- Completion offers the columns after the alias of a table quoted with
  backticks or brackets (#208). ``select o. from `order` o`` offered
  nothing on MySQL and SQLite, nor `select t. from [my table] t` on SQL
  Server and SQLite, though completion writes such names itself where
  the database needs them.
- MySQL scripts send the code in `/*!…*/` comments to the server, which
  runs it (#216). `mysqldump` writes its SETs (`/*!40101 SET … */;`),
  partitions and triggers that way: a statement made of such a comment
  was left out, and its `;` went to the server ("Query was empty") or
  ran together with the next statement, so the SETs never ran, a dumped
  trigger was never created and a table lost its partitions. MariaDB's
  `/*M!…*/` stays a comment.
- *Execute to file* writes binary values whole (#204). On MySQL, SQL
  Server and SQLite, a value over 4000 bytes was written as the hex of
  its first 4000 and its size, `89504E47…… (10485760 bytes)`: a 10 MB
  blob came back as 8020 bytes of text, or not at all. Results in the
  grid still hold such a value cut short, and *Export* and *Copy as* now
  refuse it, naming its column, instead of writing part of it; the grid
  shows its real size (it showed 3.9 KB for 10 MB).
- SQLite: blobs are told by their values, not the column's declared type
  (#209). A blob whose hex is all digits (`x'313233'`) was exported as
  the number 313233, typing `1234` into a BLOB cell stored a number, and
  a blob in an `image`, `binary(16)` or untyped column was exported as
  text, showed as text and gave a false conflict when its row was
  edited. Text in a BLOB column no longer shows as `0xhello world`.
- A very long value on one line no longer freezes the result grid
  (#212). A 10 MB value took 1.6 s to appear, on every scroll that
  brought it back too, and 470 MB of memory while it was shown; a cell
  now lays out the first 500 characters of it.
- MySQL: INSERT statements of TIMESTAMP values keep their instant when
  they run in another time zone (#213). They were written as shown in
  the session's zone: `09:00` at `+11:00` came back as `09:00` at
  `+10:00`, an hour later. Like mysqldump, they now set the zone the
  values were shown in while they run (`set @old_time_zone =
  @@time_zone, time_zone = '+11:00';` … `set time_zone =
  @old_time_zone;`), from *Execute to file* and from the grid. A session
  on the server's zone writes its offset when it has the same one all
  year (`+00:00` on UTC).
- JSON exports write booleans as JSON's `true` and `false` (#217). A
  PostgreSQL boolean was the string `"t"` or `"f"`, SQL Server's `bit`
  `"1"` or `"0"`, Oracle's BOOLEAN `"TRUE"` or `"FALSE"`, and a JSON
  reader takes each of them for true.
- Editing a result no longer writes into the wrong column when the query
  renames columns (#218). With `select id, description as name, name as
  title from t`, a change under `name` was written into the column
  `name`, without a word when both held the same value; a change under
  `title` failed (no such column). Only result columns that are the
  table's columns as they are (`col`, `t.col`, from `*`) can be edited
  now; renamed columns and expressions are read-only, their headers
  dimmed, as are the key columns.
- MySQL: a statement in `/*!…*/` is taken for what its code does
  (#219). `/*!40000 INSERT … */` ran without a transaction, so
  *Rollback* didn't undo it; after `/*!40000 ALTER TABLE … DISABLE KEYS
  */`, which commits, the window still showed a transaction to commit;
  mysqldump's `/*!40103 SET TIME_ZONE='+00:00' */` left TIMESTAMPs
  shown, and written into INSERTs, in the zone from before; and a view
  created as mysqldump writes it didn't refresh completion's names.
- MySQL: `?tls` to a server that doesn't offer TLS failed with
  "DriverError { Client requires secure connection but server does not
  have this capability }" (#222). It now says that the server doesn't
  offer TLS and to connect without `?tls`. Other errors of the MySQL
  driver no longer read `DriverError { … }` either.
- The record view no longer freezes on a very long value (#220): a 10 MB
  value was laid out whole, wrapped, on every frame the record showed.
  A field now shows its first 10,000 characters and ` …`; *Copy* still
  copies all of it.
- MariaDB: a TIMESTAMP column's type reads `TIMESTAMP`, no longer
  `TIMESTAMP UNSIGNED`, and an INET6 column's `CHAR`, not `CHAR
  UNSIGNED` (#221). MariaDB flags them unsigned; only numbers are.
- Editing a row with a binary value over 4000 bytes no longer reports a
  conflict on MySQL and SQL Server (#223). The grid holds such a value
  cut short, and applying a change to it compared the cut part with the
  server's value, which never matched. Such a value isn't compared now
  (the row is found by its key), and a cut value pasted into a cell is
  no longer written: MySQL and SQLite stored it as text, 8000 hex
  digits and the size.
- MySQL: connecting to `localhost` no longer fails with "Connection
  refused" where the server listens on 127.0.0.1 only (#239), as MySQL
  and MariaDB from Debian's and Ubuntu's packages do: `localhost` is
  `::1` first on most systems, and only a host's first address was
  tried. The others are tried now, in turn, for any host name.
- *Edit* of a MariaDB routine, trigger or view, and of a MySQL view, no
  longer drops it before the new version is created (#240): when the
  new version failed, a typo in its body say, the object was gone, and
  the drop had revoked the grants on a routine. The script replaces it
  with `CREATE OR REPLACE`, which leaves it as it was when the version
  fails. MySQL can only drop its routines and triggers first; their
  script now says so.
- PostgreSQL: a foreign key to a table of the same name in another
  schema no longer reads as one to the table's own (#226). With
  `tenant.users` and `public.users`, a key of `tenant.note` to
  `public.users` read `REFERENCES users(id)`, by the session's search
  path, in the object view's *Constraints* and in the DDL. A table's
  details now name the tables of other schemas with their schema, and
  its DDL names every schema, as `pg_dump` does: run in another schema
  or with another search path, it no longer makes keys to other tables.
- PostgreSQL 18: a virtual generated column reads as one (#227). `twice
  int generated always as (a * 2)`, virtual by default since 18, showed
  `(a * 2)` as its default in an object view's columns, and the table's
  DDL wrote `twice integer (a * 2)`, which fails (syntax error). It now
  reads `generated always as ((a * 2)) virtual`, as a stored one reads
  `… stored`.
- PostgreSQL: a table's *Indexes* show each index's columns as it has
  them (#228). An index on `(lower(code) desc nulls last, n)` read
  `lower(code), n`, one on `(code collate "C" text_pattern_ops)` read
  `code`, and the INCLUDE columns of a covering index were left out, so
  that `(code) include (name, n)` looked like an index on `code` alone.
  *Columns* now has the order, collation and operator class, and
  *Include* the INCLUDE columns.
- Results of *Query data* can be edited on PostgreSQL (#229). *Edit* on
  `select * from public.orders`, as *Query data* writes it for a table
  in `public`, said "Only results of a single-table query can be
  edited", and so it did for tables named `session`, `comment`, `level`,
  `type` or `mode`, words of Oracle's SQL that the other databases take
  as names. The INSERT statements of such a result (*Copy as*, *Export*,
  *Execute to file*) were written for `my_table`; they now name the
  table.
- The object browser writes a table's columns as the database needs
  them (#230). On PostgreSQL, a column `"Order Date"` dragged into an
  editor became `order date` and `"Id"` became `id`, another column or
  none; columns are now listed and dragged quoted where they need it,
  and on Oracle a quoted name such as `"Mixed"` keeps its case instead
  of becoming `"mixed"`. *Generate → INSERT* and *UPDATE statement*
  named their binds after the columns as they are, so `:order date` or
  `:e-mail` broke the statement, and two columns `Id` and `id` shared
  `:id`; binds are now `:order_date`, `:e_mail`, `:id_2`. *SELECT
  statement* no longer takes a digit for its alias.
- Binary values of SQL Server (`varbinary`, `binary`, `image`), MySQL
  (`VARBINARY`, `BINARY`) and PostgreSQL (`bytea`) show as binary in the
  result grid (#224), as Oracle's and BLOBs did: their start and size in
  the cell, a hex dump in *View value…*. They showed as hex text, without
  their size, and a value over 4000 bytes didn't show that the grid holds
  it cut short. So do SQLite columns declared `MEDIUMBLOB` or `LONGBLOB`.
- *View value…* shows images (#231): a PNG, JPEG, GIF, WebP or BMP
  value showed ⚠ in place of the image, as Pythia had no decoder for
  them. An image viewed in a cell no longer shows again for another
  value viewed there later.
- *View value…* shows a binary value over 4000 bytes as binary (#232):
  its size, a hex dump of the first 4000 bytes, which the grid holds of
  it, and that an image can't be shown from them. It showed the value as
  8015 characters of text, hex digits and `… (51200 bytes)`.
- A CSV or Excel file dropped on Pythia opens an import window only on
  Oracle connections, where *Import Data* is offered (#235). On MySQL,
  PostgreSQL, SQL Server and SQLite, the window opened anyway, showed
  Oracle's `CREATE TABLE` and failed only at *Import* ("This tool isn't
  available for SQL Server connections"); the log now says so at once.
- SQLite: completion offers the columns of an attached database's table
  named without its schema (#236), as SQLite reads such a name: in
  `temp`, `main`, then the attached databases in the order they were
  attached. It looked in `temp` and `main` only and offered nothing.
- The result grid sorts and adds up numbers of any size exactly (#238).
  Numbers past the 15 to 17 digits of a double, such as BIGINT keys or
  `NUMBER(38)` values, sorted out of order (9223372036854775807 before
  …806), and the selection's summary rounded them: "Min
  9223372036854775808." for 9223372036854775806, with a stray dot after
  every number of 1e15 and more. The summary now shows the smallest and
  largest value as they are, and the sum and average exactly.
- *Export* and *Copy all* write a result's rows as the grid shows them
  (#243): those its filter passes, in the order shown. With "12 of 100
  rows" in the result bar, they wrote all 100, in the order fetched,
  while leaving hidden columns out. The message says how many rows were
  exported ("Exported 12 rows to result.csv").
- MySQL scripts end `SET`, `SHOW`, `DESC`, `EXECUTE` and `START
  TRANSACTION` at their `;`, as MySQL does (#225). They ended at the end
  of their line, so what followed on it went to the server with them and
  ran unseen: `select 1; set @x = 3; select @x;` never showed the result
  of `select @x`, and a `SET` over two lines failed. `BEGIN` took in the
  rest of the script, up to a `/`; it now starts a transaction, and
  MariaDB's `BEGIN NOT ATOMIC` still opens a block.
- PostgreSQL: a table opened with *Query* in the object browser can be
  edited, and its INSERT exports name it (#237). The browser writes the
  schema, `select * from public.orders`, and `public`, spelled like a
  keyword, made the query look like one that reads no single table: the
  result had no *Edit*, and *Copy as*, *Export* and *Execute to file*
  wrote `insert into my_table`. The same went for a MySQL table such as
  `shop.user`.
- *Export tables* writes INSERTs into the table as Oracle names it
  (#242): a name that needs quotes is quoted, as the columns were.
  `"guidelines-division"` became `insert into guidelines-division`, a
  syntax error, and `"Foo"` became `foo`, another table. Plain names
  stay in lowercase.
- MySQL: names that are MySQL's reserved words, such as `rank`,
  `system`, `groups`, `change` or `read`, are written in backticks
  (#233). Only Oracle's keywords were, so INSERT exports of a table with
  such a column failed with ERROR 1064 when they ran, as did editing
  that column in a result, and completion inserted the name bare.
- Completion after an opening quote writes the name closed (#234). On
  MySQL, `` `ord `` became ``` ``order` ```, and a name that needs no
  backticks stayed unclosed; `[` on SQL Server and SQLite did the same,
  and after `"` the popup didn't open at all. Columns are offered after
  a quote that isn't closed yet too, and only names are: no keywords or
  functions.
- A large selection in a result grid no longer slows down every frame
  (#241). Its count, sum, average, minimum and maximum were worked out
  again on each frame: with a million rows selected, a frame took a
  quarter of a second, and scrolling and typing lagged. They are now
  worked out when the selection or the rows change.
- A tab's ✕ closes the tab only when it is released on the ✕ (#244).
  Pressed, then released a few points beside it, it closed the tab too,
  so sliding off the ✕ to back out didn't always work.
- Closing a SQL window, or exiting Pythia, asks first when its results
  have changes that weren't applied (#245). A value being typed into a
  cell and the changes waiting for *Apply* were dropped without a word;
  only an uncommitted transaction was asked about. *Discard and close*
  (or *Exit and discard*) goes ahead, *Cancel* keeps them.
- The connect dialog no longer saves a connection over another one
  (#246). *Save* under another saved connection's name replaced that
  connection and its saved passwords; *Connect* after renaming one saved
  it as a new connection next to the old one, "super" next to "Super".
  A name another connection has, in any case, is now refused with a
  message, for *Save* and *Connect*; a connection can still change the
  case of its own name, and *Connect* renames it as *Save* does.
- An *Apply* of result changes that stops part-way, at a value the
  database refuses or at a conflict, no longer leaves the changes before
  it applied but still pending (#247). Applying again found a conflict
  on the first of them ("changed or deleted by another session"), a new
  row went in twice, and a commit committed changes the grid showed as
  not applied. The result is now fetched again and shows its rows as
  they are; the changes that weren't applied wait for *Apply*, but for
  those on a row another session changed meanwhile, which the messages
  name.
- *Edit* creates the object where it was, also when another database or
  schema is current (#248). On MySQL the script dropped a routine or
  trigger in its database and created it in the current one, or failed
  after the drop, so a trigger was lost; it now starts with `use` of the
  object's database. On SQLite the same went for the views, triggers and
  indexes of an attached database, which went to `main`, and on SQL
  Server a routine stored without its schema was created in the default
  one. Both scripts now name the schema.
- Copies and exports of a result being edited leave out the key column
  Pythia adds to find its rows (#249). *Copy as*, *Export*, *Copy all*
  and *Copy* wrote `pythia$id` (Oracle's `PYTHIA$ROWID`) as one of the
  table's columns, so their INSERT statements failed with "Unknown
  column 'pythia$id'". A selection of only that column copies nothing
  and says why.
- Copies and exports of a result being edited no longer write values
  other than the grid shows (#250). With changes waiting for *Apply*,
  *Copy*, *Copy as*, *Export* and *Copy all* wrote the rows as fetched:
  the old value of a changed cell, rows marked to be deleted, no new
  rows. Rows with changes not applied now aren't copied until the
  changes are applied or discarded, and the messages say so; other rows
  copy as before. *View value…* shows a changed cell as the grid does.
- A program window compiles a unit of another schema in that schema
  (#251). Opened from the object browser, Favorites or Recent as a DBA,
  a package, procedure, function, type or trigger compiled into the
  session's own schema and left its own as it was: Oracle keeps a unit's
  source without its schema. The window now names the unit's schema, and
  a trigger's table's, when the text doesn't.
- SQL Server: a table's DDL has its columnstore indexes (#252). It left
  them out, so that run elsewhere it made a table with a clustered
  columnstore index a heap stored in rows, and one with a nonclustered
  columnstore index a table without it. *Indexes* showed such an index
  without columns, and a covering index `(a) include (b, c)` as one on
  `a` alone; a columnstore index's columns are now under *Columns*, and
  *Include* has the INCLUDE columns, as on PostgreSQL.
- Tab in a cell being edited goes on to the next cell and edits it
  (#254). It kept the value but left the cursor where it was, so the
  value typed next went into the same cell and replaced it. Shift+Tab
  edits the cell before; read-only columns are passed over, and at the
  end of a row Tab goes on to the next one. In the record view, Tab goes
  to the next field.
- A change to a result being edited can be taken back before *Apply*
  (#257). A cell set to NULL with Delete by mistake, or a paste over the
  wrong cells, could only be taken back with *Discard*, which dropped
  every other change too. Ctrl+Z in the grid, or *Undo* in the cell's
  menu, now takes back the last change, one at a time.
- *Format Code* no longer changes strings, comments and quoted names on
  MySQL, PostgreSQL, SQL Server and SQLite (#259). It read every window
  as Oracle's SQL: MySQL's `'it\'s FROM here'` got a line break and a
  space inside and `from` in lower case, a `#` comment was cut and its
  rest became code, `[Order]` became `[ order ]`, and PostgreSQL's
  `E'…'` and `$$…$$` strings broke the same way; the rest of the
  statement, such as its ORDER BY, wasn't laid out. The formatter now
  reads the text as the window's database does, also when formatting on
  save or as typed, and when checking what *AI: Format Unsupported
  Statements* answers.
- SQLite: a database file made with *New…* is no longer made again,
  empty, once it's moved or deleted (#255). The connection was saved
  with `?create`, so connecting again, or a new window while connected,
  made an empty database in its place without a word. `?create` now
  makes the file for the connect it's given to: the connection is saved
  and opens its other sessions without it, and a file that's gone says
  "There is no database file".
- SQL Server's `float` and `real`, and MySQL's DOUBLE and FLOAT in a
  query with bind variables, keep their exponent (#261). 2e-20 showed as
  `0.00000000000000000002` and 1e-300 as 302 characters, which the grid
  cut short to `0.0000…`, as if it were 0; *Copy* and the CSV exports
  wrote them so. They now read `2e-20` and `1e-300`, as on SQLite and
  PostgreSQL; values from 1e-4 to 1e16 stay as they were (`2.5`, `100`).
- MySQL: a BIT value can be edited in a result (#253). `1` typed into a
  BIT(1), which shows as `00` or `01`, was written as `X'1'`, a syntax
  error, and on MariaDB every BIT edit was refused ("Truncated incorrect
  DECIMAL value"), also of a value typed as it is shown. A BIT is now
  written as the number its hex stands for (`81` as 129), in INSERT
  exports too.
- A binary value typed into a result as the grid shows it, `0x…`, is
  stored as those bytes (#256). On MySQL, PostgreSQL and SQLite
  `0x171066e1` went in as text, the ten bytes `0x3078313731…`, without
  a word; SQL Server took it already. An odd number of digits gets a 0
  in front, as MySQL reads `0x…`. Text columns keep such a value as
  text.
- MySQL: a table's *Indexes* show an index on an expression with its
  expression (#258). An index on `((lower(name)))` showed no columns,
  and one on `(id, (lower(name)))` read `id`, like an index on `id`
  alone. Such a key part now reads ``(lower(`name`))``, as in the DDL.
- MySQL: a column's default reads with its quotes in an object view's
  *Columns* (#260), as on MariaDB and in the DDL. `DEFAULT ''` read as
  no default, `DEFAULT 'NULL'` as NULL, `'CURRENT_TIMESTAMP'` as the
  function, and an ENUM's `'a'` as `a`. Numbers, bits and expressions
  such as `CURRENT_TIMESTAMP` or `uuid()` read as before.
- A NULL cell of a result being edited stays NULL when its editor is
  opened and closed without typing (#262). It became an empty text: a
  new row's TIMESTAMP opened so failed on *Apply* with "Incorrect
  datetime value: ''" instead of taking its default, a date or number
  of a fetched row failed the same way, and a text column's NULL was
  replaced by `''`. *Set NULL* and the value editor's *Apply* work as
  before.
- Rows copied from a result and pasted into one being edited keep their
  empty texts (#275). *Copy* writes a NULL and `''` alike, as an empty
  cell, and *Paste* took every empty cell for NULL: `''` went back as
  NULL, which a NOT NULL column refused on *Apply*. The cells Pythia
  copied now paste as they were, NULLs too, also into another result;
  text from elsewhere, such as a spreadsheet's range, still pastes an
  empty cell as NULL.
- *Paste* into a result no longer splits a value with a line break or a
  tab into new rows and columns (#276). *Copy* wrote such a value as it
  was, so pasted back it went over the cells next to it and added rows,
  and in a spreadsheet it took two rows too. *Copy*, *Copy with header*
  and *Copy all* now write it in double quotes, as spreadsheets do, and
  *Paste* reads a cell in quotes as one value, also from a spreadsheet.
  Other text pastes as before.
- The summary of a selection in a result being edited adds up what the
  grid shows (#277). *Count*, *Sum*, *Avg*, *Min* and *Max* were of the
  values fetched: a changed value counted with its old one, a cell set
  to NULL still counted, a row to be deleted was added up, and with a
  filter on, a new row's cells were those of a row the filter hid. They
  now take the changes waiting for *Apply* and leave out the rows to be
  deleted, and follow each change.
- The summary of a selection no longer reads `Sum 0` for very small
  numbers (#278). The sum and average of floats such as `2e-20` and
  `3e-20` were rounded to four decimals, "Sum 0 · Avg 0", and those of
  `1` and `1e-5` read 1 and 0.5. They now read as floats are shown,
  `5e-20` and `2.5e-20`, `1.00001` and `0.500005`; plain decimals still
  add up exactly.
- Rows fetched into a sorted result no longer move the selection and
  the cell being edited to other rows (#279). The rows that come in when
  such a result is scrolled to its end are sorted in among the others,
  and the selection, the cursor, the cell being edited, *View value…*
  and the dialog editing a cell stayed where they were in the view, on
  other rows: a value being typed went into another row. They now stay
  on their rows, and so do new rows.
- *View value…* no longer freezes Pythia on a value of megabytes (#284),
  such as a PostgreSQL `text`, `json` or `bytea`: it worked the value out
  again on every frame and laid all of it out, seconds a frame for 20 MB.
  A text over 100,000 characters now shows in rows, a long line cut
  every 1,000 characters, laid out as they come into view; all of it can
  still be scrolled through, and *Copy* copies all of it.
- An *Apply* whose transaction is rolled back part-way no longer loses
  the changes it made before (#289). On SQL Server, Break during an
  *Apply* opens the connection again, which rolls back what wasn't
  committed, and on MySQL a deadlock rolls back the whole transaction;
  the window took the changes before for applied, and they were gone
  from the grid without a word. The messages now say that they were
  undone, and all of the *Apply*'s changes wait for *Apply* again.
- PostgreSQL: a table's DDL creates the sequences of its `serial`
  columns (#263). It used `nextval('shop.orders_id_seq'::regclass)`
  without creating the sequence, so it failed in an empty database, and
  a copy in another schema took its ids from the original's sequence.
  The DDL now creates each sequence a column owns before the table and
  makes the column its owner after it, as `pg_dump` does. An identity
  column keeps its options, `generated by default as identity (start
  with 10 increment by 5)`, which were left out.
- PostgreSQL: *Find Objects* finds names by their lowercase non-ASCII
  letters (#264). `čaj`, `ČAJ` and `č` found no table `čaj`, nor
  `BIÈRE` one named `bière`, in any database: PostgreSQL's names collate
  as C, where `upper()` changes ASCII letters only. Each letter of the
  filter now matches its cases, as the object browser's filter does.
- SQL Server: a table's DDL has its XML and spatial indexes, and an
  ordered columnstore index's `ORDER` (#265). They were left out, so
  that run elsewhere the DDL made a table without its XML and spatial
  indexes, and a clustered columnstore index `order (b, a)` of SQL
  Server 2022 without the order its rows are kept in. Spatial indexes
  keep their tessellation, bounding box, grids and cells per object.
  Selective XML indexes are still left out.
- MariaDB: sequences are listed under *Sequences* in the object
  browser, no longer under *Tables* (#270). They opened as tables, with
  the sequence's state for columns, and their DDL, `CREATE TABLE …
  SEQUENCE=1`, made a sequence that starts at 1 and goes up by 1,
  whatever the sequence's own values. A sequence's DDL is now `SHOW
  CREATE SEQUENCE`'s, *Find Objects* finds it as a sequence, and
  completion offers it in expressions, `nextval(order_no)`, no longer
  after `FROM`.
- MySQL: a table's *Indexes* show a key part's prefix length and
  order (#283), as the DDL has them. An index on `name(5)` read `name`,
  as if it were on the whole column, and one on `(a desc, b)` read `a,
  b`; they now read `name(5)` and `a DESC, b`, and an expression part
  ``(lower(`name`)) DESC``. MariaDB's prefixes read the same way; it
  keeps DESC from 10.8.
- MySQL: a view's DDL names the tables of other databases with their
  database, and those of its own without it, whatever the session's
  database (#285). `SHOW CREATE VIEW` left out the session's database
  instead: *Edit* of a view of another database that reads one of its
  tables failed with "Table … doesn't exist", as the script runs in
  the view's database, and on MariaDB a view that reads another
  database named its own, so that a copy run elsewhere read the
  original's tables. The view is named without its database, as a
  table is.
- SQLite: a `file:` URI makes a missing database file only with
  `mode=rwc` (#266), as a path does with `?create`. Without a `mode=`,
  a URI whose file was mistyped, moved or deleted connected to a new,
  empty database in its place. It now fails with "There is no database
  file"; `mode=ro` and `mode=memory` work as before.
- *Drop…* names the object as its database writes it (#268), in its
  question and in the log, as do the hover over a browser entry and the
  log lines of a failed drop and of *Show in object browser*. They
  quoted every name as Oracle would, on any database: SQLite's
  `main.t` read `"main"."t"`, MySQL's ``shop.`my t` `` read
  `"shop"."my t"`, two strings. A MySQL account reads `'user'@'host'`.
- The object browser shows names with their case (#272). It showed
  every name in lowercase, so PostgreSQL's `"Foo"` and `foo`, MySQL's
  `Foo` and `foo` on Linux, and Oracle's `FOO`, `"Foo"` and `"foo"` each
  looked the same, and SQLite's or SQL Server's `Foo` didn't read as
  created. Names show as they are; on Oracle an uppercase name still
  reads in lowercase (`foo`), and one with lowercase letters as SQL
  writes it (`"Foo"`). So do the tabs, *Favorites* and *Recent*, a
  table's indexes, constraints and triggers (and their *Copy name*), and
  the object named in an editor's context menu.
- SQLite: a table named `""` shows as `""` in the object browser (#281),
  under *Favorites* and *Recent*, on its tab and in its view's heading
  (`main.""`). It showed as its icon and a button without a name, 16
  pixels wide, and its tab had no title. A name of spaces only shows
  quoted as well (`" "`).
- *Edit* of a SQLite trigger, view or index, and of a PostgreSQL
  materialized view, no longer leaves the object dropped when its new
  version fails (#286). The script dropped it, then created it, and the
  drop was committed: a typo in the new version made the object go.
  SQLite's script now runs in a transaction of its own that a failing
  statement rolls back; with a transaction pending in the window it
  stops at its `begin`, before anything changed. PostgreSQL's drops and
  creates the view in one `DO` block, which waits for Commit.
- The JSON and Excel exports keep every digit of a number (#267).
  18446744073709551615 went into JSON as `1.8446744073709552e+19` and
  9223372036854775807 into Excel as 9223372036854775808. JSON now has
  the number as the database shows it (`.5` as `0.5`): a reader that
  takes JSON numbers for doubles gets the nearest one as before, one that
  reads them exactly gets all the digits. Excel holds 15 significant
  digits, so a number whose whole part has more, as
  9223372036854775807, goes in as text with all of them; a longer
  fraction rounds as Excel's own numbers do, so an average stays a
  number.
- Oracle: a binary value typed into a result as the grid shows it,
  `0x…`, is stored as those bytes (#271), as on the other databases
  (#256). In a RAW, and in the RAW, BLOB or LONG RAW of a new row,
  `0x171066e1` failed on *Apply* with ORA-01465 "invalid hex number".
- Oracle: INSERT statements name a column or a table called like a
  reserved word in quotes (#274), `"LEVEL"` or `"SIZE"`. *Copy as*, the
  grid's *Export*, *Execute to file* and *Export tables* wrote
  `insert into t (id, level, size)`, which fails with ORA-03050.
  `RESOURCE`, `RENAME`, `EXCEPT`, `MLSLABEL` and `SUCCESSFUL` weren't
  known as reserved words: they are now, also in the SQL that result
  editing and the object editors write.
- *Export tables* and *Export schema to folder* write each object into a
  file of its own (#280). The file was the name in lowercase, so `FOO`,
  `"Foo"` and `"foo"` shared one and only the last was left, `"A/B"`
  went into a folder `a`, and `"A:B"` was no file name on Windows. A
  name Oracle reads unquoted keeps its file (`foo.sql`); in others a `^`
  marks each run of letters of the other case (`"Foo"` → `f^oo.sql`,
  `"foo"` → `^foo.sql`), which keeps them apart also where `Foo.sql` and
  `foo.sql` are one file, and a character a file name can't hold is
  written as `%` and its hex (`"A/B"` → `a%2Fb.sql`). Comparing such a
  file with the database finds its object again.
- Pythia builds with tokio 1.54.1 and clipboard-win 5.4.3 (#288): 1.54.0
  and 5.4.1 were yanked, tokio's for RUSTSEC-2026-0384, a race in its
  timers that can corrupt memory. rsa, which the SSH tunnels use, still
  has RUSTSEC-2023-0071 (a timing side channel), for which there is no
  fixed version yet.
- *Explain Plan* in a SQL window explains on the window's own session,
  where its statements run (#269). It used the connection's main session,
  so a temporary table, a search path, a current schema or a `use db` of
  the window didn't count: Explain said there was no such table, or
  explained another schema's. The window's transaction stays as it was,
  also when there is no plan. While the window runs a statement, Explain
  says so; on MySQL, PostgreSQL, SQL Server and SQLite it ends the fetch
  of a result with rows left, which then says it shows the rows fetched.
- *Explain Plan* of a selection without a statement, only a comment or
  blank space, says "Nothing to explain" (#273); it did nothing and said
  nothing. A lone `;` is no statement either: it was sent, and PostgreSQL
  answered `explain (format json) ;` with a syntax error. `&name` is
  replaced only where the window substitutes, as for *Execute*: on MySQL
  a `DEFINE`d name changed `'A &amp; B'` in the plan's statement.
- Completion offers the columns of a table in another schema also after
  it listed that schema's objects (#282). On SQL Server, PostgreSQL,
  MySQL and SQLite, once `select * from r51_s.` had offered `r51_t`,
  `select t. from r51_s.r51_t t` offered nothing: the table, typed in
  another case than stored, was taken for one that doesn't exist.
- Completion after an alias that follows `&` without a space, as in
  `where 1&c.`, offers its columns on MySQL, PostgreSQL, SQL Server and
  SQLite (#287). It offered nothing, reading `&c.` as a substitution
  variable, which there it is only after `SET DEFINE ON` (#190). Code
  assistance now reads `&name` as the window substitutes.
- MySQL: a statement with several results shows each in a tab of its own
  (#290): the SELECTs of a procedure a `CALL` runs, statements kept
  together after `DELIMITER` or in MariaDB's `BEGIN NOT ATOMIC`. Only the
  first showed, without a word, and an error after it was lost, so the
  script went on; a SELECT after a `DO` didn't show at all. Such an error
  now fails the statement. *Execute to file* writes the first result and
  says how many others there were.
- A script in a SQL window runs at its database's pace (#291). It ran
  one statement per frame the window drew: 4,000 short statements took
  at least 67 s at 60 Hz, and longer while the window was in the
  background. The window now sends a script's statements ahead to its
  session, all but those it has to handle itself (variables to ask for,
  `PROMPT`, `DEFINE`, `WHENEVER`, `@file`), which wait for the ones before
  them. An error, Break's too, still stops the script unless `WHENEVER
  SQLERROR CONTINUE`, and each statement keeps its message and its
  result. The Messages tab lays out only the messages in view: with
  40,000 of them a frame took a quarter of a second.
- Names show with their case outside the object browser too (#295), as
  #272 shows them there: Oracle's `FOO` as `foo` and `"Foo"` as
  `"Foo"`. The maintenance scan, an index's *Maintenance…*, *Export
  tables*, *Invalid objects*, a table's indexes and constraints in the
  browser, *Edit table…*'s partitions and *Grants…* showed every name in
  lowercase, so `"Foo"` read like `FOO`; the scan's tab did so on every
  database. The program window offers the file *Export schema to folder*
  writes for the unit (`f^oo.pks` for `"Foo"`), not `foo.pks` for both.
  A foreign key on a column such as `"P_Id"` marks it in the browser.
- A name with a space at its start or end shows quoted (#298), `" t"`
  and `"t "`, in the object browser, *Favorites*, *Recent*, the tabs and
  the view's heading (`main."t "`). It read like `t`, so on SQLite or
  PostgreSQL `" t"`, `"t "` and `t` looked the same.
- PostgreSQL: a materialized view's DDL has its indexes, comments and
  grants (#301). It had only the `CREATE`, so *Edit*, which drops the
  view and creates it again from the DDL, made it without them: no
  index, not even the unique one `REFRESH … CONCURRENTLY` needs, and no
  privileges for the roles that had them. They now follow the `CREATE`
  in *Edit*'s block, and in the *DDL* tab; the owner's own privileges
  come with the view.
- The object browser offers *Edit* for views, PostgreSQL's materialized
  views and SQLite's indexes (#300), on the databases without program
  windows, as it did for routines and triggers; so do the editor's
  context menu and the object view. The README said *Edit* opens SQLite
  views, but only their triggers had it, and the scripts that replace a
  view (`CREATE OR REPLACE`, `CREATE OR ALTER`, SQLite's and
  PostgreSQL's in a transaction of their own) were out of reach.
- SQL Server: *Indexes*, *Constraints*, *Triggers* and a table's *DDL*
  work on SQL Server 2016 and in a database at compatibility level 100
  (#294), as one moved from SQL Server 2008 is. On 2016 they failed with
  "'string_agg' is not a recognized built-in function name", and at
  level 100 all but *Triggers* with "Incorrect syntax near '('". What
  they show is as before.
- SQL Server: a memory-optimized table's DDL makes a memory-optimized
  table (#302), `with (memory_optimized = on, durability = …)`, and
  declares its indexes in it, hash indexes with their bucket count. It
  made a disk-based table, left out the hash indexes, and created the
  other ones with CREATE INDEX, which such a table refuses; a hash key
  without its bucket count made the DDL fail.
- SQL Server: a table's DDL has its selective XML indexes (#311), with
  their namespaces and paths (`as sql int`, `as xquery 'xs:string'
  maxlength(20)`, `singleton`), and the secondary ones on their paths.
  They were left out, so that run elsewhere the DDL made a table whose
  XML queries had no index to use.
- SQL Server: a table's DDL has its indexes' and keys' options where
  they aren't the defaults (#315): `ignore_dup_key = on`, `fillfactor`,
  `pad_index`, `allow_row_locks = off`, `allow_page_locks = off`,
  `statistics_norecompute`, `statistics_incremental`,
  `optimize_for_sequential_key`, `compression_delay`, and their
  `data_compression` and `xml_compression`, by partition where they
  differ. The table, its indexes and its LOB data keep their filegroup
  or partition scheme (`on [fg2]`, `on [ps] ([id])`, `textimage_on`), and
  a heap its compression. They were left out, so that run elsewhere a
  key that ignored duplicates rejected them, and tables and indexes were
  made uncompressed and unpartitioned on the default filegroup. An index
  on a partition scheme no longer gets the partitioning column as its
  first column, in the DDL and in *Indexes*.
- A sort or a filter of the result grid no longer moves the selection
  and the cell being edited to other rows (#296). After a click on a
  column's header, the selection, the cursor, the cell being edited,
  *View value…* and the dialog editing a cell stayed where they were in
  the view, on other rows: *Set NULL*, *Delete / undelete rows*, a value
  typed and the dialog's *Apply* went into rows that weren't selected.
  A filter did the same to the viewer, the dialog and the edit. They now
  stay on their rows; a selection of several rows keeps the cursor's
  row. Where the filter hides their row, the viewer and the dialog close
  and the value being typed is kept.
- Ctrl+Z that takes back a new row no longer leaves *View value…*, the
  dialog editing a cell and a cell being typed into on another row
  (#299). They stayed where they were in the view: past the rows, where
  the dialog's *Apply* did nothing or went into the next new row, or on
  a new row Ctrl+Z brought back in between. They now stay on their rows;
  on the row taken back, the viewer and the dialog close and the edit
  ends, which also no longer keeps the keys from the grid.
- SQL Server: a connection lost once a statement of an Apply ran, while
  Pythia read how many rows it changed, no longer reads as "Conflict on
  row N: it was changed or deleted by another session" (#310). The
  statement now fails with the lost connection, which says the statement
  ran and its row count couldn't be read, and the change waits for
  Apply. A connection out of step then is opened again.
- SQLite: *Find Objects* finds names by their non-ASCII letters in any
  case (#292). `čaj`, `ČAJ` and `č` found no table `čaj`, `BIÈRE` none
  named `bière` and `straße` no view `Straße`: SQLite's `upper()` and
  `LIKE` change ASCII letters only. Each letter of the filter now
  matches its cases, as the object browser's filter does.
- Oracle: the filters of names find a quoted name by its lowercase
  letters (#305): the object browser's (also *Favorites*, *Recent*,
  users and roles), *Find Objects*, *Export tables*, *Invalid objects*,
  *Search data*'s tables and column names, *Search source*'s name and
  *Compare schemas*' name filter and list. They compared what was
  typed, in uppercase, with the name as stored, so `case` found no
  `"My_Case"`, only `MY_C` did. The names are compared in uppercase
  too.
- *Find Objects* finds a name with a `ß` by it (#306), on Oracle, MySQL,
  MariaDB and SQL Server, and so do *Search data*'s column names and
  *Compare schemas*' name filter on Oracle. The filter's `ß` became
  `SS`, which no database's `upper()` writes, so `straße` found no table
  `straße`. A letter whose uppercase is more than one letter now stays
  as it is.
- PostgreSQL: the DDL of a sequence that a column owns, a `serial`
  column's or one made `owned by` it, names that column (#307), `owned
  by shop.orders.id`. Run elsewhere, the copy was owned by no column:
  `pg_get_serial_sequence` didn't find it, and dropping the copy's
  table left it behind.
- PostgreSQL: a table's DDL and *Columns* keep the name given to an
  identity column's sequence (#308), `generated by default as identity
  (sequence name shop.order_ids)`. A copy's sequence was named as
  PostgreSQL chooses, `orders_id_seq`, and what reads it by its name
  failed. A name PostgreSQL chose is still left out, so that a copy
  beside the table chooses its own.
- JSON exports keep every column, in the result's order (#297). Each
  row was an object keyed by the column names, sorted: of two columns
  named `ID`, as in most joins, only the last one's value was written. A
  name that comes again is now `ID_2` (or the next number no column is
  called). *Copy as*, the grid's *Export*, *Execute to file* and *Export
  tables* write JSON so.
- Compare: sync scripts quote a column, table, constraint or index
  called `RESOURCE`, `RENAME`, `EXCEPT`, `MLSLABEL` or `SUCCESSFUL`
  (#303), as INSERT statements do since #274: `create table words (id
  NUMBER, resource NUMBER)` failed with ORA-03050.
- Oracle: BINARY_DOUBLE and BINARY_FLOAT values keep their exponent
  (#304), as SQL Server's and MySQL's floats do since #261: `1e300d`
  showed as a 1 and 300 zeros, `2e-20d` as `0.00000000000000000002`, in
  the grid and in copies and exports, also with the Instant Client.
- Oracle: a fetched row's CLOB, NCLOB, BLOB, LONG or LONG RAW can be
  changed in a result (#309). *Apply* failed with ORA-00932 or
  ORA-00997: the UPDATE checked the value fetched with DECODE, which
  takes no LOB and no LONG. A LOB is now checked with
  `dbms_lob.compare`; a LONG or LONG RAW, which SQL can't compare, is
  left out of the check, and so is a LOB with the pure-Rust driver,
  which types it as one.
- Oracle: INSERT statements of a BLOB over 2000 bytes run (#314). They
  wrote it as one `hextoraw('…')`, which takes no more than 4000 hex
  digits: ORA-01704. *Execute to file* and *Export tables* failed so on
  any larger BLOB, *Copy as* and the grid's *Export* on one of 2001 to
  4000 bytes. Such a row is now written as a PL/SQL block that makes the
  BLOB from parts of 1000 bytes and inserts it; Oracle compiles it in
  some seconds a megabyte. With the Instant Client, a change to a
  fetched BLOB of that size no longer fails on the check of its value.
- Oracle: a CLOB, NCLOB or LONG over 4000 bytes can be written (#317).
  INSERT statements and result editing wrote it as one literal, which
  holds no more: ORA-01704 from *Execute to file*, *Export tables*,
  *Copy as* and the grid's *Export*, and on *Apply* of a change to such
  a value or a new row with one. Result editing now writes it as
  `to_clob('…') || '…'` of parts of 1000 characters, which Oracle takes
  for up to about 250,000 characters; INSERT statements write such a row
  as a PL/SQL block that appends the parts to a CLOB, for any length. A
  LONG over 4000 bytes still can't be written so.
- A result whose query was closed with rows left says so (#312): it
  keeps its `+`, *Fetch all* and scrolling don't fetch from it, and the
  messages say it shows the rows fetched. The next statement of a script
  closes it, and on MySQL, PostgreSQL, SQL Server and SQLite any other
  request to the window's session, such as a Commit or Rollback. *Fetch
  all* then found nothing more there, and the result looked complete with
  rows missing; on Oracle the window waited for rows that never came and
  could run nothing else. *Execute to file* fails in that case rather than
  end the file early as if it were complete.
- SQL Server and PostgreSQL: a statement with several results shows
  each in a tab of its own (#316), as on MySQL (#290): the SELECTs of a
  T-SQL batch that `DECLARE`s, the result sets of a procedure
  (`exec sp_help 't'`), the SELECTs rules add to an INSERT. SQL Server
  showed the first with a notice that there were more; PostgreSQL showed
  the rows of all under the first one's columns. An error after the
  first result now fails the statement, which stops a script; it came as
  a message only. *Execute to file* writes the first result and says how
  many others there were.
- The statement history takes in the statements a script ran in one go
  (#318). Each statement went through the whole history, 2,000 entries,
  and since #291 a frame hands over up to 1,000: 17 ms of every such
  frame went to it. The order and the counts stay as they were.
- A `;` with no statement before it, as in `select 1;;` or on a line of
  its own, is no statement (#313). It was sent to the database, alone or
  with the next statement: MySQL answered ERROR 1065 or 1064, Oracle
  ORA-00900 and SQLite "bad parameter or other API misuse", which
  stopped the script; PostgreSQL and SQL Server said "; executed".
- The Output tab of a SQL window lays out only the lines in view
  (#321), as the Messages tab does since #291. It laid out every line of
  DBMS_OUTPUT in each frame: with 40,000 a frame took 101 ms.
- The SQL window highlights `&name` as a substitution variable only
  where it is one (#322): on Oracle unless `SET DEFINE OFF`, elsewhere
  after `SET DEFINE ON` (#190). On MySQL, PostgreSQL, SQL Server and
  SQLite `1&c.id` showed `&c.` as a variable, where `&` is an operator
  and `c` the alias, as completion reads them since #287.
- *AI: Analyze Execution Plan* explains in the SQL window's own session
  (#319), as *Explain Plan* does since #269, and the assistant describes
  the window's tables by its current schema. On the connection's main
  session a private temporary table of the window had no plan (ORA-00942),
  and `alter session set current_schema` didn't count. The window's
  transaction stays as it was. While the window runs a statement, the
  plan analysis says so; the other requests are then looked up on the
  main session as before.
- PostgreSQL: the DDL of a view or materialized view names every table
  and function with its schema (#329), as a table's does. It left the
  schema out where the connection's search path found them, `FROM t`
  for `public.t`, so that *Edit* in a SQL window whose search path finds
  another `t` first made the view read that one.
- PostgreSQL: *Edit* of a materialized view keeps its owner, its lack
  of data and its storage (#330). The view it made again belonged to the
  role that ran *Edit*, was populated where it was made `with no data`,
  and had the default storage parameters, tablespace and access method.
  The DDL now has them where they aren't the defaults, and `alter
  materialized view … owner to …` where the owner isn't you. A role that
  can't give the view back to its owner gets an error, and the view
  stays as it was.
- PostgreSQL: the DDL of a table, view, sequence, function or procedure
  has its grants (#331), as a materialized view's has, a table's with
  its columns' and its `serial` sequences'. Run elsewhere, a copy had no
  privileges for any role but its owner, and a routine whose `EXECUTE`
  was revoked from `PUBLIC` could be run by every role again: its DDL
  now revokes that too.
- PostgreSQL: the DDL of an identity column's sequence says that the
  table's DDL makes it, and sets the identity's options (#332), `alter
  table shop.orders alter column id set increment by 3 …`. It was a
  `create sequence`, which next to a copy of the table failed with
  `relation … already exists`, and elsewhere made a sequence no column
  used.
- SQL Server: a table's DDL keeps its columns' collations where they
  aren't the database's (#323), `[code] varchar(10) collate
  Latin1_General_CS_AS`. They were left out, so that run elsewhere a
  case-sensitive or binary column compared without case, and a UTF-8
  column stored only the letters of the database's code page: `čaj 茶`
  became `caj ?`.
- SQL Server: a temporal table's DDL makes a temporal table (#324): its
  period columns are `generated always as row start` and `row end`,
  `hidden` where they are, with `period for system_time (…)` and
  `with (system_versioning = on (history_table = …,
  history_retention_period = …))`. It made a plain table, which kept no
  history and showed the hidden columns. A history table SQL Server
  named is left out, so that a copy gets its own. Run elsewhere, SQL
  Server makes the history table as it made the original's, or takes
  the one the history table's DDL made before.
- SQL Server: a table's DDL keeps its sparse columns and its column set
  (#325), `[a] int sparse null`, `[cs] xml column_set for
  all_sparse_columns null`. They were made plain columns: run elsewhere,
  rows written through the column set left the sparse columns NULL,
  `select *` read every sparse column, and a table of more than 1,024
  columns failed.
- SQL Server: a table's DDL keeps what CREATE TABLE can't give (#326).
  A foreign key or check that isn't trusted is added after the table
  `with nocheck`, and a disabled one then disabled (`nocheck
  constraint`); a disabled index or key is disabled after it is made,
  a clustered one last; and the table's `lock_escalation` is set where
  it isn't `TABLE`. Run elsewhere, they were all enabled and trusted:
  the copy rejected rows the original takes, and built and kept up to
  date indexes the original doesn't.
- SQL Server: a table's DDL makes its clustered index before the other
  indexes (#327). It came in the order of their names, so that run
  after the rows were loaded, the nonclustered indexes before it were
  built twice, the second time by the clustered index. Hypothetical
  indexes, which have statistics and no rows, are left out: they were
  made as real ones.
- *Search data* opens a hit of a quoted table or column by its name as
  it is (#334). The query named schema, table and column in lowercase,
  quotes and all, so a hit in `"R71_Data"` read `"r71_data"`: it failed
  with ORA-00942, or read the rows of another table. Plain names still
  read in lowercase.
- *Import* into a new table creates the table and columns it inserts
  into (#335). Its CREATE TABLE lowercased the quoted names too, so
  `"R71_My Table"` was created as `"r71_my table"`, and a column read
  from a header `Date` or `Café` as `"date"` or `"café"`, where the
  INSERT names `"DATE"` and `"CAFé"`: the import failed with ORA-00942
  or ORA-00904 and left the table behind. Plain names still read in
  lowercase.
- A package's subprograms read with their case in an editor's context
  menu and in the program window (#336), as names do since #272. A
  quoted `"MyProc"` read `myproc` in the menu's heading, its *Edit* and
  "… not found", and `"myproc"` in the outline and "… isn't in the
  body". They read quoted where they have lowercase letters, others in
  lowercase.
- The *Schema* of *Export tables*, *Search data* and *Search source*
  takes a quoted name as written (#337): `"r71_schema"` finds the schema
  `r71_schema`, `hr` still `HR`. They uppercased what was typed, quotes
  and all, so a schema with lowercase letters couldn't be named and
  nothing was found. So does a connection's *Default schema*, which gave
  "Login script: ORA-01741" for `"r71_schema"` on Oracle, a schema
  `"""sales"""` for `"Sales"` on PostgreSQL and failed for MySQL's
  `` `my db` ``.
- *Delete / undelete rows* on a new row and *Discard* no longer leave
  *View value…*, the dialog editing a cell and a cell being typed into
  on another row (#328). They stayed where they were in the view: on the
  next new row, where the dialog's *Apply* wrote, or past the rows, where
  the value typed was lost and the next *Insert row* came. They now
  follow their rows, and on a row taken away the viewer and the dialog
  close and the edit ends. The selection stays on the rows there are:
  a Paste past them added empty new rows up to it.
- *View value…* no longer keeps every image it showed in memory (#333).
  egui kept an image's bytes, its pixels and its texture until the
  viewer's ✕ closed it: going through a column of images with the viewer
  open kept each one, as did a viewer closed by a filter, by Ctrl+Z or
  with its result, a photo's pixels taking tens of megabytes twice. The
  viewer now lets go of an image once it shows another value or closes.
- Result editing: with *Auto-commit* on, an Apply whose connection is
  lost part-way keeps the changes it made before applied (#338). Each
  of them was committed as it ran, but all of them stayed pending on the
  rows as they were fetched, and applying again reported "Conflict on
  row 1: it was changed or deleted by another session". The result is
  now fetched again, in a new session, with the changes from the one
  that failed on waiting for Apply. In a transaction, which the lost
  connection rolled back, all the changes still wait.
- Oracle: BINARY_DOUBLE and BINARY_FLOAT values are written as literals
  of their type (#339): `1e300d`, `2.5f`, `binary_double_infinity`,
  `binary_float_nan`. As NUMBERs, values beyond NUMBER's range failed in
  INSERT statements and on *Apply* of a change with ORA-01426 (`1e300`)
  or were written as 0 (`2e-300`), and the check of an infinity or NaN
  fetched compared it as text and reported the row as changed by
  another session.
- Oracle: a fetched row's JSON or VECTOR can be changed in a result
  (#340). *Apply* failed with ORA-40796 or ORA-00932: the UPDATE checked
  the value fetched with DECODE, which takes neither. A JSON is now
  checked with `json_equal`, which finds an object with its keys in
  another order than Pythia shows them; a VECTOR, which SQL can't
  compare, is left out of the check.
- Oracle: a VECTOR's float elements keep their exponent (#341), as
  BINARY_DOUBLE values do since #304, and so do a JSON's binary numbers:
  `[1e30,0.1]` showed as `[1000000000000000000000000000000,0.1]`,
  `1e300` as a 1 and 300 zeros. A sparse VECTOR shows as Oracle writes
  it, `[6,[0,5],[2.5,1e-30]]`; it showed as `SparseVector {…}`, which
  its INSERT statements wrote and Oracle rejected with ORA-51804.
- Oracle: a JSON or VECTOR over 4000 bytes can be written (#342), as a
  CLOB's text since #317: INSERT statements and result editing wrote it
  as one literal, which holds no more (ORA-01704 from *Execute to file*,
  *Export tables*, *Copy as* and *Export*, and on *Apply*). A literal of
  parts now makes each part a CLOB, for CLOBs too: a text with many `&`,
  such as a JSON of URLs, failed with ORA-01489.
- Oracle: INSERT statements make each value's literal once (#343).
  Since #314 every row's literals were made twice, once to see whether
  the row needed a PL/SQL block, and each BLOB's hex was copied for it:
  20,000 rows of 2000-byte BLOBs took 1.05 s in a debug build, now
  78 ms.
- *Load from file…* into a new row (#293) puts the file into that row,
  or into none (#344). The file is chosen and read while the grid goes
  on: a new row deleted, taken back by Ctrl+Z or discarded meanwhile gave
  the chosen row's place to another, and the file went there, or nowhere,
  while the message said it was loaded. Where its row went, the file now
  isn't loaded, and the message says so.
- SQL Server: with *Auto-commit* on, a change whose statement ran but
  whose row count couldn't be read, the connection lost in between
  (#310), counts as applied (#345). Auto-commit had committed it, yet it
  stayed pending: a new row's insert failed with a duplicate key when
  applied again, and an update stayed on the row's old value or was
  dropped with "… was changed by another session". The result is now
  fetched again, and the messages say auto-commit committed it.
- Completion in a SQL window knows what the window's statements did to
  its names (#320): after `alter session set current_schema` (Oracle),
  `use db` (MySQL), `set search_path` (PostgreSQL), `use db` or `execute
  as` (SQL Server) and `attach` (SQLite) it offers the tables and
  columns those name, where it offered the connection's, also those of a
  table of the same name there. Names are still looked up on the
  connection's main session, by what the window's session reports, and
  windows that share that share the answers. The window's temporary
  tables complete too: SQLite's, PostgreSQL's, MySQL's and MariaDB's,
  SQL Server's `#tables` and Oracle's private temporary tables, which
  only its session sees, so they are looked up there while it is idle,
  its transaction left as it was (on PostgreSQL after a savepoint, so
  that a lookup that fails doesn't abort it). With a result of the
  window open, completion goes on without them
  rather than end the result's query (on Oracle, which keeps it open,
  with them). SQL Server finds unqualified names in `dbo` after the
  default schema, PostgreSQL in each schema of the search path.
- PostgreSQL: a rule whose actions are in parentheses, `create rule …
  do also (select 1; select 2)`, runs as one statement (#346). The
  script ended it at the first `;` inside the parentheses, which failed
  with "syntax error at end of input", and ran the rest on its own. A
  rule whose parenthesis is still open at the end of the script, as
  while it is typed, ends at that `;` as before, so the statements
  after it stay apart.
- SQL Server: Break while a result's rows are read fails that result's
  statement (#347). The session sends the rows it read, which end the
  statement, before the failure, which the window counted against the
  statement running then: the next one of a script, which was sent
  ahead (#291), counted as failed rather than skipped; after the window
  fetched more of a result, a statement failing ("Failed after …"), for
  which `WHENEVER SQLERROR EXIT ROLLBACK` rolled back. The failure is now
  the statement's, or the fetch's, as one without rows first is. An error
  after a result of a statement with several, which comes the same way
  on MySQL, PostgreSQL and SQL Server, is too.
- PostgreSQL: the DDL of a trigger, function or procedure names every
  table, function and type with its schema (#350), as a view's does
  since #329. It left the schema out where the connection's search path
  found them, `ON t … EXECUTE FUNCTION tf()` for `public.t`, so that
  *Edit* in a SQL window whose search path finds another `t` first made
  the trigger on that table, and a routine whose argument types it finds
  elsewhere was made again beside the one edited, with those types.
- PostgreSQL: *Edit* of a view keeps its `security_barrier`,
  `security_invoker` and check option (#351). The view's DDL left them
  out, and its `create or replace view` reset them: a security barrier
  view let the functions of a query see the rows it filters out, a
  security invoker view read its tables as its owner again, and a view
  `with check option` took rows it doesn't show. The DDL now has them,
  `with (security_barrier='true') as … with local check option`.
- PostgreSQL: a table's DDL makes it as it is (#352), as a materialized
  view's does since #330: `create unlogged table` where it is unlogged,
  its `serial` sequences too, with its access method, storage parameters
  (its TOAST table's too) and tablespace where they aren't the defaults,
  and each index in its tablespace, a primary key's or unique
  constraint's with its storage parameters. Run elsewhere, the copy was
  logged, `heap`, with the default parameters, and it and its indexes
  were in the database's tablespace. A materialized view's indexes keep
  their tablespace too.
- PostgreSQL: the DDL of a table, view, sequence, function or procedure
  gives it its owner where that isn't you (#353), `alter … owner to …`
  after the `CREATE`, as a materialized view's does since #330. Run
  elsewhere, a copy belonged to the role that ran it, and so did a
  table's `serial` sequences; its grants then revoked what the owner had
  given up from a role that didn't have it. A sequence owned by a column
  gets its owner before `owned by`, which needs the table's. *Edit*
  replaces views and routines in place, keeping their owner, so the line
  changes nothing there.
- PostgreSQL: the DDL of a view, sequence, function, procedure or
  trigger has its comments (#354), a view's columns' too, as a table's
  and a materialized view's have since #301, and a table's has its
  `serial` sequences'. Run elsewhere, a copy came without them.
- *Edit in dialog…* no longer freezes Pythia on a text of megabytes
  (#356), fetched or loaded from a file: its text field laid all of it
  out on every frame and for each letter typed, seconds for 10 MB. A
  text over 100,000 characters now shows in rows, as *View value…* shows
  it, to read but not to edit there: *Save to file…* writes it and *Load
  from file…* takes it back. A shorter text scrolls in its field, where
  one of some thousand characters made the dialog taller than the
  screen and put *Apply* out of reach.
- A *Load from file…* that ends without a file is forgotten (#361): one
  chosen while another file dialog was open, which does nothing, and one
  whose file can't be read stayed among the result's loads in progress
  for as long as it was shown.
- PostgreSQL: completion follows a search path set with `set_config`
  (#362), as it follows one set with `SET` since #320: after `select
  set_config('search_path', 'sales', false)` it went on offering the
  names of the path before, and so after a `set_config('role', …)`,
  which changes the schema `"$user"` names. A function that sets the
  path in its body, without a `SET` clause, still changes it unseen.
- SQLite: a database in memory named by a `file:` URI, `file::memory:`
  or `file:scratch?mode=memory`, is one for all the connection's
  sessions, as `:memory:` is (#364). Each session had one of its own:
  the object browser and completion showed an empty database, and
  another window had none of a window's tables. `cache=private` keeps
  one for each.
- SQL Server: a table's DDL keeps its columns' dynamic data masks
  (#348). `masked with (function = …)` was left out, so a copy, an
  *Edit* or a script made from the DDL masked nothing: a user without
  UNMASK read there the e-mail addresses, numbers and dates the masks
  hide in the original.
- SQL Server: a table's DDL keeps a typed XML column's schema
  collection (#349), as `xml(document [schema].[collection])` or
  `xml(content …)`. The column was plain `xml`: a copy took any XML the
  original rejects. The collection itself isn't made by the table's
  DDL, as a foreign key's table isn't.
- SQL Server: a table's DDL keeps ROWGUIDCOL, a persisted computed
  column's NOT NULL, and NOT FOR REPLICATION of identities, foreign keys
  and checks (#355). A copy had no `$rowguid`, took NULLs its computed
  column rejects, and its foreign keys and checks, added `with nocheck`
  as SQL Server never trusts one not for replication, checked the rows
  replication puts in.
- SQL Server: a table's DDL makes its statistics (#357): those made
  with `create statistics` follow its indexes, with their `where` and
  `norecompute`, `incremental = on` and `auto_drop = on`. A copy had
  none of them, and SQL Server makes no statistics on several columns
  or with a filter by itself.
- SQL Server: a graph table's DDL makes a node or edge table (#360),
  `create table … (…) as node` or `as edge`, with its edge constraints.
  It made a plain table with the columns SQL Server adds to a graph
  table written out (`graph_id_…`, `$node_id_…`) and the unique index on
  them, which took no rows without them and no `$node_id`; indexes and
  statistics on `$node_id`, `$from_id` and `$to_id` now name those.
- SQL Server: a ledger table's DDL makes a ledger table (#366), `with
  (system_versioning = on, ledger = on)` or `ledger = on (append_only =
  on)`, its ledger columns `generated always as transaction_id start
  hidden` and so on, and its history table, ledger view and the view's
  columns named where SQL Server didn't name them. It made a plain
  table, which kept no ledger and took no rows without values for the
  ledger columns.
- SQL Server: a table's DDL keeps its Always Encrypted columns
  encrypted (#367), `encrypted with (column_encryption_key = …,
  encryption_type = …, algorithm = …)`. A copy's columns were plain
  ones, which took the values a client sends them in plain text and
  kept them so.
- SQL Server: a table's *Indexes* has a *STATUS* column (#368),
  `ENABLED`, `DISABLED` or `HYPOTHETICAL`. Disabled and hypothetical
  indexes read like the others, which the *DDL* disables (#326) or
  leaves out (#327).
- SQL Server: a statement run while a result has rows left no longer
  waits for the rest of that result (#383). The server sent all of it
  and the driver read it off the wire first: a table of a hundred
  million rows took 50 s here before the next statement started, with
  no message, and Break couldn't stop it. A query is now stopped, as on
  MySQL and PostgreSQL, and an open transaction stays. A batch that may
  change something, such as an `insert … output`, a procedure or a
  `commit` after the query, is still read to its end so that its changes
  are made, and so is any query in a transaction while `XACT_ABORT` is
  on, which stopping would roll back.
- SQL Server: Break keeps the connection and its transaction (#384), as
  on Oracle, MySQL and PostgreSQL. It opened the connection again, which
  rolled back the changes that weren't committed and lost the session's
  temporary tables and settings: the driver gave up reading the server's
  answer to the cancel, which comes after the end of the stopped batch.
- SQL Server: Break stops a fetch while rows keep arriving (#385). It was
  taken only once the session had read all it had asked for, so *Fetch
  all* of a large table couldn't be stopped and read rows into memory
  until the table ended.

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

[Unreleased]: https://github.com/ortic/pythia/compare/v0.11.0...HEAD
[0.11.0]: https://github.com/ortic/pythia/compare/v0.10.0...v0.11.0
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
