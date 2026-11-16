//! Terminal UI (--ui): a live list of the sqlplus sessions with the running
//! or last command and the last output of the selected session, and keys to
//! release or kill a session and to start or stop the server.

use std::borrow::Cow;
use std::io;
use std::time::Duration;

use futures_util::StreamExt;
use ratatui::crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};
use tokio::time::{sleep_until, timeout, Instant};

use crate::client::{send_request, spawn_server};
use crate::constants::{
    ACTION_TIMEOUT, FAST_REFRESH, FAST_REFRESH_WINDOW, HEADINGS, LIST_TIMEOUT, MAX_REFRESH_SECS,
    MIN_REFRESH_SECS, PAGE_LINES, SCROLL_COLUMNS, SERVER_NOT_RUNNING, STARTING,
};
use crate::pipe::connect_client;
use crate::protocol::{column_widths, Listing, Request, SessionInfo};

// ---------------------------------------------------------------------------
// State and key handling (no terminal involved, so it is unit-testable)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pane {
    Command,
    Output,
}

#[derive(PartialEq)]
enum ServerView {
    Down,
    NotResponding,
    Up(Listing),
}

/// An action waiting for the user's y/n.
#[derive(Clone, PartialEq, Debug)]
enum Confirm {
    Release(String),
    Kill(String),
    StopServer,
}

/// What the main loop should do after a key.
#[derive(PartialEq, Debug)]
enum Action {
    Refresh,
    Release(String),
    Kill(String),
    StartServer,
    StopServer,
}

/// True if `key` is the character `c`, in either case.
fn is(key: &KeyEvent, c: char) -> bool {
    matches!(key.code, KeyCode::Char(k) if k.eq_ignore_ascii_case(&c))
}

struct App {
    view: ServerView,
    selected: usize,
    /// Name of the selected session, to keep the selection when the list changes.
    selected_name: Option<String>,
    pane: Pane,
    scroll_y: u16,
    scroll_x: u16,
    confirm: Option<Confirm>,
    message: String,
    refresh_secs: u64,
    quit: bool,
}

impl App {
    fn new(refresh_secs: u64) -> Self {
        Self {
            view: ServerView::Down,
            selected: 0,
            selected_name: None,
            pane: Pane::Command,
            scroll_y: 0,
            scroll_x: 0,
            confirm: None,
            message: String::new(),
            refresh_secs: refresh_secs.clamp(MIN_REFRESH_SECS, MAX_REFRESH_SECS),
            quit: false,
        }
    }

    fn sessions(&self) -> &[SessionInfo] {
        match &self.view {
            ServerView::Up(l) => &l.sessions,
            _ => &[],
        }
    }

    fn current(&self) -> Option<&SessionInfo> {
        self.sessions().get(self.selected)
    }

    fn reset_scroll(&mut self) {
        self.scroll_y = 0;
        self.scroll_x = 0;
    }

    /// Shows a new session list. Returns false (and changes nothing) when
    /// it is the list that is already shown, so the screen need not be redrawn.
    fn set_listing(&mut self, listing: Listing) -> bool {
        if matches!(&self.view, ServerView::Up(old) if *old == listing) {
            return false;
        }
        let old_name = self.selected_name.clone();
        let found = old_name.as_ref().and_then(|n| {
            listing.sessions.iter().position(|s| !s.free && &s.name == n)
        });
        self.selected = found
            .unwrap_or_else(|| self.selected.min(listing.sessions.len().saturating_sub(1)));
        self.selected_name = listing
            .sessions
            .get(self.selected)
            .filter(|s| !s.free)
            .map(|s| s.name.clone());
        if found.is_none() && old_name.is_some() {
            self.reset_scroll(); // the selected session is gone
        }
        self.view = ServerView::Up(listing);
        true
    }

    /// Returns false when the server was already shown as down.
    fn set_down(&mut self) -> bool {
        if self.view == ServerView::Down {
            return false;
        }
        self.view = ServerView::Down;
        self.selected = 0;
        self.selected_name = None;
        self.reset_scroll();
        true
    }

    /// Returns false when the server was already shown as not responding.
    fn set_not_responding(&mut self) -> bool {
        if self.view == ServerView::NotResponding {
            return false;
        }
        self.view = ServerView::NotResponding;
        true
    }

