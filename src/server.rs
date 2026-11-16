use chrono::{DateTime, Local};
use std::sync::{Arc, MutexGuard};
use std::io;
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinSet;

use crate::config::Config;
use crate::constants::{
    DEFAULT_LOGIN_CMDS, DEFAULT_SESSION, HEADINGS, MAX_OUTPUT_LINES, ROLLBACK, TIME_FORMAT,
};
use crate::log::{elog, init_file, log, safe_conn};
use crate::pipe::{pipe_name, PipeListener};
use crate::protocol::{
    column_widths, read_message, write_message, Listing, Request, SessionInfo,
};
use crate::sqlplus::SqlPlus;

// ---------------------------------------------------------------------------
// Session registry
// ---------------------------------------------------------------------------

/// Bookkeeping of one session slot. Behind a plain (std) mutex that is never
/// held across an await, so it can always be read - e.g. by --status - even
/// while a long query holds the slot's session lock.
struct SlotMeta {
    /// The session name that owns this slot. None = a free slot: its sqlplus
    /// process is kept running, without a db connection, for the next new
    /// session to take over. (Also None briefly while a session is released,
    /// so requests still queued for it notice and start over.)
    owner: Option<String>,
    /// "user@tns" of the slot's current db connection.
    connected_as: Option<String>,
    /// When the command that is running right now started (None = idle).
    busy_since: Option<DateTime<Local>>,
    last_used: DateTime<Local>,
    /// The command lines running now, or the last ones that ran (for --ui).
    command: Vec<String>,
    /// The last lines of output of the last finished command (for --ui).
    output: Vec<String>,
}

/// One sqlplus session slot. The sqlplus process starts lazily and may be
/// absent (never started, or died).
struct Slot {
    session: Mutex<Option<SqlPlus>>,
    meta: std::sync::Mutex<SlotMeta>,
    /// Signalled by --kill to make a running request give up and drop the
    /// sqlplus process (the running request holds the session lock).
    kill: Notify,
}

impl Slot {
    fn new(owner: &str) -> Arc<Slot> {
        Arc::new(Slot {
            session: Mutex::new(None),
            meta: std::sync::Mutex::new(SlotMeta {
                owner: Some(owner.to_string()),
                connected_as: None,
                busy_since: None,
                last_used: Local::now(),
                command: Vec::new(),
                output: Vec::new(),
            }),
            kill: Notify::new(),
        })
    }

    fn meta(&self) -> MutexGuard<'_, SlotMeta> {
        self.meta.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_owned_by(&self, name: &str) -> bool {
        self.meta().owner.as_deref() == Some(name)
    }
}

/// State shared by all client connections.
struct ServerState {
    slots: std::sync::Mutex<Vec<Arc<Slot>>>,
    max_sessions: usize,
}

type SharedState = Arc<ServerState>;

impl ServerState {
    fn new(max_sessions: usize) -> Self {
        Self {
            slots: std::sync::Mutex::new(Vec::new()),
            max_sessions,
        }
    }

    fn slots(&self) -> MutexGuard<'_, Vec<Arc<Slot>>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Finds the slot for a session name: the one this name already owns,
    /// else the free slot (its sqlplus process is already running; the first
    /// request connects it), else a new one if the limit allows, else an error.
    fn acquire(&self, name: &str) -> Result<Arc<Slot>, String> {
        let mut slots = self.slots();

        if let Some(slot) = slots.iter().find(|s| s.is_owned_by(name)) {
            return Ok(Arc::clone(slot));
        }

        if let Some(slot) = slots.iter().find(|s| s.meta().owner.is_none()) {
            slot.meta().owner = Some(name.to_string());
            return Ok(Arc::clone(slot));
        }

        if slots.len() < self.max_sessions {
            let slot = Slot::new(name);
            slots.push(Arc::clone(&slot));
            return Ok(slot);
        }

        Err(format!(
            "Session limit reached ({n} of {n}), use --status and --release <name>",
            n = slots.len()
        ))
    }

