//! Every constant of the program, in one place. Grouped by area.

use std::time::Duration;

// ---------------------------------------------------------------------------
// Time and text formats
// ---------------------------------------------------------------------------

/// chrono format of every timestamp shown or logged: dd-mm-yyyy hh24:mi:ss.
pub const TIME_FORMAT: &str = "%d-%m-%Y %H:%M:%S";

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Config file name, looked up next to the exe unless --configfile is given.
pub const CONFIG_FILENAME: &str = "sqlPlusExec.config";

/// Config.max_sessions when the config file does not set it.
pub const DEFAULT_MAX_SESSIONS: usize = 10;

/// Used when Config.sqlplus_cmd is empty (sqlplus must be on PATH).
pub const DEFAULT_SQLPLUS: &str = "sqlplus";

// ---------------------------------------------------------------------------
// sqlplus
// ---------------------------------------------------------------------------

/// ROLLBACK is a SQL statement (not a SQL*Plus command), so it needs the
/// terminator; without it sqlplus keeps waiting for more of the statement
/// and swallows the end marker that follows.
pub const ROLLBACK: &str = "rollback;";

/// Run after every new db connect, AFTER Config.sqlplus_login, so these
/// hardcoded settings always win over whatever the user's login lines set.
pub const DEFAULT_LOGIN_CMDS: [&str; 5] = [
    "set linesize 512",
    "col error for a220",
    "set timing off",
    "set serveroutput on",
    "set feedback on",
    // No "&variable" prompts: they would read the end marker from stdin
    // and the command would never finish.
    //"set define off",
];

/// Marker lines printed by sqlplus before/after every command. A per-command
/// counter is appended, e.g. "__SQLPLUS_EXEC_START_7__", so each command's
/// output can be told apart from startup noise and any other stray output.
pub const START_MARKER_PREFIX: &str = "__SQLPLUS_EXEC_START_";
pub const END_MARKER_PREFIX: &str = "__SQLPLUS_EXEC_END_";

/// How long a graceful exit may take before the process is killed.
pub const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Session name used when a client does not pass --session.
pub const DEFAULT_SESSION: &str = "default";

/// How many lines of a session's last output are kept for the --ui screen.
pub const MAX_OUTPUT_LINES: usize = 200;

// ---------------------------------------------------------------------------
// Named pipe and client
// ---------------------------------------------------------------------------

/// Pipe name used if the exe name cannot be determined.
pub const FALLBACK_PIPE_NAME: &str = r"\\.\pipe\sqlPlusExec-rs";

/// Windows error code: all pipe instances are busy.
#[cfg(windows)]
pub const ERROR_PIPE_BUSY: i32 = 231;

/// Wait between attempts to open a pipe whose instances are all busy.
#[cfg(windows)]
pub const PIPE_BUSY_RETRY: Duration = Duration::from_millis(50);

/// How many times the client tries to start the server before giving up.
pub const MAX_SERVER_START_ATTEMPTS: u32 = 5;
/// After each start attempt, how long to wait for the pipe to come up.
pub const SERVER_START_WAIT: Duration = Duration::from_secs(3);
pub const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Printed when a client command finds no server (they never start one).
pub const SERVER_NOT_RUNNING: &str = "Server is not running.";

// ---------------------------------------------------------------------------
// Session table (--status and --ui)
// ---------------------------------------------------------------------------

pub const HEADINGS: [&str; 4] = ["SESSION", "STATE", "CONNECTION", "LAST USED"];

/// Shown as the name of the free session, and as the connection of a
/// session that has none.
pub const FREE_SESSION_NAME: &str = "-";
pub const NOT_CONNECTED: &str = "not connected";

// ---------------------------------------------------------------------------
// Terminal UI
// ---------------------------------------------------------------------------

/// Refresh interval when --ui has no value.
pub const DEFAULT_REFRESH_SECS: u64 = 5;
pub const MIN_REFRESH_SECS: u64 = 1;
pub const MAX_REFRESH_SECS: u64 = 60;

/// After starting or stopping the server, refresh faster for a while so the
/// screen follows the change.
pub const FAST_REFRESH: Duration = Duration::from_millis(500);
pub const FAST_REFRESH_WINDOW: Duration = Duration::from_secs(6);

/// A list request must answer quickly; release/kill/stop may take a few
/// seconds (the sqlplus process is given time to exit).
pub const LIST_TIMEOUT: Duration = Duration::from_secs(3);
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(15);

pub const STARTING: &str = "Starting the server...";
pub const PAGE_LINES: u16 = 10;
pub const SCROLL_COLUMNS: u16 = 8;
