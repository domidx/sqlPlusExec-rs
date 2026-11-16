//! The language client and server speak, with no platform code in it:
//! the request format, the session list, and JSON-line framing. The pipe
//! itself is in `pipe.rs`.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::constants::{FREE_SESSION_NAME, HEADINGS, NOT_CONNECTED};

/// Request sent from client to server. One request, one meaning: the enum
/// makes a combination like "stop and status" impossible. (No `Debug` derive
/// on purpose: `conn` contains the password and must never end up in a log.)
#[derive(Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    /// Run `sqlcmd` lines on connection `conn` in a sqlplus session.
    Run {
        /// Oracle connection string: username/password@tnsname.
        conn: String,
        /// Lines to run in the sqlplus session.
        sqlcmd: Vec<String>,
        /// Name of the sqlplus session to run in; empty = "default".
        #[serde(default)]
        session: String,
    },
    /// The server shuts down.
    Stop,
    /// The server replies with its status as text.
    Status,
    /// The server replies with the session list as JSON (see `Listing`);
    /// this is what the --ui screen reads.
    List,
    /// Name of a session to release (roll back and mark free).
    Release(String),
    /// Name of a session to kill (terminate its sqlplus process at once).
    Kill(String),
}

impl Request {
    /// A `Run` request as built from the command line arguments.
    pub fn run(conn: &str, sqlcmd: &[String], session: &Option<String>) -> Self {
        Request::Run {
            conn: conn.to_string(),
            sqlcmd: sqlcmd.to_vec(),
            session: session.clone().unwrap_or_default(),
        }
    }
}

/// One session as reported to --status and --ui.
/// (No `Debug` derive: `command` may contain secrets.)
#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct SessionInfo {
    /// Session name; empty for the free session.
    pub name: String,
    pub free: bool,
    /// Seconds the running command has been running; None = idle.
    pub busy_secs: Option<i64>,
    /// "user@tns"; None = not connected.
    pub connection: Option<String>,
    /// dd-mm-yyyy hh24:mi:ss
    pub last_used: String,
    /// The command lines running now, or the last ones that ran.
    pub command: Vec<String>,
    /// The last lines of output of the last finished command.
    pub output: Vec<String>,
}

impl SessionInfo {
    /// "free", "idle" or "busy 12m05s".
    pub fn state_text(&self) -> String {
        if self.free {
            "free".to_string()
        } else {
            match self.busy_secs {
                Some(secs) => format!("busy {}", format_duration(secs)),
                None => "idle".to_string(),
            }
        }
    }

    /// The cells of this session's row in the --status / --ui table
    /// (same order as `HEADINGS`).
    pub fn row(&self) -> [String; 4] {
        [
            if self.free { FREE_SESSION_NAME.to_string() } else { self.name.clone() },
            self.state_text(),
            self.connection.clone().unwrap_or_else(|| NOT_CONNECTED.to_string()),
            self.last_used.clone(),
        ]
    }
}

/// Width of every column: its longest cell, heading included.
pub fn column_widths(rows: &[[String; 4]]) -> [usize; 4] {
    let mut widths = HEADINGS.map(|h| h.chars().count());
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    widths
}

/// The reply to a `list` request.
#[derive(Serialize, Deserialize, Default, PartialEq)]
pub struct Listing {
    pub max_sessions: usize,
    pub sessions: Vec<SessionInfo>,
}

/// "45s", "12m05s", "2h05m".
pub fn format_duration(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Serialize `value` as JSON and write it as a single newline-terminated line.
/// The newline acts as the message delimiter.
pub async fn write_message<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

/// Read one newline-terminated JSON message. Returns `Ok(None)` on EOF
/// (the other side closed the connection).
pub async fn read_message<R, T>(reader: &mut BufReader<R>) -> io::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        return Ok(None);
    }
    let value = serde_json::from_str(line.trim_end())?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_format_cases() {
        assert_eq!(format_duration(-5), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(125), "2m05s");
        assert_eq!(format_duration(3599), "59m59s");
        assert_eq!(format_duration(7500), "2h05m");
    }

    fn round_trip(request: &Request) -> Request {
        serde_json::from_str(&serde_json::to_string(request).unwrap()).unwrap()
    }

    #[test]
    fn every_request_survives_the_wire() {
        let run = Request::run("u/p@db", &["select 1;".to_string()], &Some("v1".to_string()));
        for request in [
            run,
            Request::Stop,
            Request::Status,
            Request::List,
            Request::Release("a".to_string()),
            Request::Kill("b".to_string()),
        ] {
            assert!(round_trip(&request) == request);
        }
    }

    #[test]
    fn run_without_a_session_uses_the_empty_name() {
        let Request::Run { conn, sqlcmd, session } = Request::run("u/p@db", &["x".to_string()], &None)
        else {
            panic!("not a Run request");
        };
        assert_eq!((conn.as_str(), session.as_str()), ("u/p@db", ""));
        assert_eq!(sqlcmd, ["x"]);
        // A client that leaves "session" out entirely is accepted as well.
        let json = r#"{"run":{"conn":"u/p@db","sqlcmd":[]}}"#;
        let Request::Run { session, .. } = serde_json::from_str(json).unwrap() else { panic!() };
        assert_eq!(session, "");
    }

    #[test]
    fn a_request_has_exactly_one_meaning() {
        // Two actions in one message do not parse; "stop and status" cannot exist.
        assert!(serde_json::from_str::<Request>(r#"{"stop":null,"status":null}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"nonsense":null}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#""stop""#).is_ok());
    }

    #[tokio::test]
    async fn messages_are_one_json_line_each() {
        let (mut a, b) = tokio::io::duplex(1024);
        write_message(&mut a, &Request::Release("x".to_string())).await.unwrap();
        write_message(&mut a, &Request::Stop).await.unwrap();
        drop(a);
        let mut reader = BufReader::new(b);
        assert!(read_message::<_, Request>(&mut reader).await.unwrap() == Some(Request::Release("x".to_string())));
        assert!(read_message::<_, Request>(&mut reader).await.unwrap() == Some(Request::Stop));
        assert!(read_message::<_, Request>(&mut reader).await.unwrap().is_none()); // EOF
    }

    fn info(name: &str, free: bool, conn: Option<&str>) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            free,
            connection: conn.map(String::from),
            last_used: "07-11-2026 16:40:31".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn row_cells() {
        assert_eq!(
            info("v1", false, Some("scott@db1")).row(),
            ["v1", "idle", "scott@db1", "07-11-2026 16:40:31"]
        );
        let free = info("", true, None).row();
        assert_eq!(&free[..3], ["-", "free", "not connected"]);
    }

    #[test]
    fn widths_cover_headings_and_cells() {
        // Short cells: the headings decide.
        let rows = [info("a", false, None).row()];
        assert_eq!(column_widths(&rows), [7, 5, 13, 19]);
        // Long cells: they decide.
        let rows = [info("a_very_long_name", false, Some("scott@db1")).row()];
        assert_eq!(column_widths(&rows)[0], 16);
        assert_eq!(column_widths(&[]), [7, 5, 10, 9]);
    }
}