    fn select(&mut self, index: usize) {
        self.selected = index;
        self.selected_name = self
            .sessions()
            .get(index)
            .filter(|s| !s.free)
            .map(|s| s.name.clone());
        self.reset_scroll();
    }

    /// Lines of the bottom pane (the selected session's command or output),
    /// borrowed from the session list: nothing is copied.
    fn detail_lines(&self) -> Vec<&str> {
        let Some(info) = self.current() else {
            return vec!["(no session)"];
        };
        match self.pane {
            Pane::Command if info.command.is_empty() => vec!["(no command yet)"],
            Pane::Command => as_strs(&info.command),
            Pane::Output if info.output.iter().any(|l| !l.is_empty()) => as_strs(&info.output),
            Pane::Output if info.busy_secs.is_some() => {
                vec!["(running - the output appears when the command finishes)"]
            }
            Pane::Output => vec!["(no output yet)"],
        }
    }

    fn scroll_limits(&self) -> (u16, u16) {
        let lines = self.detail_lines();
        let max_y = lines.len().saturating_sub(1);
        let max_x = lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
            .saturating_sub(1);
        (max_y.min(u16::MAX as usize) as u16, max_x.min(u16::MAX as usize) as u16)
    }

    /// Changes the refresh interval by `delta` seconds (kept within limits).
    fn change_interval(&mut self, delta: i64) {
        self.refresh_secs = self
            .refresh_secs
            .saturating_add_signed(delta)
            .clamp(MIN_REFRESH_SECS, MAX_REFRESH_SECS);
        self.message = format!("Refreshing every {} s.", self.refresh_secs);
    }

    /// The question shown while an action waits for y/n.
    fn prompt(&self) -> Option<String> {
        self.confirm.as_ref().map(|c| match c {
            Confirm::Release(n) => format!("Release session '{n}'? (y/n)"),
            Confirm::Kill(n) => {
                format!("Kill session '{n}'? Its sqlplus process is terminated at once. (y/n)")
            }
            Confirm::StopServer => {
                "Stop the server? Idle sessions are closed, busy ones are killed. (y/n)".to_string()
            }
        })
    }