    fn find_owned(&self, name: &str) -> Option<Arc<Slot>> {
        self.slots().iter().find(|s| s.is_owned_by(name)).cloned()
    }

    /// Takes the slot away from session `name` in one step (so nobody can
    /// claim it half-way): it is removed from the registry, except when
    /// `keep_if_last` is set and it is the only slot, where it stays as a
    /// free slot. Returns None if `name` no longer owns it, else whether the
    /// slot was kept.
    fn unown(&self, slot: &Arc<Slot>, name: &str, keep_if_last: bool) -> Option<bool> {
        let mut slots = self.slots();
        {
            let mut meta = slot.meta();
            if meta.owner.as_deref() != Some(name) {
                return None;
            }
            meta.owner = None;
        }
        if keep_if_last && slots.len() == 1 {
            return Some(true);
        }
        slots.retain(|s| !Arc::ptr_eq(s, slot));
        Some(false)
    }

    /// All sessions, as shown by --status and --ui.
    fn session_infos(&self) -> Vec<SessionInfo> {
        let slots: Vec<Arc<Slot>> = self.slots().clone();
        slots
            .iter()
            .map(|slot| {
                let m = slot.meta();
                SessionInfo {
                    name: m.owner.clone().unwrap_or_default(),
                    free: m.owner.is_none(),
                    busy_secs: m.busy_since.map(|since| (Local::now() - since).num_seconds()),
                    connection: m.connected_as.clone(),
                    last_used: m.last_used.format(TIME_FORMAT).to_string(),
                    command: m.command.clone(),
                    output: m.output.clone(),
                }
            })
            .collect()
    }

    /// The reply to a `list` request: all sessions as JSON.
    fn listing_json(&self) -> String {
        let listing = Listing {
            max_sessions: self.max_sessions,
            sessions: self.session_infos(),
        };
        serde_json::to_string(&listing).unwrap_or_default()
    }

    /// One line per session; always lists all of them.
    fn status_text(&self) -> String {
        let infos = self.session_infos();
        let mut out = format!(
            "Server running, sessions: {} of {}",
            infos.len(),
            self.max_sessions
        );
        if infos.is_empty() {
            out.push_str("\nNo sessions");
            return out;
        }

        // Header plus one row per session; the first three columns are as
        // wide as their longest value so long names stay aligned.
        let rows: Vec<[String; 4]> = infos.iter().map(SessionInfo::row).collect();
        let [w0, w1, w2, _] = column_widths(&rows);
        let header = HEADINGS.map(String::from);
        for r in std::iter::once(&header).chain(&rows) {
            out.push_str(&format!(
                "\n{:<w0$}  {:<w1$}  {:<w2$}  {}",
                r[0], r[1], r[2], r[3]
            ));
        }
        out
    }
}

/// The last `max` lines of `text`.
fn tail_lines(text: &str, max: usize) -> Vec<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let start = lines.len().saturating_sub(max);
    lines[start..]
        .iter()
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect()
}

fn session_name(raw: &str) -> &str {
    let name = raw.trim();
    if name.is_empty() {
        DEFAULT_SESSION
    } else {
        name
    }
}

// ---------------------------------------------------------------------------
// Server loop
// ---------------------------------------------------------------------------

/// Runs the server until a stop request arrives.
pub async fn server(cfg: Config) -> io::Result<()> {
    let mut listener = PipeListener::bind().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "cannot create pipe {} (is the server already running?): {e}",
                pipe_name()
            ),
        )
    })?;

    // Only now that this instance owns the pipe is it safe to reset the log
    // file; a second server that failed to bind must not wipe the log of
    // the running one.
    if cfg.enable_log_file {
        match init_file() {
            Ok(path) => log(format!("Logging to {}", path.display())),
            Err(e) => elog(format!("Cannot create log file, continuing without it: {e}")),
        }
    }

    let max_sessions = cfg.max_sessions;
    let cfg = Arc::new(cfg);
    let state: SharedState = Arc::new(ServerState::new(max_sessions));
    let shutdown = Arc::new(Notify::new());

    log(format!(
        "Server listening on {} (max {max_sessions} sessions)",
        pipe_name()
    ));

    loop {
        // Wait for either a client or the stop request.
        let accepted = tokio::select! {
            r = listener.accept() => Some(r),
            _ = shutdown.notified() => None,
        };
        let Some(accepted) = accepted else { break };
        let stream = accepted?;

        let cfg = Arc::clone(&cfg);
        let state = Arc::clone(&state);
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, cfg, state, shutdown).await {
                elog(format!("Connection error: {e}"));
            }
        });
    }

    // No new clients from here on.
    drop(listener);
    shutdown_sessions(&state).await;

    log("Server stopped.");
    Ok(())
}

