//! Minimal timestamped logging: "dd-mm-yyyy hh24:mi:ss: <text>".

use chrono::Local;
use std::fmt::Display;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::constants::TIME_FORMAT;

/// Set once by `init_file`; when present every log line is also written here.
static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

fn timestamp() -> String {
    Local::now().format(TIME_FORMAT).to_string()
}

/// <exe name>.log in the same folder as the exe (e.g. sqlplus_exec.exe -> sqlplus_exec.log).
fn log_file_path() -> std::io::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_extension("log"))
}

/// Creates the log file, discarding the content of any previous run, and
/// from now on mirrors all log lines into it. Returns the file's path.
pub fn init_file() -> std::io::Result<PathBuf> {
    let path = log_file_path()?;
    let file = File::create(&path)?; // truncates
    let _ = LOG_FILE.set(Mutex::new(file));
    Ok(path)
}

fn write_to_file(line: &str) {
    if let Some(file) = LOG_FILE.get() {
        if let Ok(mut f) = file.lock() {
            let _ = writeln!(f, "{line}"); // logging must never break the server
        }
    }
}

/// Log a line to stdout (and the log file, if enabled).
pub fn log(msg: impl Display) {
    let line = format!("{}: {msg}", timestamp());
    // Never panic on a failed write (e.g. closed stdout): logging must not break the server.
    let _ = writeln!(std::io::stdout(), "{line}");
    write_to_file(&line);
}

/// Log a line to stderr (and the log file, if enabled).
pub fn elog(msg: impl Display) {
    let line = format!("{}: {msg}", timestamp());
    let _ = writeln!(std::io::stderr(), "{line}");
    write_to_file(&line);
}

/// Reduces an Oracle connection string (username/password@tnsname) to
/// "username@tnsname" so the password never reaches the log.
/// Handles quoted passwords ("pa@ss/word") and EZConnect/descriptor
/// tns names (host:1521/service). Never returns the password, even when
/// the string cannot be parsed.
pub fn safe_conn(conn: &str) -> String {
    let conn = conn.trim();

    let (user, rest) = match conn.split_once('/') {
        Some((user, rest)) => (user, Some(rest)),
        None => {
            // No password part: "user@tns" or just "user".
            return match conn.split_once('@') {
                Some((user, tns)) => format!("{}@{}", user.trim(), tns.trim()),
                None => conn.to_string(),
            };
        }
    };

    let rest = rest.unwrap_or("");
    let after_password = if let Some(quoted) = rest.strip_prefix('"') {
        // Quoted password: skip to the closing quote.
        quoted.find('"').map(|i| &quoted[i + 1..])
    } else {
        rest.find('@').map(|i| &rest[i..])
    };

    match after_password.and_then(|s| s.strip_prefix('@')) {
        Some(tns) => format!("{}@{}", user.trim(), tns.trim()),
        None => user.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_conn_cases() {
        assert_eq!(safe_conn("scott/tiger@db1"), "scott@db1");
        assert_eq!(safe_conn("  scott/tiger@db1 "), "scott@db1");
        assert_eq!(safe_conn("scott/\"ti@ger/x\"@db1"), "scott@db1");
        assert_eq!(safe_conn("scott/tiger@host:1521/svc"), "scott@host:1521/svc");
        assert_eq!(safe_conn("scott/tiger"), "scott");
        assert_eq!(safe_conn("scott@db1"), "scott@db1");
        assert_eq!(safe_conn("scott/tiger@db1 as sysdba"), "scott@db1 as sysdba");
        assert_eq!(safe_conn("/@db1"), "@db1");
        assert!(!safe_conn("scott/s3cret").contains("s3cret"));
        assert!(!safe_conn("scott/\"s3cret").contains("s3cret"));
    }
}
