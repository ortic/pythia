//! What can be done to an object, with which options, and the statements it
//! takes: each database's own maintenance commands.

use pythia_db::{DbKind, Dialect, ObjectType};
use serde::{Deserialize, Serialize};

use super::facts::{Facts, IndexSegment, Part};
use crate::documents::ObjectRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Task {
    /// Oracle's `DBMS_STATS.GATHER_*_STATS`.
    GatherStats,
    DeleteStats,
    LockStats,
    UnlockStats,
    /// A table's unusable indexes and index (sub)partitions.
    RebuildUnusable,
    /// An index, whole or partition by partition.
    RebuildIndex,
    Shrink,
    Move,
    Vacuum,
    /// PostgreSQL's `ANALYZE`, MySQL's `ANALYZE TABLE`.
    Analyze,
    Reindex,
    Optimize,
    Check,
    UpdateStatistics,
    RebuildIndexes,
    ReorganizeIndexes,
}

impl Task {
    pub fn label(self, dialect: Dialect) -> &'static str {
        match self {
            Task::GatherStats => "Gather statistics",
            Task::DeleteStats => "Delete statistics",
            Task::LockStats => "Lock statistics",
            Task::UnlockStats => "Unlock statistics",
            Task::RebuildUnusable => "Rebuild unusable indexes",
            Task::RebuildIndex => "Rebuild",
            Task::Shrink => "Shrink space",
            Task::Move => "Move",
            Task::Vacuum => "VACUUM",
            Task::Analyze if dialect.kind() == DbKind::MySql => "ANALYZE TABLE",
            Task::Analyze => "ANALYZE",
            Task::Reindex => "REINDEX",
            Task::Optimize => "OPTIMIZE TABLE",
            Task::Check if dialect.kind() == DbKind::Sqlite => "Integrity check",
            Task::Check => "CHECK TABLE",
            Task::UpdateStatistics => "Update statistics",
            Task::RebuildIndexes => "Rebuild indexes",
            Task::ReorganizeIndexes => "Reorganize indexes",
        }
    }

    /// One line on what it is for.
    pub fn purpose(self, dialect: Dialect) -> &'static str {
        match (self, dialect.kind()) {
            (Task::GatherStats, _) => {
                "Fresh optimizer statistics, e.g. after loading or deleting many rows"
            }
            (Task::DeleteStats, _) => "Remove the statistics; the optimizer samples instead",
            (Task::LockStats, _) => {
                "Keep the statistics as they are, e.g. for a table that is empty at night"
            }
            (Task::UnlockStats, _) => "Let the statistics be gathered again",
            (Task::RebuildUnusable, _) => {
                "Indexes and partitions left unusable by a move, partition maintenance \
                 or a direct-path load"
            }
            (Task::RebuildIndex, _) => {
                "Rebuild the index, e.g. when it is unusable or after it shrank a lot"
            }
            (Task::Shrink, _) => "Give space freed by deleted rows back, keeping the segment",
            (Task::Move, _) => "Rewrite the table compactly, or into another tablespace",
            (Task::Vacuum, DbKind::Sqlite) => {
                "Rewrite the database file compactly, giving its free pages back"
            }
            (Task::Vacuum, _) => "Reclaim dead rows for reuse, update the visibility map",
            (Task::Analyze, DbKind::MySql) => "Update the index statistics (key distribution)",
            (Task::Analyze, DbKind::Sqlite) => {
                "Statistics of the indexes for the query planner (sqlite_stat1)"
            }
            (Task::Analyze, _) => "Fresh planner statistics from a sample of rows",
            (Task::Reindex, DbKind::Sqlite) => {
                "Rebuild the indexes, e.g. after a collation changed"
            }
            (Task::Reindex, _) => "Rebuild the table's indexes, e.g. invalid or bloated ones",
            (Task::Optimize, _) => {
                "Rebuild the table and its indexes, giving reclaimable space back"
            }
            (Task::Check, _) => "Check the table and its indexes for errors",
            (Task::UpdateStatistics, _) => "Fresh statistics of the table's indexes and columns",
            (Task::RebuildIndexes, _) => "Rebuild every index of the table (heavily fragmented)",
            (Task::ReorganizeIndexes, _) => {
                "Defragment the leaf pages of every index (lightly fragmented)"
            }
        }
    }
}