/// Closes the sqlplus sessions gracefully: every idle session gets
/// "rollback" and "exit" (a session that was never connected just "exit").
/// Busy sessions are not touched; they are killed when the runtime drops.
async fn shutdown_sessions(state: &ServerState) {
    let slots: Vec<Arc<Slot>> = state.slots().clone();
    let mut closing = JoinSet::new();

    for slot in slots {
        let taken = match slot.session.try_lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => {
                log("A session is busy, it will be killed.");
                None
            }
        };
        if let Some(sqlplus) = taken {
            closing.spawn(sqlplus.shutdown());
        }
    }

    if !closing.is_empty() {
        log(format!("Closing {} sqlplus session(s)...", closing.len()));
    }
    while closing.join_next().await.is_some() {}
}

/// Serves one client connection; asks the accept loop to exit if the client
/// sent a stop request.
async fn handle_connection<S>(
    stream: S,
    cfg: Arc<Config>,
    state: SharedState,
    shutdown: Arc<Notify>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut stop_requested = false;
    let result = serve(stream, &cfg, &state, &mut stop_requested).await;
    if stop_requested {
        shutdown.notify_one();
    }
    result
}

/// Reads requests from the connection until the client disconnects.
async fn serve<S>(
    stream: S,
    cfg: &Config,
    state: &ServerState,
    stop_requested: &mut bool,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    while let Some(req) = read_message::<_, Request>(&mut reader).await? {
        let reply = reply_to(&req, cfg, state, stop_requested).await;
        write_message(&mut writer, &reply).await?;
    }
    Ok(())
}

/// The reply text for one request.
async fn reply_to(
    req: &Request,
    cfg: &Config,
    state: &ServerState,
    stop_requested: &mut bool,
) -> String {
    match req {
        Request::Stop => {
            log("Stop request received.");
            // Shut down only after the client has disconnected (i.e. has read
            // the reply), so the reply cannot be lost.
            *stop_requested = true;
            "Server stopping.".to_string()
        }
        Request::Status => state.status_text(),
        Request::List => state.listing_json(),
        Request::Release(name) => release_session(state, session_name(name)).await,
        Request::Kill(name) => kill_session(state, session_name(name)).await,
        Request::Run { conn, sqlcmd, session } => {
            let session = session_name(session);
            log(format!("[{session}] Request from {}:", safe_conn(conn)));
            for line in sqlcmd {
                log(format!("  {line}"));
            }
            process_request(state, cfg, session, conn, sqlcmd).await
        }
    }
}

// ---------------------------------------------------------------------------
// Request processing
// ---------------------------------------------------------------------------

