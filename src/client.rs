use std::{io, process::Stdio, time::Duration};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::time::sleep;

use crate::constants::{
    MAX_SERVER_START_ATTEMPTS, SERVER_NOT_RUNNING, SERVER_POLL_INTERVAL, SERVER_START_WAIT,
};
use crate::pipe::{connect_client, PipeClient};
use crate::protocol::{read_message, write_message, Request};

/// Runs sqlcmd lines through the server (starting it if needed) and prints
/// the response.
pub async fn client(
    conn: &str,
    sqlcmd: &[String],
    session: &Option<String>,
    configfile: &Option<String>,
) -> io::Result<()> {
    let stream = connect_or_start_server(configfile).await?;
    let response = send_request(stream, &Request::run(conn, sqlcmd, session)).await?;
    println!("{response}");
    Ok(())
}

/// Sends a control request (stop, status, release, kill) to the running
/// server and prints the reply. Never starts a server.
pub async fn send_to_running_server(request: Request) -> io::Result<()> {
    match connect_client().await {
        Ok(stream) => println!("{}", send_request(stream, &request).await?),
        Err(_) => println!("{SERVER_NOT_RUNNING}"),
    }
    Ok(())
}

/// Sends one request and waits for the response.
pub async fn send_request<S>(stream: S, request: &Request) -> io::Result<String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);

    write_message(&mut writer, request).await?;
    read_message::<_, String>(&mut reader).await?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "server closed the connection without a response",
        )
    })
}

/// Connects to the pipe. If that fails the server is assumed not to be
/// running: start it in a separate process and retry.
async fn connect_or_start_server(configfile: &Option<String>) -> io::Result<PipeClient> {
    if let Ok(stream) = connect_client().await {
        return Ok(stream);
    }

    for _ in 1..=MAX_SERVER_START_ATTEMPTS {
        // No progress message here: the caller only wants sqlplus output.
        // If an earlier attempt is merely slow, a second server fails to
        // create the pipe (first_pipe_instance) and exits harmlessly.
        spawn_server(configfile)?;

        let mut waited = Duration::ZERO;
        while waited < SERVER_START_WAIT {
            sleep(SERVER_POLL_INTERVAL).await;
            waited += SERVER_POLL_INTERVAL;
            if let Ok(stream) = connect_client().await {
                return Ok(stream);
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("Server not found (could not start it after {MAX_SERVER_START_ATTEMPTS} attempts)"),
    ))
}

/// Starts "<this exe> --start" as a separate, detached process.
pub fn spawn_server(configfile: &Option<String>) -> io::Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("--start");
    if let Some(path) = configfile {
        cmd.arg("--configfile").arg(path);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200; // survives Ctrl+C of the client
        const CREATE_NO_WINDOW: u32 = 0x0800_0000; // hidden console, inherited by sqlplus
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    // The child handle is dropped without waiting; the process keeps running.
    cmd.spawn()?;
    Ok(())
}