    fn ask_session_action(&mut self, kill: bool) {
        let target = self.current().map(|s| (s.free, s.name.clone()));
        match target {
            Some((false, name)) => {
                self.message.clear();
                self.confirm = Some(if kill { Confirm::Kill(name) } else { Confirm::Release(name) });
            }
            Some((true, _)) => {
                self.message = "The free session has no owner; use s to stop the server.".to_string();
            }
            None => self.message = "No session selected.".to_string(),
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return None;
        }

        // While a question is open only y confirms; any other key cancels.
        if let Some(confirm) = self.confirm.take() {
            return if is(&key, 'y') {
                self.message.clear();
                Some(match confirm {
                    Confirm::Release(n) => Action::Release(n),
                    Confirm::Kill(n) => Action::Kill(n),
                    Confirm::StopServer => Action::StopServer,
                })
            } else {
                self.message = "Cancelled.".to_string();
                None
            };
        }

        match key.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.select(self.selected - 1);
                }
            }
            KeyCode::Down => {
                if self.selected + 1 < self.sessions().len() {
                    self.select(self.selected + 1);
                }
            }
            KeyCode::PageUp => self.scroll_y = self.scroll_y.saturating_sub(PAGE_LINES),
            KeyCode::PageDown => {
                let (max_y, _) = self.scroll_limits();
                self.scroll_y = (self.scroll_y + PAGE_LINES).min(max_y);
            }
            KeyCode::Left => self.scroll_x = self.scroll_x.saturating_sub(SCROLL_COLUMNS),
            KeyCode::Right => {
                let (_, max_x) = self.scroll_limits();
                self.scroll_x = (self.scroll_x + SCROLL_COLUMNS).min(max_x);
            }
            KeyCode::Home => self.reset_scroll(),
            KeyCode::Tab => {
                self.pane = if self.pane == Pane::Command { Pane::Output } else { Pane::Command };
                self.reset_scroll();
            }
            KeyCode::F(5) => return Some(Action::Refresh),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change_interval(1),
            KeyCode::Char('-') | KeyCode::Char('_') => self.change_interval(-1),
            _ if is(&key, 'q') => self.quit = true,
            _ if is(&key, 'r') => self.ask_session_action(false),
            _ if is(&key, 'k') => self.ask_session_action(true),
            _ if is(&key, 's') => match self.view {
                ServerView::Down => return Some(Action::StartServer),
                _ => {
                    self.message.clear();
                    self.confirm = Some(Confirm::StopServer);
                }
            },
            _ => {}
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let wanted = (app.sessions().len() as u16).saturating_add(3).max(5);
    let top_height = wanted.min((area.height * 40 / 100).max(5));
    let [top, middle, footer] = Layout::vertical([
        Constraint::Length(top_height),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(area);

    render_sessions(frame, top, app);
    render_detail(frame, middle, app);
    render_footer(frame, footer, app);
}

fn render_sessions(frame: &mut Frame, area: Rect, app: &App) {
    let (title, empty_text) = match &app.view {
        ServerView::Up(l) => (
            format!(
                " sqlPlusExec - server running - sessions {} of {} - refresh {} s ",
                l.sessions.len(),
                l.max_sessions,
                app.refresh_secs
            ),
            "No sessions".to_string(),
        ),
        ServerView::Down => (
            format!(" sqlPlusExec - server not running - refresh {} s ", app.refresh_secs),
            "Server not running. Press s to start it.".to_string(),
        ),
        ServerView::NotResponding => (
            format!(" sqlPlusExec - server not responding - refresh {} s ", app.refresh_secs),
            "The server does not answer. Press s to stop it.".to_string(),
        ),
    };
    let block = Block::bordered().title(title);

    let sessions = app.sessions();
    if sessions.is_empty() {
        frame.render_widget(Paragraph::new(empty_text).block(block), area);
        return;
    }

    let cells: Vec<[String; 4]> = sessions.iter().map(SessionInfo::row).collect();
    let widths = column_widths(&cells).map(|w| Constraint::Length(w as u16));

    let table = Table::new(cells.into_iter().map(Row::new), widths)
        .header(Row::new(HEADINGS).style(Style::new().add_modifier(Modifier::BOLD)))
        .block(block)
        .column_spacing(2)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn as_strs(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

/// Tabs become four blanks; a line without a tab is not copied.
fn expand_tabs(line: &str) -> Cow<'_, str> {
    if line.contains('\t') {
        Cow::Owned(line.replace('\t', "    "))
    } else {
        Cow::Borrowed(line)
    }
}

fn render_detail(frame: &mut Frame, area: Rect, app: &App) {
    let tab = |label: &str, active: bool| {
        let text = format!(" {label} ");
        if active {
            Span::styled(text, Style::new().add_modifier(Modifier::REVERSED))
        } else {
            Span::raw(text)
        }
    };
    let mut title = vec![
        tab("Command", app.pane == Pane::Command),
        Span::raw("|"),
        tab("Output", app.pane == Pane::Output),
    ];
    if let Some(info) = app.current() {
        let who = if info.free { "free session".to_string() } else { info.name.clone() };
        title.push(Span::raw(format!(" {who} - {} ", info.state_text())));
    }

    let lines: Vec<Line> = app
        .detail_lines()
        .into_iter()
        .map(|l| Line::raw(expand_tabs(l)))
        .collect();
    let paragraph = Paragraph::new(lines)
        .block(Block::bordered().title(Line::from(title)))
        .scroll((app.scroll_y, app.scroll_x));
    frame.render_widget(paragraph, area);
}

/// "key:action | key:action ...": keys bold, delimiters dim. The s action
/// depends on whether the server is running.
fn help_items(app: &App) -> Vec<(&'static str, &'static str)> {
    let server = if matches!(app.view, ServerView::Down) { "start" } else { "stop" };
    vec![
        ("Up/Dn", "select"),
        ("r", "release"),
        ("k", "kill"),
        ("s", server),
        ("Tab", "cmd/output"),
        ("PgUp/PgDn/</>", "scroll"),
        ("+/-", "interval"),
        ("F5", "refresh"),
        ("q", "quit"),
    ]
}

fn help_line(app: &App) -> Line<'static> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut spans = Vec::new();
    for (i, (key, action)) in help_items(app).into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" \u{2502} ", dim));
        }
        spans.push(Span::styled(key, Style::new().add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(":", dim));
        spans.push(Span::raw(action));
    }
    Line::from(spans)
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let first = match app.prompt() {
        Some(p) => Line::styled(p, Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)),
        None => Line::raw(app.message.clone()),
    };
    let help = help_line(app);
    frame.render_widget(Paragraph::new(vec![first, help]), area);
}