/// The tasks for an object, in the order offered.
pub fn tasks(dialect: Dialect, object_type: ObjectType) -> &'static [Task] {
    use Task::*;
    match (dialect.kind(), object_type) {
        (DbKind::Oracle, ObjectType::Table) => &[
            GatherStats,
            DeleteStats,
            LockStats,
            UnlockStats,
            RebuildUnusable,
            Shrink,
            Move,
        ],
        (DbKind::Oracle, ObjectType::MaterializedView) => &[
            GatherStats,
            DeleteStats,
            LockStats,
            UnlockStats,
            RebuildUnusable,
        ],
        (DbKind::Oracle, ObjectType::Index) => &[GatherStats, DeleteStats, RebuildIndex, Shrink],
        (DbKind::Postgres, ObjectType::Table | ObjectType::MaterializedView) => {
            &[Vacuum, Analyze, Reindex]
        }
        (DbKind::MySql, ObjectType::Table) => &[Analyze, Optimize, Check],
        (DbKind::SqlServer, ObjectType::Table) => {
            &[UpdateStatistics, RebuildIndexes, ReorganizeIndexes]
        }
        (DbKind::Sqlite, ObjectType::Table) => &[Analyze, Reindex, Check],
        (DbKind::Sqlite, ObjectType::Index) => &[Analyze, Reindex],
        _ => &[],
    }
}

/// Whether an object has maintenance tasks (the browser's menu).
pub fn has_tasks(dialect: Dialect, object_type: ObjectType) -> bool {
    !tasks(dialect, object_type).is_empty()
}

/// The options of every task; each task reads the ones it has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    /// Statistics or shrinking of the indexes too.
    pub cascade: bool,
    /// Oracle's sample size in percent; none for the automatic one.
    pub sample: Option<u32>,
    /// Invalidate dependent cursors at once instead of over a while.
    pub invalidate_now: bool,
    /// Gather or delete locked statistics.
    pub force: bool,
    pub online: bool,
    /// Shrink without releasing the space (no lock at the end).
    pub compact: bool,
    /// Move into this tablespace (empty: stay).
    pub tablespace: String,
    /// A partitioned index: only its unusable partitions.
    pub unusable_only: bool,
    pub analyze: bool,
    pub full: bool,
    pub freeze: bool,
    pub verbose: bool,
    pub concurrently: bool,
    pub fullscan: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            cascade: true,
            sample: None,
            invalidate_now: false,
            force: false,
            online: true,
            compact: false,
            tablespace: String::new(),
            unusable_only: true,
            analyze: true,
            full: false,
            freeze: false,
            verbose: false,
            concurrently: true,
            fullscan: false,
        }
    }
}

/// Whether the task can run online here.
pub fn online_possible(task: Task, facts: &Facts) -> bool {
    match task {
        // MOVE ONLINE came with 12.2 (an IOT could before).
        Task::Move => facts.online && (facts.release >= (12, 2) || facts.iot),
        _ => facts.online,
    }
}

/// Whether PostgreSQL can reindex without blocking writes (12+).
pub fn concurrently_possible(facts: &Facts) -> bool {
    facts.release >= (12, 0)
}