/// Runs the request in the session's sqlplus process and returns its output.
/// The process is started on first use and restarted if it dies.
async fn process_request(
    state: &ServerState,
    cfg: &Config,
    session: &str,
    conn: &str,
    sqlcmd: &[String],
) -> String {
    loop {
        let slot = match state.acquire(session) {
            Ok(slot) => slot,
            Err(msg) => {
                elog(format!("[{session}] {msg}"));
                return format!("Error: {msg}");
            }
        };

        // Requests for the same session queue up here, in order.
        let mut guard = slot.session.lock().await;

        // The session may have been released (and re-assigned) while this
        // request was waiting; if so, ask the registry again.
        if !slot.is_owned_by(session) {
            continue;
        }

        if guard.is_none() {
            let cmd = cfg.sqlplus_command();
            match SqlPlus::spawn(cmd, cfg.encoding()) {
                Ok(s) => *guard = Some(s),
                Err(e) => return format!("Error: failed to start sqlplus [{cmd}]: {e}"),
            }
        }

        {
            let mut meta = slot.meta();
            meta.busy_since = Some(Local::now());
            meta.command = sqlcmd.to_vec();
            meta.output.clear();
        }

        let sqlplus = guard.as_mut().expect("session was just created");
        // A --kill request interrupts the wait; the run is then abandoned.
        let outcome = tokio::select! {
            r = run_request(sqlplus, &slot, cfg, session, conn, sqlcmd) => Some(r),
            _ = slot.kill.notified() => None,
        };

        let mut meta = slot.meta();
        meta.busy_since = None;
        meta.last_used = Local::now();
        let response = match outcome {
            Some(Ok(output)) => output,
            None => {
                // Killed: drop the process (kill_on_drop). kill_session has
                // already taken the session out of the registry.
                *guard = None;
                meta.connected_as = None;
                format!("Error: session [{session}] was killed")
            }
            Some(Err(e)) => {
                // Drop the dead process; the next request starts a fresh one
                // (and reconnects, since the new process has no connection).
                *guard = None;
                meta.connected_as = None;
                elog(format!("[{session}] sqlplus session failed: {e}"));
                format!("Error: sqlplus session failed: {e}")
            }
        };
        meta.output = tail_lines(&response, MAX_OUTPUT_LINES);
        return response;
    }
}

/// Connects if the connection string changed, then runs the request's lines.
async fn run_request(
    sqlplus: &mut SqlPlus,
    slot: &Slot,
    cfg: &Config,
    session: &str,
    conn: &str,
    sqlcmd: &[String],
) -> io::Result<String> {
    let conn = conn.trim();
    let conn_key = conn.to_lowercase();

    if sqlplus.last_conn.as_deref() != Some(conn_key.as_str()) {
        // Switching to another connection: discard whatever is left
        // uncommitted on the old one first. (Only if connected; otherwise
        // rollback would just print "not connected".)
        if sqlplus.last_conn.is_some() {
            sqlplus.execute(&[ROLLBACK.to_string()]).await?;
        }

        // A failed CONNECT leaves the session disconnected, so forget the
        // old connection until the new one is known to be good.
        sqlplus.last_conn = None;
        slot.meta().connected_as = None;

        log(format!("[{session}] Connecting as {}", safe_conn(conn)));
        let output = sqlplus.execute(&connect_lines(conn, cfg)).await?;
        if has_oracle_error(&output) {
            elog(format!("[{session}] Connect/login failed for {}", safe_conn(conn)));
            return Ok(format!("Error: connect/login failed:\n{output}"));
        }
        sqlplus.last_conn = Some(conn_key);
        slot.meta().connected_as = Some(safe_conn(conn));
    }

    let mut lines = Vec::with_capacity(sqlcmd.len() + 2);
    let is_select = is_select_command(sqlcmd);
    if is_select {
        lines.push("set markup csv on".to_string());
    }
    lines.extend(sqlcmd.iter().cloned());
    if is_select {
        lines.push("set markup csv off".to_string());
    }

    let output = sqlplus.execute(&lines).await?;
    Ok(clean_output(&output, is_select))
}

/// The lines run after a (re)connect: the connect itself, the user's login
/// lines, then the hardcoded defaults - last, so they win over anything the
/// login lines changed.
fn connect_lines(conn: &str, cfg: &Config) -> Vec<String> {
    let mut lines = vec![format!("connect {conn}")];
    lines.extend(cfg.sqlplus_login.iter().cloned());
    lines.extend(DEFAULT_LOGIN_CMDS.iter().map(|s| s.to_string()));
    lines
}