// ---------------------------------------------------------------------------
// Talking to the server, and the main loop
// ---------------------------------------------------------------------------

enum CallError {
    /// No server on the pipe.
    Down,
    /// Connected, but no (valid) answer.
    Failed(String),
}

/// One request on a fresh connection.
async fn call(request: &Request, limit: Duration) -> Result<String, CallError> {
    let stream = match timeout(limit, connect_client()).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) => return Err(CallError::Down),
        Err(_) => return Err(CallError::Failed("timed out".to_string())),
    };
    match timeout(limit, send_request(stream, request)).await {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(CallError::Failed(e.to_string())),
        Err(_) => Err(CallError::Failed("timed out".to_string())),
    }
}

/// The text to show in the footer for the outcome of an action.
async fn call_message(request: Request) -> String {
    match call(&request, ACTION_TIMEOUT).await {
        Ok(text) => text,
        Err(CallError::Down) => SERVER_NOT_RUNNING.to_string(),
        Err(CallError::Failed(e)) => format!("Error: {e}"),
    }
}

/// Asks the server for its sessions and shows the answer. Returns whether
/// anything on the screen changed.
async fn refresh(app: &mut App) -> bool {
    match call(&Request::List, LIST_TIMEOUT).await {
        Ok(json) => match serde_json::from_str::<Listing>(&json) {
            Ok(listing) => {
                let was_starting = matches!(app.view, ServerView::Down) && app.message == STARTING;
                if was_starting {
                    app.message.clear();
                }
                app.set_listing(listing) || was_starting
            }
            Err(e) => {
                let message = format!("Unexpected answer from the server: {e}");
                let changed = app.set_not_responding() || app.message != message;
                app.message = message;
                changed
            }
        },
        Err(CallError::Down) => app.set_down(),
        Err(CallError::Failed(_)) => app.set_not_responding(),
    }
}

/// Runs the UI until the user quits.
pub async fn ui(configfile: &Option<String>, refresh_secs: u64) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, configfile, refresh_secs).await;
    ratatui::restore();
    result
}

/// Carries out what a key asked for; the outcome goes to the footer.
async fn run_action(
    action: Action,
    app: &mut App,
    configfile: &Option<String>,
    fast_until: &mut Option<Instant>,
) {
    match action {
        Action::Refresh => {}
        Action::Release(name) => app.message = call_message(Request::Release(name)).await,
        Action::Kill(name) => app.message = call_message(Request::Kill(name)).await,
        Action::StopServer => {
            app.message = call_message(Request::Stop).await;
            *fast_until = Some(Instant::now() + FAST_REFRESH_WINDOW);
        }
        Action::StartServer => {
            app.message = match spawn_server(configfile) {
                Ok(()) => STARTING.to_string(),
                Err(e) => format!("Cannot start the server: {e}"),
            };
            *fast_until = Some(Instant::now() + FAST_REFRESH_WINDOW);
        }
    }
}

