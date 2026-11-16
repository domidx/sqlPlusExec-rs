# sqlPlusExec

A small, fast **sqlplus executor** written in Rust. It keeps Oracle SQL\*Plus
sessions open in a background server process, so every query you send runs
instantly on an already-connected session instead of paying the cost of
starting `sqlplus` and logging in each time.

This is the **Rust version** of the Python program
[sqlPlusExec-py](https://github.com/domidx/sqlPlusExec-py), with the same
client/server idea and command line, plus a few additions (multiple named
sessions, a terminal UI).

It is designed to be used as the **backend of the Vim plugin
[oracleExec.vim](https://github.com/domidx/oracleExec.vim)**, but it is a
normal command line tool and can be used from any editor or script.

> **Platform:** Windows only. Client and server talk over Windows named pipes.
> On other platforms the program builds (so the tests can run) but exits with
> an error at start-up.

---

## Contents

- [How it works](#how-it-works)
- [Requirements](#requirements)
- [Build](#build)
- [Quick start](#quick-start)
- [Command line parameters](#command-line-parameters)
- [Sessions](#sessions)
- [Config file](#config-file)
- [Terminal UI](#terminal-ui)
- [Output format](#output-format)
- [Logging and security notes](#logging-and-security-notes)
- [Using it from Vim](#using-it-from-vim)
- [Project layout](#project-layout)
- [Tests](#tests)

---

## How it works

The same executable plays two roles:

```
 editor / script                 sqlPlusExec --start (background)
┌───────────────┐   named pipe   ┌──────────────────────────────┐
│ sqlPlusExec   │ ─────────────► │ server                       │
│  --conn ...   │  one JSON line │  ├─ session "default" ─► sqlplus -S -L /nolog
│  --sqlcmd ... │ ◄───────────── │  ├─ session "v1"      ─► sqlplus ...
└───────────────┘  one JSON line │  └─ session "v2"      ─► sqlplus ...
                                 └──────────────────────────────┘
```

1. **Client run** (`--conn` + `--sqlcmd`). The client connects to the server's
   named pipe. If no server is running, it starts one (`sqlPlusExec --start`,
   as a detached, hidden process that survives the client), waits for the pipe
   to come up, sends the request and prints the reply.
2. **Server** (`--start`). Listens on the pipe
   `\\.\pipe\<exe name>-rs` and owns a pool of long-lived `sqlplus`
   processes, one per named session. Requests for the same session are queued
   and run in order; different sessions run in parallel.
3. **Protocol.** Each message is a single line of JSON terminated by `\n`.
   One request in, one reply (a string) out.
4. **Running SQL.** Your lines are written to the session's `sqlplus` stdin,
   wrapped between two unique marker lines
   (`__SQLPLUS_EXEC_START_<n>__` / `__SQLPLUS_EXEC_END_<n>__`). Only the output
   between the markers is returned, so login-script noise and stray output
   never leak into results. There is no timeout: a command runs as long as it
   needs.
5. **Connections are reused.** The server connects a session on first use and
   reconnects only when the connection string changes (compared
   case-insensitively). Before switching connection, uncommitted work on the
   old one is rolled back. If a `sqlplus` process dies, the next request starts
   a fresh one and reconnects.

`sqlplus` is started as `sqlplus -S -L /nolog` (silent, no re-prompt on login
failure, no initial connect), followed by a `connect ...` command.

## Requirements

- Windows
- Oracle `sqlplus` (Instant Client or full client), on `PATH` or configured
  with `sqlplus_cmd`
- Rust toolchain to build it (the manifest declares `edition = "2024"` and
  `rust-version = "1.98"`)

## Build

```
cargo build --release
```

The executable is `target\release\sqlPlusExec.exe`. Put it wherever you like;
the config file is looked up next to it by default (see
[Config file](#config-file)).

> The pipe name and the log file name are derived from the **exe name**, so if
> you rename the exe, the pipe name changes with it. Client and server always
> agree because the client starts the server from the very same exe.

## Quick start

```bat
:: Run a query. The server is started automatically if needed.
sqlPlusExec --conn scott/tiger@db1 --sqlcmd "select * from dept;"

:: Several values = several lines sent to sqlplus
sqlPlusExec --conn scott/tiger@db1 --sqlcmd "select ename," "sal" "from emp;"

:: What is running?
sqlPlusExec --status

:: Work in a separate session (its own sqlplus process and transaction)
sqlPlusExec --session v1 --conn scott/tiger@db1 --sqlcmd "update emp set sal = sal + 1;"
sqlPlusExec --release v1        :: roll back and free it

:: Live terminal UI
sqlPlusExec --ui

:: Shut the server down
sqlPlusExec --stop
```

## Command line parameters

A run is either a **client run** (`--conn` and `--sqlcmd`) or exactly **one
control command**. Control commands are mutually exclusive; `--conn` and
`--sqlcmd` are not needed with them.

### Client run

| Parameter | Description |
|---|---|
| `--conn <STRING>` | Oracle connection string, `username/password@tnsname`. Required. EZConnect strings (`host:1521/service`) and `as sysdba` suffixes work too. |
| `--sqlcmd <LINE>...` | One or more values, **each one line** sent to the sqlplus session. Required. |
| `--session <NAME>` | Session to run in. Default: `default`. Each session is a separate `sqlplus` process. |
| `--configfile <PATH>` | Config file to use instead of the default location. |

Starts the server if it is not running.

### Control commands

| Parameter | Description |
|---|---|
| `--start` | Run as the server (in the foreground of this process). Normally you do not need this; a client run starts it for you. |
| `--stop` | Stop the server. Idle sessions get `rollback` and `exit`; busy ones are killed. |
| `--status` | Show whether the server is running and list its sessions. |
| `--release <SESSION>` | Roll back the session's uncommitted work and mark it free for re-use. A busy session cannot be released. |
| `--kill <SESSION>` | Terminate the session's `sqlplus` process immediately, even if it is busy. Oracle rolls back uncommitted work when it notices the dropped connection. A client still waiting on that session gets an error. |
| `--ui [SECONDS]` | Open the terminal UI. Optional refresh interval in seconds (≥ 1, default 5). |

`--stop`, `--status`, `--release`, `--kill` never start a server; if none is
running they print `Server is not running.`

`--help` prints the usage plus a built-in config-file reference.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success. Note that a server-side error (e.g. `Error: Session limit reached ...`) is returned as the reply text and printed; the exit code is still `0`. |
| `1` | Client-side error (pipe/IO error, server could not be started, ...). |
| `2` | Bad command line, bad config file, or not running on Windows. |

## Sessions

- A session is identified by a name. No `--session` means the session
  `default`.
- Each session has its own `sqlplus` process, its own connection and its own
  transaction. Sessions are created on first use, up to `max_sessions`.
- A new session name beyond the limit is refused with
  `Session limit reached (N of N), use --status and --release <name>`.
- `--release` rolls back and frees a session. Normally its `sqlplus` process
  exits and the slot disappears. The **last remaining** session is kept as a
  free, disconnected `sqlplus` process that the next new session takes over,
  so the next editor instance doesn't have to start `sqlplus` again.
- `--status` output:

```
Server running, sessions: 2 of 10
SESSION  STATE       CONNECTION   LAST USED
default  idle        scott@db1    16-11-2026 10:15:02
v1       busy 12m05s scott@db1    16-11-2026 10:03:00
```

`STATE` is `idle`, `busy <duration>` or `free`. Passwords never appear here;
only `user@tns` is shown.

## Config file

A [TOML](https://toml.io) file read by the server (`--start`) and validated on
every other run, so a mistake is reported before a server is spawned.

**Location:** `--configfile <path>`, otherwise `sqlPlusExec.config` in the same
folder as the exe. The file must exist. **Every option is optional**; a missing
option uses its default (an empty file is valid).

| Option | Type | Default | Description |
|---|---|---|---|
| `sqlplus_cmd` | string | `""` | Command that starts sqlplus, e.g. `"C:/oracle/bin/sqlplus.exe"`. Empty = `sqlplus` from `PATH`. |
| `sqlplus_login` | array of strings | `[]` | SQL\*Plus lines run after every new database connect, e.g. `["alter session set nls_date_format='YYYY-MM-DD'"]`. |
| `sqlplus_encoding` | string | `""` | Character set of sqlplus input/output as an encoding label (`"utf-8"`, `"windows-1252"`, ...). Should match the charset part of `NLS_LANG`. Empty = UTF-8. An unknown label is a config error. |
| `max_sessions` | integer | `10` | Most `sqlplus` sessions (processes) kept open at once. Must be 1 or more. |
| `enable_log_file` | bool | `false` | `true` = also write the server log to `<exe name>.log` next to the exe (recreated on every server start). `0` / `1` are accepted for `false` / `true` (older config files). |

**Example `sqlPlusExec.config`:**

```toml
sqlplus_cmd      = "C:/oracle/bin/sqlplus.exe"
sqlplus_login    = ["set pagesize 0"]
sqlplus_encoding = "windows-1252"
max_sessions     = 5
enable_log_file  = true
```

### Built-in settings

After every connect, the server runs `sqlplus_login` and **then** these
built-in lines, so they always win if the two conflict:

```
set linesize 512
col error for a220
set timing off
set serveroutput on
set feedback on
```

`&variable` substitution is **not** disabled by default. A statement
containing `&name` makes sqlplus prompt for a value on stdin, which swallows
the end marker, so the command never finishes. If your SQL contains `&`
(in string literals, say), add `"set define off"` to `sqlplus_login`.

## Terminal UI

`sqlPlusExec --ui [SECONDS]` opens a live view of the server: a table of
sessions, plus the running (or last) command and the last output of the
selected session (up to 200 lines kept per session).

| Key | Action |
|---|---|
| `Up` / `Down` | Select a session |
| `r` | Release the selected session (asks `y` to confirm) |
| `k` | Kill the selected session (asks `y` to confirm) |
| `s` | Start the server if it is down, otherwise stop it (asks `y` to confirm) |
| `Tab` | Switch between the **command** and **output** panes |
| `PgUp` / `PgDn` | Scroll vertically |
| `<` / `>` (`Left` / `Right`) | Scroll horizontally |
| `Home` | Reset scroll |
| `+` / `-` | Change refresh interval (1–60 s) |
| `F5` | Refresh now |
| `q` / `Ctrl+C` | Quit |

While a confirmation is open, only `y` confirms; any other key cancels. The UI
refreshes faster for a few seconds after starting or stopping the server so
the screen follows the change.

## Output format

- **`SELECT` / `WITH` queries** (first word of the first non-blank line) are
  returned as **CSV** (`set markup csv on`), with the trailing blank line and
  `N rows selected.` line removed.
- **Anything else** (DML, DDL, PL/SQL, SQL\*Plus commands) is returned as plain
  sqlplus output, e.g. `1 row updated.`.
- Errors from Oracle (`ORA-...`, `SP2-...`) are part of the output text.
- If a connect or login fails, the reply starts with
  `Error: connect/login failed:` followed by sqlplus' message.
- Server-side problems come back as `Error: ...` (session limit reached,
  `sqlplus` failed to start, session killed, `sqlplus` exited, ...).

## Logging and security notes

- The server logs to the console (stdout/stderr), and optionally to
  `<exe name>.log` (see `enable_log_file`). Lines look like
  `16-11-2026 10:15:02: [default] Request from scott@db1:`.
- **Passwords are never logged or listed**: connection strings are reduced to
  `user@tns` (quoted passwords and EZConnect strings are handled).
- **SQL text is logged and shown in the UI as-is**, not masked. Avoid putting
  secrets in statements if you enable the log file.
- The connection string, including the password, is passed to the server over
  the local named pipe and to `sqlplus` through its stdin (not on its command
  line).
- Uncommitted work is **rolled back**, never committed, on release, on
  connection switch and on server stop (`set exitcommit off` is used).

## Using it from Vim

[oracleExec.vim](https://github.com/domidx/oracleExec.vim) calls this program
as its backend: it passes the connection with `--conn`, the lines of the
current statement/selection with `--sqlcmd`, and uses `--session` to give each
Vim instance its own transaction. Install the plugin as described in its
README and point it at `sqlPlusExec.exe`; the server starts automatically on
first use and stays warm between queries.

## Project layout

```
src/
├── main.rs       entry point, dispatches client / control / server / UI
├── args.rs       command line definition (clap)
├── config.rs     config file parsing, validation and built-in help text
├── constants.rs  every constant in one place
├── client.rs     client side: connect, auto-start server, send request
├── server.rs     server: session registry, request processing, release/kill
├── sqlplus.rs    one long-lived sqlplus subprocess driven via stdin/stdout
├── protocol.rs   request/reply types and JSON-line framing
├── pipe.rs       Windows named pipe (stand-in on other platforms)
├── ui.rs         terminal UI (ratatui + crossterm)
└── log.rs        timestamped logging, password-safe connection strings
```

Main dependencies: `tokio`, `clap`, `serde` / `serde_json`, `toml`,
`encoding_rs`, `chrono`, `ratatui`, `crossterm`.

## Tests

```
cargo test
```

Unit tests cover argument parsing, config validation, the wire protocol,
output cleanup, SELECT detection, password masking and the UI key handling.
They also run on non-Windows platforms.