/// The statements the task runs, or why there is nothing to run.
pub fn statements(
    dialect: Dialect,
    object: &ObjectRef,
    task: Task,
    options: &Options,
    facts: &Facts,
) -> Result<Vec<String>, String> {
    let name = dialect.qualified(&object.owner, &object.name);
    let online = options.online && online_possible(task, facts);
    let index = object.object_type == ObjectType::Index;
    Ok(match task {
        Task::GatherStats => {
            let mut args = stats_target(object);
            if !index {
                args.push(format!("cascade => {}", options.cascade));
            }
            if let Some(p) = options.sample {
                args.push(format!("estimate_percent => {}", p.clamp(1, 100)));
            }
            if options.invalidate_now {
                args.push("no_invalidate => false".into());
            }
            if options.force && facts.stats_locked {
                args.push("force => true".into());
            }
            let call = if index {
                "gather_index_stats"
            } else {
                "gather_table_stats"
            };
            vec![block(call, &args)]
        }
        Task::DeleteStats => {
            let mut args = stats_target(object);
            if options.force && facts.stats_locked {
                args.push("force => true".into());
            }
            let call = if index {
                "delete_index_stats"
            } else {
                "delete_table_stats"
            };
            vec![block(call, &args)]
        }
        Task::LockStats => {
            if facts.stats_locked {
                return Err("The statistics are locked already".into());
            }
            vec![block("lock_table_stats", &stats_target(object))]
        }
        Task::UnlockStats => {
            if !facts.stats_locked {
                return Err("The statistics aren't locked".into());
            }
            vec![block("unlock_table_stats", &stats_target(object))]
        }
        Task::RebuildUnusable => {
            let unusable: Vec<String> = facts
                .segments
                .iter()
                .filter(|s| s.unusable)
                .map(|s| rebuild(dialect, s, online))
                .collect();
            if unusable.is_empty() {
                return Err("No index or index partition is unusable".into());
            }
            unusable
        }
        Task::RebuildIndex => {
            if facts.index_type == "IOT - TOP" {
                return Err(
                    "The index of an index-organized table is rebuilt by moving the table".into(),
                );
            }
            let only_unusable = facts.partitioned && options.unusable_only;
            let out: Vec<String> = facts
                .segments
                .iter()
                .filter(|s| !only_unusable || s.unusable)
                .map(|s| rebuild(dialect, s, online))
                .collect();
            if out.is_empty() {
                return Err(if only_unusable {
                    "No partition of the index is unusable".into()
                } else {
                    "The index has no segments to rebuild".into()
                });
            }
            out
        }
        Task::Shrink if index => {
            let compact = if options.compact { " compact" } else { "" };
            vec![format!("alter index {name} shrink space{compact}")]
        }
        Task::Shrink => {
            if facts.temporary {
                return Err("A temporary table has no space to give back".into());
            }
            let compact = if options.compact { " compact" } else { "" };
            let cascade = if options.cascade { " cascade" } else { "" };
            let shrink = format!("alter table {name} shrink space{compact}{cascade}");
            // Shrinking moves rows (heap tables): row movement for as long
            // as it takes.
            if facts.row_movement || facts.iot {
                vec![shrink]
            } else {
                vec![
                    format!("alter table {name} enable row movement"),
                    shrink,
                    format!("alter table {name} disable row movement"),
                ]
            }
        }
        Task::Move => {
            if facts.temporary {
                return Err("A temporary table has no segment to move".into());
            }
            if facts.partitioned {
                return Err(
                    "A partitioned table moves partition by partition (ALTER TABLE … MOVE \
                     PARTITION)"
                        .into(),
                );
            }
            let tablespace = crate::object_editor::ident(&options.tablespace);
            let tablespace = if tablespace.is_empty() {
                String::new()
            } else {
                format!(" tablespace {}", dialect.quote_ident(&tablespace))
            };
            let mode = if online { " online" } else { "" };
            let mut out = vec![format!("alter table {name} move{mode}{tablespace}")];
            // An offline move leaves the indexes unusable (an IOT's
            // secondary indexes stay usable).
            if !online && !facts.iot {
                out.extend(facts.segments.iter().map(|s| rebuild(dialect, s, false)));
            }
            out
        }
        // The whole file: `object` names the database.
        Task::Vacuum if dialect.kind() == DbKind::Sqlite => {
            vec![format!("vacuum {}", dialect.quote_ident(&object.owner))]
        }
        Task::Vacuum => {
            let mut flags = Vec::new();
            if options.full {
                flags.push("full");
            }
            if options.freeze {
                flags.push("freeze");
            }
            if options.verbose {
                flags.push("verbose");
            }
            if options.analyze {
                flags.push("analyze");
            }
            if flags.is_empty() {
                vec![format!("vacuum {name}")]
            } else {
                vec![format!("vacuum ({}) {name}", flags.join(", "))]
            }
        }
        Task::Analyze if dialect.kind() == DbKind::MySql => vec![format!("analyze table {name}")],
        Task::Analyze => {
            let verbose = if options.verbose { "verbose " } else { "" };
            vec![format!("analyze {verbose}{name}")]
        }
        Task::Reindex if dialect.kind() == DbKind::Sqlite => vec![format!("reindex {name}")],
        Task::Reindex => {
            let concurrently = if options.concurrently && concurrently_possible(facts) {
                "concurrently "
            } else {
                ""
            };
            vec![format!("reindex table {concurrently}{name}")]
        }
        Task::Optimize => vec![format!("optimize table {name}")],
        // Rows that say what is wrong, or `ok`.
        Task::Check if dialect.kind() == DbKind::Sqlite => vec![format!(
            "pragma {}.integrity_check({})",
            dialect.quote_ident(&object.owner),
            dialect.quote_ident(&object.name)
        )],
        Task::Check => vec![format!("check table {name}")],
        Task::UpdateStatistics => {
            let fullscan = if options.fullscan {
                " with fullscan"
            } else {
                ""
            };
            vec![format!("update statistics {name}{fullscan}")]
        }
        Task::RebuildIndexes | Task::ReorganizeIndexes if facts.indexes == 0 => {
            return Err("The table has no indexes".into());
        }
        Task::RebuildIndexes => {
            let with = if online { " with (online = on)" } else { "" };
            vec![format!("alter index all on {name} rebuild{with}")]
        }
        Task::ReorganizeIndexes => vec![format!("alter index all on {name} reorganize")],
    })
}

