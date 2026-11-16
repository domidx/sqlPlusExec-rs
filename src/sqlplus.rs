//! A long-lived sqlplus subprocess driven through stdin/stdout.

use encoding_rs::Encoding;
use std::{io, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

use crate::constants::{END_MARKER_PREFIX, ROLLBACK, SHUTDOWN_WAIT, START_MARKER_PREFIX};

pub struct SqlPlus {
    /// The process lives as long as this struct (`kill_on_drop`).
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Incremented for every command; makes the markers unique.
    counter: u64,
    /// Character set sqlplus reads and writes (decoding never fails).
    encoding: &'static Encoding,
    /// Lowercased connection string of the current db session, if any.
    pub last_conn: Option<String>,
}

impl SqlPlus {
    pub fn spawn(sqlplus_cmd: &str, encoding: &'static Encoding) -> io::Result<Self> {
        // -S: silent (no banner/prompts), -L: don't re-prompt on login failure,
        // /nolog: start without connecting; a "connect ..." command follows.
        let mut child = Command::new(sqlplus_cmd)
            .args(["-S", "-L", "/nolog"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("failed to open sqlplus stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("failed to open sqlplus stdout"))?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            counter: 0,
            encoding,
            last_conn: None,
        })
    }

    /// Sends the lines to sqlplus and returns everything it printed in response.
    ///
    /// sqlplus gives no "done" signal, so the lines are wrapped between two
    /// unique marker lines (START_<n> / END_<n>). Only text between them is
    /// returned; anything before START_<n> (login-script output, stray
    /// output, stale markers) is discarded. There is deliberately no
    /// timeout: a command runs for as long as it needs.
    pub async fn execute(&mut self, lines: &[String]) -> io::Result<String> {
        self.counter += 1;
        let start = format!("{START_MARKER_PREFIX}{}__", self.counter);
        let end = format!("{END_MARKER_PREFIX}{}__", self.counter);

        let mut script = format!("PROMPT {start}\n");
        for line in lines {
            script.push_str(line);
            script.push('\n');
        }
        script.push_str(&format!("PROMPT {end}\n"));

        self.send(&script).await?;

        let mut output = String::new();
        let mut raw: Vec<u8> = Vec::new();
        let mut started = false;
        loop {
            raw.clear();
            // Read raw bytes: sqlplus output is not necessarily UTF-8, and
            // read_line() fails on invalid UTF-8.
            if self.stdout.read_until(b'\n', &mut raw).await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "sqlplus exited",
                ));
            }
            let (line, _) = self.encoding.decode_without_bom_handling(&raw);
            let text = line.trim_end();

            if !started {
                // Skip everything until our own START marker.
                started = text == start;
            } else if text == end {
                break;
            } else {
                output.push_str(&line);
            }
        }
        Ok(output.trim_end().to_string())
    }

    /// Encodes `script` in sqlplus's character set and writes it to stdin.
    async fn send(&mut self, script: &str) -> io::Result<()> {
        let (bytes, _, _) = self.encoding.encode(script);
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await
    }

    /// Ends the session gracefully: roll back any uncommitted work, then
    /// exit. If the process has not exited after a few seconds it is killed.
    pub async fn shutdown(mut self) {
        // "exitcommit off" makes sure the exit itself can never commit.
        let script = if self.last_conn.is_some() {
            format!("{ROLLBACK}\nset exitcommit off\nexit\n")
        } else {
            "exit\n".to_string()
        };
        let _ = self.send(&script).await;
        drop(self.stdin);

        if timeout(SHUTDOWN_WAIT, self.child.wait()).await.is_err() {
            let _ = self.child.start_kill();
        }
    }
}