/// --release <name>: rolls back the session's uncommitted work and ends the
/// session. Normally its sqlplus process exits and the session is removed,
/// which frees its place under `max_sessions`. The last remaining session is
/// different: its process stays running, disconnected from the database, as
/// a free slot that the next new session takes over - so the next vim does
/// not have to start sqlplus again. A busy session cannot be released.
async fn release_session(state: &ServerState, name: &str) -> String {
    let Some(slot) = state.find_owned(name) else {
        return format!("No such session [{name}]");
    };

    let mut guard = match slot.session.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            return format!(
                "Error: session [{name}] is busy, cannot release it now (use --kill to terminate it)"
            )
        }
    };

    // Un-owned while we hold the session lock: a request for this name that
    // is still queued will notice and ask the registry again.
    let Some(kept) = state.unown(&slot, name, true) else {
        return format!("No such session [{name}]");
    };

    if kept {
        // "rollback;" first: DISCONNECT would otherwise commit pending work.
        if let Some(sqlplus) = guard.as_mut() {
            if sqlplus.last_conn.is_some() {
                let lines = [ROLLBACK.to_string(), "disconnect".to_string()];
                match sqlplus.execute(&lines).await {
                    Ok(_) => sqlplus.last_conn = None,
                    Err(e) => {
                        *guard = None;
                        elog(format!("[{name}] sqlplus session failed during release: {e}"));
                    }
                }
            }
        }
        let mut meta = slot.meta();
        meta.connected_as = None;
        meta.command.clear(); // the next owner must not see the old user's work
        meta.output.clear();
        meta.last_used = Local::now();
        log(format!("[{name}] Released (kept as a free session)."));
    } else {
        if let Some(sqlplus) = guard.take() {
            sqlplus.shutdown().await;
        }
        log(format!("[{name}] Released."));
    }
    format!("Session [{name}] released.")
}

/// --kill <name>: terminates the session's sqlplus process at once, busy or
/// idle, with no rollback/exit commands, and removes the session. Oracle
/// rolls back uncommitted work when it notices the dropped connection. A
/// client still waiting for this session gets an error instead of hanging.
async fn kill_session(state: &ServerState, name: &str) -> String {
    let Some(slot) = state.find_owned(name) else {
        return format!("No such session [{name}]");
    };

    // Unregistered first, so no new request can reach this slot.
    if state.unown(&slot, name, false).is_none() {
        return format!("No such session [{name}]");
    }

    match slot.session.try_lock() {
        // Idle: dropping the process kills it (kill_on_drop).
        Ok(mut guard) => *guard = None,
        // Busy: the running request holds the lock. notify_one stores the
        // signal, so it is not lost if that request is just about to start
        // waiting for it; the request then drops the process itself.
        Err(_) => slot.kill.notify_one(),
    }
    log(format!("[{name}] Killed."));
    format!("Session [{name}] killed.")
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

/// Tidies the sqlplus output before it goes back to the client.
/// For a successful SELECT, "feedback on" appends an empty line and an
/// "x rows selected." line after the result rows; both are removed.
fn clean_output(output: &str, is_select: bool) -> String {
    let data = output.trim();

    if is_select && !data.contains("no rows selected") && !data.contains("ERROR at line") {
        let mut lines: Vec<&str> = data.split('\n').collect();
        lines.truncate(lines.len().saturating_sub(2));
        // sqlplus on Windows ends lines with \r\n; drop the \r left on the
        // last kept line after splitting on \n.
        return lines.join("\n").trim_end_matches('\r').to_string();
    }
    data.to_string()
}

/// True if the first word on the first non-blank line is SELECT or WITH.
fn is_select_command(sqlcmd: &[String]) -> bool {
    sqlcmd
        .iter()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| l.split_whitespace().next())
        .map(|w| w.eq_ignore_ascii_case("select") || w.eq_ignore_ascii_case("with"))
        .unwrap_or(false)
}