/// `ownname => '…', tabname => '…'` (or `indname`) of a DBMS_STATS call.
fn stats_target(object: &ObjectRef) -> Vec<String> {
    let what = if object.object_type == ObjectType::Index {
        "indname"
    } else {
        "tabname"
    };
    vec![
        format!("ownname => {}", literal(&object.owner)),
        format!("{what} => {}", literal(&object.name)),
    ]
}

/// A block calling a DBMS_STATS procedure.
fn block(procedure: &str, args: &[String]) -> String {
    format!(
        "begin\n  dbms_stats.{procedure}({});\nend;",
        args.join(", ")
    )
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Rebuilds an index segment: the index, or one of its (sub)partitions.
pub(super) fn rebuild(dialect: Dialect, segment: &IndexSegment, online: bool) -> String {
    let index = dialect.qualified(&segment.owner, &segment.index);
    let part = match &segment.part {
        Part::Whole => String::new(),
        Part::Partition(p) => format!(" partition {}", dialect.quote_ident(p)),
        Part::Subpartition(p) => format!(" subpartition {}", dialect.quote_ident(p)),
    };
    let online = if online { " online" } else { "" };
    format!("alter index {index} rebuild{part}{online}")
}

/// Whether the task keeps others from changing (or reading) the object
/// while it runs: its cost is shown as a warning.
pub fn blocks(task: Task, options: &Options, facts: &Facts) -> bool {
    let online = options.online && online_possible(task, facts);
    match task {
        Task::RebuildUnusable | Task::RebuildIndex | Task::Move | Task::RebuildIndexes => !online,
        Task::Vacuum => options.full,
        Task::Reindex => !(options.concurrently && concurrently_possible(facts)),
        Task::Shrink => !options.compact,
        _ => false,
    }
}

/// What running the task costs: locks, commits, changing plans.
pub fn impact(dialect: Dialect, task: Task, options: &Options, facts: &Facts) -> String {
    let online = options.online && online_possible(task, facts);
    match task {
        Task::GatherStats => {
            let cursors = if options.invalidate_now {
                "cursors using the table are invalidated at once"
            } else {
                "their cursors are invalidated over a while (AUTO_INVALIDATE)"
            };
            format!(
                "Reads the table (or a sample) and commits. Plans of statements on the table \
                 may change: {cursors}."
            )
        }
        Task::DeleteStats => "Commits. Without statistics the optimizer samples the table \
                              when it parses a statement (dynamic sampling), until they are \
                              gathered again, e.g. by the nightly task."
            .into(),
        Task::LockStats => "Gathering, also by the nightly task, leaves the statistics as \
                            they are until they are unlocked (or gathered with force)."
            .into(),
        Task::UnlockStats => "The nightly task gathers them again when they are stale.".into(),
        Task::RebuildUnusable | Task::RebuildIndex if online => {
            "DDL, commits. Reads and changes go on while it rebuilds; it needs space for the \
             new index beside the old one."
                .into()
        }
        Task::RebuildUnusable | Task::RebuildIndex => "DDL, commits. Changes to the table \
                                                       wait until each rebuild is done."
            .into(),
        Task::Shrink => {
            let end = if options.compact {
                "Compact only: the space stays with the segment, and no lock is taken at the end."
            } else {
                "Releasing the space at the end locks the table briefly."
            };
            let rowids = if facts.row_movement || facts.iot {
                ""
            } else {
                " Row movement is enabled for it and disabled again."
            };
            format!(
                "DDL, commits. Moves rows within the segment, so their ROWIDs change.{rowids} \
                 {end} Needs an ASSM tablespace; Oracle refuses some tables (e.g. with \
                 function-based indexes)."
            )
        }
        Task::Move if online => "DDL, commits. Reads and changes go on while it moves, and \
                                 the indexes stay usable; it needs space for a second copy."
            .into(),
        Task::Move => "DDL, commits. Changes to the table wait until it has moved, and its \
                       indexes are unusable until they are rebuilt (the statements after the \
                       move): queries using them fail meanwhile."
            .into(),
        Task::Vacuum if options.full => "FULL rewrites the table under an exclusive lock: \
                                         nothing reads or writes it until it's done, and it \
                                         needs space for a copy."
            .into(),
        Task::Vacuum => {
            let freeze = if options.freeze {
                " FREEZE writes every page that has rows to freeze."
            } else {
                ""
            };
            format!("Runs beside reads and writes.{freeze}")
        }
        Task::Analyze if dialect.kind() == DbKind::MySql => {
            "Reads index pages; plans may change. Waits for statements using the table, as \
             it reloads the table's definition."
                .into()
        }
        Task::Analyze => "Reads a sample, beside reads and writes. Plans may change.".into(),
        Task::Reindex if options.concurrently && concurrently_possible(facts) => {
            "Builds new indexes beside reads and writes, then swaps them in; takes longer and \
             needs space for the new ones."
                .into()
        }
        Task::Reindex => "Blocks writes to the table, and reads that use an index, until \
                          it's done."
            .into(),
        Task::Optimize => "InnoDB rebuilds the table (as ALTER TABLE … FORCE) and analyzes \
                           it: reads and writes go on, but it needs space for a copy and \
                           locks the table briefly at the start and the end."
            .into(),
        Task::Check => "Reads the whole table and its indexes.".into(),
        Task::UpdateStatistics => {
            let scan = if options.fullscan {
                "Reads the whole table."
            } else {
                "Reads a sample."
            };
            format!("{scan} Plans using the statistics are compiled again.")
        }
        Task::RebuildIndexes if online => "Rebuilds each index beside reads and writes, with \
                                           space for a copy; updates their statistics with a \
                                           full scan."
            .into(),
        Task::RebuildIndexes => "Each rebuild locks the table against changes (the clustered \
                                 index's against reads too); updates their statistics with a \
                                 full scan."
            .into(),
        Task::ReorganizeIndexes => "Online and light: compacts and orders the leaf pages. Can \
                                    be stopped any time, keeping what's done; doesn't update \
                                    statistics."
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(owner: &str, name: &str) -> ObjectRef {
        ObjectRef {
            owner: owner.into(),
            name: name.into(),
            object_type: ObjectType::Table,
        }
    }

    fn segment(index: &str, part: Part, unusable: bool) -> IndexSegment {
        IndexSegment {
            owner: "APP".into(),
            index: index.into(),
            part,
            unusable,
        }
    }

    fn oracle(facts: &Facts, object: &ObjectRef, task: Task, options: &Options) -> Vec<String> {
        statements(Dialect::ORACLE, object, task, options, facts).unwrap()
    }

    #[test]
    fn gather_statistics() {
        let orders = table("APP", "ORDERS");
        let facts = Facts::default();
        assert_eq!(
            oracle(&facts, &orders, Task::GatherStats, &Options::default()),
            [
                "begin\n  dbms_stats.gather_table_stats(ownname => 'APP', tabname => 'ORDERS', \
              cascade => true);\nend;"
            ]
        );
        let options = Options {
            cascade: false,
            sample: Some(10),
            invalidate_now: true,
            force: true,
            ..Options::default()
        };
        // Force only matters for locked statistics.
        let locked = Facts {
            stats_locked: true,
            ..Facts::default()
        };
        assert_eq!(
            oracle(&locked, &orders, Task::GatherStats, &options),
            [
                "begin\n  dbms_stats.gather_table_stats(ownname => 'APP', tabname => 'ORDERS', \
              cascade => false, estimate_percent => 10, no_invalidate => false, \
              force => true);\nend;"
            ]
        );
        assert!(!oracle(&facts, &orders, Task::GatherStats, &options)[0].contains("force"));
        let index = ObjectRef {
            object_type: ObjectType::Index,
            name: "ORDERS_IX".into(),
            ..orders.clone()
        };
        assert_eq!(
            oracle(&facts, &index, Task::GatherStats, &Options::default()),
            [
                "begin\n  dbms_stats.gather_index_stats(ownname => 'APP', indname => \
              'ORDERS_IX');\nend;"
            ]
        );
        // Names go in as the dictionary has them.
        let odd = table("APP", "it's");
        assert!(
            oracle(&facts, &odd, Task::DeleteStats, &Options::default())[0]
                .contains("tabname => 'it''s'")
        );
    }

    #[test]
    fn lock_and_unlock_follow_the_state() {
        let orders = table("APP", "ORDERS");
        let unlocked = Facts::default();
        let locked = Facts {
            stats_locked: true,
            ..Facts::default()
        };
        let o = Options::default();
        assert!(statements(Dialect::ORACLE, &orders, Task::UnlockStats, &o, &unlocked).is_err());
        assert!(statements(Dialect::ORACLE, &orders, Task::LockStats, &o, &locked).is_err());
        assert_eq!(
            oracle(&locked, &orders, Task::UnlockStats, &o),
            [
                "begin\n  dbms_stats.unlock_table_stats(ownname => 'APP', tabname => \
              'ORDERS');\nend;"
            ]
        );
    }

    #[test]
    fn rebuilds_unusable_segments() {
        let orders = table("APP", "ORDERS");
        let facts = Facts {
            online: true,
            segments: vec![
                segment("ORDERS_PK", Part::Whole, false),
                segment("ORDERS_IX", Part::Whole, true),
                segment("ORDERS_LX", Part::Partition("P2024".into()), true),
                segment("ORDERS_CX", Part::Subpartition("SYS_SUBP41".into()), true),
            ],
            ..Facts::default()
        };
        assert_eq!(
            oracle(&facts, &orders, Task::RebuildUnusable, &Options::default()),
            [
                "alter index APP.ORDERS_IX rebuild online",
                "alter index APP.ORDERS_LX rebuild partition P2024 online",
                "alter index APP.ORDERS_CX rebuild subpartition SYS_SUBP41 online",
            ]
        );
        // Standard Edition can't.
        let offline = Facts {
            online: false,
            ..facts.clone()
        };
        assert_eq!(
            oracle(
                &offline,
                &orders,
                Task::RebuildUnusable,
                &Options::default()
            )[0],
            "alter index APP.ORDERS_IX rebuild"
        );
        let fine = Facts {
            segments: vec![segment("ORDERS_PK", Part::Whole, false)],
            ..Facts::default()
        };
        assert!(
            statements(
                Dialect::ORACLE,
                &orders,
                Task::RebuildUnusable,
                &Options::default(),
                &fine
            )
            .is_err()
        );
    }

    #[test]
    fn rebuilds_an_index() {
        let index = ObjectRef {
            owner: "APP".into(),
            name: "ORDERS_LX".into(),
            object_type: ObjectType::Index,
        };
        let facts = Facts {
            partitioned: true,
            segments: vec![
                segment("ORDERS_LX", Part::Partition("P1".into()), false),
                segment("ORDERS_LX", Part::Partition("P2".into()), true),
            ],
            ..Facts::default()
        };
        let all = Options {
            unusable_only: false,
            online: false,
            ..Options::default()
        };
        assert_eq!(
            oracle(&facts, &index, Task::RebuildIndex, &all),
            [
                "alter index APP.ORDERS_LX rebuild partition P1",
                "alter index APP.ORDERS_LX rebuild partition P2",
            ]
        );
        assert_eq!(
            oracle(&facts, &index, Task::RebuildIndex, &Options::default()),
            ["alter index APP.ORDERS_LX rebuild partition P2"]
        );
        let iot = Facts {
            index_type: "IOT - TOP".into(),
            ..Facts::default()
        };
        assert!(
            statements(
                Dialect::ORACLE,
                &index,
                Task::RebuildIndex,
                &Options::default(),
                &iot
            )
            .is_err()
        );
    }

    #[test]
    fn shrink_enables_row_movement_for_as_long_as_it_takes() {
        let orders = table("APP", "ORDERS");
        let facts = Facts::default();
        assert_eq!(
            oracle(&facts, &orders, Task::Shrink, &Options::default()),
            [
                "alter table APP.ORDERS enable row movement",
                "alter table APP.ORDERS shrink space cascade",
                "alter table APP.ORDERS disable row movement",
            ]
        );
        let moving = Facts {
            row_movement: true,
            ..Facts::default()
        };
        let compact = Options {
            compact: true,
            cascade: false,
            ..Options::default()
        };
        assert_eq!(
            oracle(&moving, &orders, Task::Shrink, &compact),
            ["alter table APP.ORDERS shrink space compact"]
        );
    }

    #[test]
    fn an_offline_move_rebuilds_the_indexes() {
        let orders = table("APP", "ORDERS");
        let facts = Facts {
            online: true,
            release: (12, 1),
            segments: vec![
                segment("ORDERS_PK", Part::Whole, false),
                segment("ORDERS_GX", Part::Partition("P1".into()), false),
            ],
            ..Facts::default()
        };
        let options = Options {
            tablespace: "users".into(),
            ..Options::default()
        };
        // ONLINE needs 12.2.
        assert_eq!(
            oracle(&facts, &orders, Task::Move, &options),
            [
                "alter table APP.ORDERS move tablespace USERS",
                "alter index APP.ORDERS_PK rebuild",
                "alter index APP.ORDERS_GX rebuild partition P1",
            ]
        );
        let newer = Facts {
            release: (19, 0),
            ..facts.clone()
        };
        assert_eq!(
            oracle(&newer, &orders, Task::Move, &options),
            ["alter table APP.ORDERS move online tablespace USERS"]
        );
        let partitioned = Facts {
            partitioned: true,
            ..newer
        };
        assert!(statements(Dialect::ORACLE, &orders, Task::Move, &options, &partitioned).is_err());
    }

    #[test]
    fn postgres() {
        let orders = table("public", "Orders");
        let facts = Facts {
            release: (17, 2),
            ..Facts::default()
        };
        let run = |task, options: &Options| {
            statements(Dialect::POSTGRES, &orders, task, options, &facts).unwrap()
        };
        assert_eq!(
            run(Task::Vacuum, &Options::default()),
            ["vacuum (analyze) public.\"Orders\""]
        );
        let all = Options {
            full: true,
            freeze: true,
            verbose: true,
            analyze: false,
            ..Options::default()
        };
        assert_eq!(
            run(Task::Vacuum, &all),
            ["vacuum (full, freeze, verbose) public.\"Orders\""]
        );
        let plain = Options {
            analyze: false,
            ..Options::default()
        };
        assert_eq!(run(Task::Vacuum, &plain), ["vacuum public.\"Orders\""]);
        assert_eq!(
            run(Task::Analyze, &Options::default()),
            ["analyze public.\"Orders\""]
        );
        assert_eq!(
            run(Task::Reindex, &Options::default()),
            ["reindex table concurrently public.\"Orders\""]
        );
        // CONCURRENTLY came with 12.
        let old = Facts {
            release: (11, 0),
            ..Facts::default()
        };
        assert_eq!(
            statements(
                Dialect::POSTGRES,
                &orders,
                Task::Reindex,
                &Options::default(),
                &old
            )
            .unwrap(),
            ["reindex table public.\"Orders\""]
        );
    }

    #[test]
    fn mysql_and_sql_server() {
        let orders = table("shop", "order");
        let facts = Facts::default();
        let o = Options::default();
        assert_eq!(
            statements(Dialect::MYSQL, &orders, Task::Analyze, &o, &facts).unwrap(),
            ["analyze table shop.`order`"]
        );
        assert_eq!(
            statements(Dialect::MYSQL, &orders, Task::Optimize, &o, &facts).unwrap(),
            ["optimize table shop.`order`"]
        );
        let orders = table("dbo", "Orders");
        let indexed = Facts {
            indexes: 2,
            online: true,
            ..Facts::default()
        };
        let ms = |task, options: &Options, facts: &Facts| {
            statements(Dialect::SQLSERVER, &orders, task, options, facts)
        };
        let fullscan = Options {
            fullscan: true,
            ..Options::default()
        };
        assert_eq!(
            ms(Task::UpdateStatistics, &fullscan, &indexed).unwrap(),
            ["update statistics dbo.Orders with fullscan"]
        );
        assert_eq!(
            ms(Task::RebuildIndexes, &o, &indexed).unwrap(),
            ["alter index all on dbo.Orders rebuild with (online = on)"]
        );
        // Online needs Enterprise Edition.
        let standard = Facts {
            online: false,
            ..indexed.clone()
        };
        assert_eq!(
            ms(Task::RebuildIndexes, &o, &standard).unwrap(),
            ["alter index all on dbo.Orders rebuild"]
        );
        assert_eq!(
            ms(Task::ReorganizeIndexes, &o, &indexed).unwrap(),
            ["alter index all on dbo.Orders reorganize"]
        );
        assert!(ms(Task::RebuildIndexes, &o, &facts).is_err());
    }

    #[test]
    fn every_task_offered_has_statements_and_a_cost() {
        let facts = Facts {
            stats_locked: false,
            indexes: 1,
            segments: vec![segment("X", Part::Whole, true)],
            ..Facts::default()
        };
        for dialect in [
            Dialect::ORACLE,
            Dialect::POSTGRES,
            Dialect::MYSQL,
            Dialect::SQLSERVER,
        ] {
            for t in [
                ObjectType::Table,
                ObjectType::MaterializedView,
                ObjectType::Index,
            ] {
                let object = ObjectRef {
                    owner: "o".into(),
                    name: "n".into(),
                    object_type: t,
                };
                for &task in tasks(dialect, t) {
                    if task == Task::UnlockStats {
                        continue;
                    }
                    let s = statements(dialect, &object, task, &Options::default(), &facts);
                    assert!(s.is_ok_and(|s| !s.is_empty()), "{dialect:?} {t:?} {task:?}");
                    assert!(!impact(dialect, task, &Options::default(), &facts).is_empty());
                }
            }
        }
    }
}
