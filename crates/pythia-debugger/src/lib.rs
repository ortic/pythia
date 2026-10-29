//! PL/SQL debugger for Pythia, built on `DBMS_DEBUG_JDWP`.
//!
//! The debugger listens on a TCP port; the session under test runs
//! `DBMS_DEBUG_JDWP.CONNECT_TCP(host, port)` and the database connects back
//! and is driven with JDWP. Oracle presents PL/SQL units as classes named
//! `$Oracle.<Kind>.<OWNER>.<NAME>` (anonymous blocks: `$Oracle.Block.<OWNER>.<hash>`),
//! code indexes equal source lines, and variables are objects of
//! `$Oracle.Builtin.<TYPE>` whose `_value` field holds the value as text.
//!
//! - [`Debugger`]: the debugger worker (UI side, channel based).
//! - [`target`]: running code under the debugger and debug compilation.
//! - [`setup`]: what the database needs to connect back (privileges,
//!   network ACL, a host it reaches), finding out what's missing and the
//!   SQL that allows it.
//! - [`sql`]: the SQL the stopped session ran, by line of code, and a
//!   statement's plan with estimated and actual rows.
//! - [`ui`]: an egui debugger panel and the breakpoint model.
//! - [`jdwp`]: the wire protocol.

mod condition;
mod debugger;
pub mod jdwp;
pub mod setup;
pub mod sql;
mod sql_panel;
pub mod target;
mod types;
pub mod ui;
pub mod value;

pub use debugger::Debugger;
pub use types::{
    Breakpoint, BreakpointOptions, DebugCommand, DebugEvent, Frame, Location, StopReason, Unit,
    UnitType, Variable, Watch,
};