/// Detects Oracle / SQL*Plus error codes in sqlplus output.
fn has_oracle_error(output: &str) -> bool {
    output.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("ORA-") || l.starts_with("SP2-") || l.starts_with("ERROR")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn select_detection() {
        assert!(is_select_command(&v(&["SELECT * from dual;"])));
        assert!(is_select_command(&v(&["  select", "1 from dual;"])));
        assert!(is_select_command(&v(&["with a as (select 1 x from dual)", "select * from a;"])));
        assert!(!is_select_command(&v(&["update t set a=1;"])));
        assert!(!is_select_command(&v(&["selection_thing"])));
        assert!(!is_select_command(&v(&[])));
    }

    #[test]
    fn select_output_cleanup() {
        // Rows, empty line, feedback line -> last two lines removed.
        assert_eq!(clean_output("A,B\n1,2\n3,4\n\n2 rows selected.\n", true), "A,B\n1,2\n3,4");
        assert_eq!(clean_output("A\r\n1\r\n\r\n1 row selected.", true), "A\r\n1");
        // Unchanged (apart from trim): no rows, errors, non-selects.
        assert_eq!(clean_output("\nno rows selected\n", true), "no rows selected");
        assert_eq!(
            clean_output("ERROR at line 1:\nORA-00942: table or view does not exist", true),
            "ERROR at line 1:\nORA-00942: table or view does not exist"
        );
        assert_eq!(clean_output("\n1 row updated.\n", false), "1 row updated.");
        // Fewer than two lines must not panic.
        assert_eq!(clean_output("x", true), "");
    }

    #[test]
    fn error_detection() {
        assert!(has_oracle_error("ERROR:\nORA-01017: invalid username/password"));
        assert!(has_oracle_error("SP2-0640: Not connected"));
        assert!(!has_oracle_error("Connected."));
    }

    #[test]
    fn default_login_commands_run_after_the_users_login_lines() {
        let cfg: Config =
            toml::from_str("sqlplus_login = ['set define on', 'set feedback off']").unwrap();
        let lines = connect_lines("scott/tiger@db1", &cfg);
        assert_eq!(lines[0], "connect scott/tiger@db1");
        assert_eq!(&lines[1..3], ["set define on", "set feedback off"]);
        // The hardcoded defaults come last, including "set define off".
        let defaults = &lines[3..];
        assert_eq!(defaults, DEFAULT_LOGIN_CMDS);
        assert!(defaults.contains(&"set define off".to_string()));
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail_lines("a\nb\nc", 2), vec!["b", "c"]);
        assert_eq!(tail_lines("a\r\nb\r\n", 5), vec!["a", "b", ""]);
        assert_eq!(tail_lines("", 3), vec![""]);
    }

    #[test]
    fn listing_carries_command_and_output() {
        let state = ServerState::new(3);
        let a = state.acquire("a").unwrap();
        {
            let mut m = a.meta();
            m.command = vec!["select 1".to_string(), "from dual;".to_string()];
            m.output = vec!["1".to_string()];
            m.busy_since = Some(Local::now() - chrono::Duration::seconds(65));
        }
        let listing: Listing = serde_json::from_str(&state.listing_json()).unwrap();
        assert_eq!(listing.max_sessions, 3);
        let s = &listing.sessions[0];
        assert_eq!(s.name, "a");
        assert!(!s.free);
        assert_eq!(s.command, vec!["select 1", "from dual;"]);
        assert_eq!(s.output, vec!["1"]);
        assert_eq!(s.state_text(), "busy 1m05s");
    }

    #[test]
    fn session_names() {
        assert_eq!(session_name(""), "default");
        assert_eq!(session_name("  "), "default");
        assert_eq!(session_name(" vim-1 "), "vim-1");
    }

    #[test]
    fn acquire_reuses_own_slot_and_enforces_limit() {
        let state = ServerState::new(2);

        let a = state.acquire("a").unwrap();
        state.acquire("b").unwrap();
        // Same name -> same slot.
        assert!(Arc::ptr_eq(&a, &state.acquire("a").unwrap()));

        // Limit reached.
        let err = state.acquire("c").err().unwrap();
        assert!(err.contains("Session limit reached (2 of 2)"), "{err}");
    }

    #[tokio::test]
    async fn releasing_one_of_several_removes_it() {
        let state = ServerState::new(2);
        state.acquire("a").unwrap();
        state.acquire("b").unwrap();

        assert_eq!(release_session(&state, "nope").await, "No such session [nope]");
        assert_eq!(release_session(&state, "a").await, "Session [a] released.");
        // Removed: releasing again finds nothing, its place is free again.
        assert_eq!(release_session(&state, "a").await, "No such session [a]");
        assert_eq!(state.slots().len(), 1);
        assert!(state.acquire("c").is_ok());
    }

    #[tokio::test]
    async fn releasing_the_last_session_keeps_a_free_slot_for_reuse() {
        let state = ServerState::new(1);
        let a = state.acquire("a").unwrap();

        assert_eq!(release_session(&state, "a").await, "Session [a] released.");
        // Still there, now free (shown as "-" / "free")...
        assert_eq!(state.slots().len(), 1);
        let s = state.status_text();
        assert!(s.contains("free"), "{s}");
        // ...not reachable by its old name...
        assert_eq!(release_session(&state, "a").await, "No such session [a]");
        // ...and taken over by the next new session, even at the limit.
        let b = state.acquire("b").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(b.is_owned_by("b"));
        assert!(state.status_text().contains("idle"));
    }

    #[tokio::test]
    async fn kill_removes_idle_and_busy_sessions() {
        let state = ServerState::new(3);
        assert_eq!(kill_session(&state, "nope").await, "No such session [nope]");

        // Idle - killed even though it is the last one (no free slot kept).
        state.acquire("a").unwrap();
        assert_eq!(kill_session(&state, "a").await, "Session [a] killed.");
        assert!(state.status_text().contains("No sessions"));

        // Busy: the running request holds the session lock.
        let slot = state.acquire("b").unwrap();
        let _held = slot.session.lock().await;
        assert_eq!(kill_session(&state, "b").await, "Session [b] killed.");
        assert!(state.find_owned("b").is_none(), "killed session must be gone");
        // The signal is stored for the running request to pick up.
        let got = tokio::time::timeout(std::time::Duration::from_millis(200), slot.kill.notified()).await;
        assert!(got.is_ok(), "busy session must be told to give up");
    }

    #[test]
    fn status_shows_how_long_a_session_has_been_busy() {
        let state = ServerState::new(2);
        let a = state.acquire("a").unwrap();
        a.meta().busy_since = Some(Local::now() - chrono::Duration::seconds(125));
        state.acquire("b").unwrap();
        let s = state.status_text();
        assert!(s.contains("busy 2m05s"), "{s}");
        assert!(s.contains("idle"), "{s}");
    }

    #[tokio::test]
    async fn release_refuses_a_busy_session() {
        let state = ServerState::new(2);
        let slot = state.acquire("a").unwrap();
        let _held = slot.session.lock().await; // a command is running
        let reply = release_session(&state, "a").await;
        assert!(reply.contains("is busy"), "{reply}");
        assert!(slot.is_owned_by("a"), "a refused release must not change anything");
    }

    #[test]
    fn status_lists_all_sessions() {
        let state = ServerState::new(10);
        assert!(state.status_text().contains("No sessions"));
        let a = state.acquire("a").unwrap();
        a.meta().connected_as = Some("scott@db1".to_string());
        state.acquire("b").unwrap();
        let s = state.status_text();
        assert!(s.starts_with("Server running, sessions: 2 of 10"), "{s}");
        let header = s.lines().nth(1).unwrap();
        assert!(header.starts_with("SESSION"), "{s}");
        for word in ["STATE", "CONNECTION", "LAST USED"] {
            assert!(header.contains(word), "{s}");
        }
        assert!(s.contains("scott@db1"));
        assert!(s.contains("not connected"));
        // Rows are aligned with the header: same column starts.
        let conn_col = header.find("CONNECTION").unwrap();
        assert_eq!(&s.lines().nth(2).unwrap()[conn_col..conn_col + 9], "scott@db1");
    }
}