/// The main loop: sleeps until a key arrives or the next refresh is due, and
/// draws only after something changed.
async fn run(
    terminal: &mut DefaultTerminal,
    configfile: &Option<String>,
    refresh_secs: u64,
) -> io::Result<()> {
    let mut app = App::new(refresh_secs);
    let mut events = EventStream::new();
    let mut fast_until: Option<Instant> = None;
    refresh(&mut app).await;
    let mut next_refresh = Instant::now() + Duration::from_secs(app.refresh_secs);
    let mut dirty = true;

    while !app.quit {
        if dirty {
            terminal.draw(|frame| render(frame, &app))?;
            dirty = false;
        }

        tokio::select! {
            event = events.next() => match event {
                // Windows reports key releases too; only act on presses.
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                    dirty = true;
                    if let Some(action) = app.on_key(key) {
                        run_action(action, &mut app, configfile, &mut fast_until).await;
                        next_refresh = Instant::now(); // show the result now
                    }
                }
                Some(Ok(Event::Resize(..))) => dirty = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => break, // the input is closed
            },
            _ = sleep_until(next_refresh) => {
                dirty |= refresh(&mut app).await;
                let interval = if fast_until.is_some_and(|until| Instant::now() < until) {
                    FAST_REFRESH
                } else {
                    Duration::from_secs(app.refresh_secs)
                };
                next_refresh = Instant::now() + interval;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn session(name: &str, busy: Option<i64>, conn: &str) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            free: false,
            busy_secs: busy,
            connection: Some(conn.to_string()),
            last_used: "07-11-2026 16:40:31".to_string(),
            command: vec!["select *".to_string(), "from big_table;".to_string()],
            output: vec!["ID,NAME".to_string(), "1,a".to_string()],
        }
    }

    fn app_with(sessions: Vec<SessionInfo>) -> App {
        let mut app = App::new(5);
        app.set_listing(Listing {
            max_sessions: 10,
            sessions,
        });
        app
    }

    fn two_sessions() -> App {
        app_with(vec![
            session("v1", None, "scott@db1"),
            session("v2", Some(125), "hr@db2"),
        ])
    }

    fn screen_text(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_unchanged_list_does_not_ask_for_a_redraw() {
        let mut app = two_sessions();
        let same = || Listing {
            max_sessions: 10,
            sessions: vec![
                session("v1", None, "scott@db1"),
                session("v2", Some(125), "hr@db2"),
            ],
        };
        assert!(!app.set_listing(same()));

        // A busy counter that moved on is a change.
        let mut moved = same();
        moved.sessions[1].busy_secs = Some(130);
        assert!(app.set_listing(moved));

        assert!(app.set_down());
        assert!(!app.set_down());
        assert!(app.set_not_responding());
        assert!(!app.set_not_responding());
        assert!(app.set_listing(same()));
    }

    #[test]
    fn detail_lines_are_borrowed_not_copied() {
        let app = two_sessions();
        let lines = app.detail_lines();
        assert_eq!(lines, ["select *", "from big_table;"]);
        // Same memory as the session's own strings.
        assert!(std::ptr::eq(lines[0], app.sessions()[0].command[0].as_str()));
        assert!(matches!(expand_tabs("a b"), Cow::Borrowed(_)));
        assert_eq!(expand_tabs("a\tb"), "a    b");
    }

    #[test]
    fn arrows_move_the_selection() {
        let mut app = two_sessions();
        assert_eq!(app.selected, 0);
        app.on_key(code(KeyCode::Down));
        assert_eq!(app.current().unwrap().name, "v2");
        app.on_key(code(KeyCode::Down)); // stays on the last row
        assert_eq!(app.selected, 1);
        app.on_key(code(KeyCode::Up));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn release_and_kill_ask_first_and_only_y_confirms() {
        let mut app = two_sessions();
        app.on_key(code(KeyCode::Down));

        assert_eq!(app.on_key(key('r')), None);
        assert!(app.prompt().unwrap().contains("Release session 'v2'"));
        assert_eq!(app.on_key(key('y')), Some(Action::Release("v2".to_string())));
        assert!(app.prompt().is_none());

        assert_eq!(app.on_key(key('k')), None);
        assert!(app.prompt().unwrap().contains("Kill session 'v2'"));
        assert_eq!(app.on_key(key('n')), None); // anything but y cancels
        assert!(app.prompt().is_none());
        assert_eq!(app.message, "Cancelled.");

        assert_eq!(app.on_key(key('k')), None);
        assert_eq!(app.on_key(key('y')), Some(Action::Kill("v2".to_string())));
    }

    #[test]
    fn s_starts_a_down_server_and_asks_before_stopping() {
        let mut app = App::new(5);
        assert_eq!(app.on_key(key('s')), Some(Action::StartServer)); // no question

        let mut app = two_sessions();
        assert_eq!(app.on_key(key('s')), None);
        assert!(app.prompt().unwrap().contains("Stop the server"));
        assert_eq!(app.on_key(key('y')), Some(Action::StopServer));
    }

    #[test]
    fn the_free_session_cannot_be_released_or_killed() {
        let mut free = session("", None, "x");
        free.free = true;
        free.connection = None;
        let mut app = app_with(vec![free]);
        assert_eq!(app.on_key(key('r')), None);
        assert!(app.prompt().is_none());
        assert!(app.message.contains("free session"));
    }

    #[test]
    fn selection_follows_the_session_when_the_list_changes() {
        let mut app = two_sessions();
        app.on_key(code(KeyCode::Down)); // v2
        app.set_listing(Listing {
            max_sessions: 10,
            sessions: vec![
                session("v0", None, "a@b"),
                session("v1", None, "scott@db1"),
                session("v2", None, "hr@db2"),
            ],
        });
        assert_eq!(app.current().unwrap().name, "v2");

        // v2 disappears: selection stays inside the list.
        app.set_listing(Listing {
            max_sessions: 10,
            sessions: vec![session("v0", None, "a@b")],
        });
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn tab_interval_and_quit_keys() {
        let mut app = two_sessions();
        assert_eq!(app.pane, Pane::Command);
        app.on_key(code(KeyCode::Tab));
        assert_eq!(app.pane, Pane::Output);

        app.on_key(key('+'));
        assert_eq!(app.refresh_secs, 6);
        for _ in 0..10 {
            app.on_key(key('-'));
        }
        assert_eq!(app.refresh_secs, 1);
        assert_eq!(app.on_key(code(KeyCode::F(5))), Some(Action::Refresh));

        assert!(!app.quit);
        app.on_key(key('q'));
        assert!(app.quit);
    }

    #[test]
    fn scrolling_stays_inside_the_text() {
        let mut s = session("v1", None, "a@b");
        s.output = (1..=30).map(|i| format!("line {i}")).collect();
        let mut app = app_with(vec![s]);
        app.on_key(code(KeyCode::Tab)); // output
        for _ in 0..10 {
            app.on_key(code(KeyCode::PageDown));
        }
        assert_eq!(app.scroll_y, 29);
        app.on_key(code(KeyCode::PageUp));
        assert_eq!(app.scroll_y, 19);
        app.on_key(code(KeyCode::Home));
        assert_eq!((app.scroll_y, app.scroll_x), (0, 0));
    }

    #[test]
    fn screen_shows_sessions_command_and_output() {
        let mut app = two_sessions();
        app.on_key(code(KeyCode::Down)); // the busy session
        let text = screen_text(&app, 110, 24);
        for expected in [
            "server running - sessions 2 of 10",
            "SESSION",
            "scott@db1",
            "busy 2m05s",
            "v2 - busy 2m05s",
            "select *",
            "from big_table;",
            "Release session", // not yet: no prompt
        ] {
            if expected == "Release session" {
                assert!(!text.contains(expected));
            } else {
                assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
            }
        }

        app.on_key(code(KeyCode::Tab));
        let text = screen_text(&app, 110, 24);
        assert!(text.contains("ID,NAME"), "{text}");
        assert!(text.contains("1,a"), "{text}");
    }

    #[test]
    fn screen_shows_the_confirmation_and_the_down_server() {
        let mut app = two_sessions();
        app.on_key(key('k'));
        let text = screen_text(&app, 110, 24);
        assert!(text.contains("Kill session 'v1'?"), "{text}");

        let mut down = App::new(5);
        down.set_down();
        let text = screen_text(&down, 110, 24);
        assert!(text.contains("server not running"), "{text}");
        assert!(text.contains("Press s to start it."), "{text}");
    }

    #[test]
    fn help_line_follows_the_server_state() {
        let text = |app: &App| {
            help_line(app).spans.iter().map(|s| s.content.to_string()).collect::<String>()
        };
        let mut down = App::new(5);
        down.set_down();
        assert!(text(&down).contains("s:start"));
        assert!(!text(&down).contains("s:stop"));
        let up = two_sessions();
        assert!(text(&up).contains("s:stop"));
        assert!(text(&up).contains("r:release \u{2502} k:kill"));
        assert!(screen_text(&up, 120, 24).contains("q:quit"));
    }

    #[test]
    fn screen_fits_a_small_terminal_and_empty_list() {
        let app = app_with(vec![]);
        let text = screen_text(&app, 60, 10);
        assert!(text.contains("No sessions"), "{text}");
        let busy = two_sessions();
        screen_text(&busy, 40, 8); // must not panic
    }
}
