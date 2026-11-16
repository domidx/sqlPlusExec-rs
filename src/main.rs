mod args;
mod client;
mod config;
mod constants;
mod log;
mod pipe;
mod protocol;
mod server;
mod sqlplus;
mod ui;

use std::process::ExitCode;

use constants::DEFAULT_REFRESH_SECS;
use protocol::Request;

#[tokio::main]
async fn main() -> ExitCode {
    // The program talks over Windows named pipes (see pipe.rs); on other
    // platforms it only builds, so that the tests can run there.
    if !cfg!(windows) {
        eprintln!("This app uses Windows named pipes and works on Windows only.");
        return ExitCode::from(2);
    }

    let args = args::Args::read();

    let result = if args.stop {
        // Stopping needs no config.
        client::send_to_running_server(Request::Stop).await
    } else if args.status {
        client::send_to_running_server(Request::Status).await
    } else if let Some(name) = &args.release {
        client::send_to_running_server(Request::Release(name.clone())).await
    } else if let Some(name) = &args.kill {
        client::send_to_running_server(Request::Kill(name.clone())).await
    } else {
        let cfg = config::Config::read_and_validate(&args.configfile).unwrap_or_else(|err| {
            eprintln!("Config file error: {err}");
            std::process::exit(2);
        });

        if args.start {
            server::server(cfg).await
        } else if let Some(secs) = args.ui {
            // Config is only needed to start the server (key "s"), but loading
            // it here reports a bad config before the screen takes over.
            ui::ui(&args.configfile, secs.unwrap_or(DEFAULT_REFRESH_SECS)).await
        } else {
            // Config is only needed by the server, but loading it here
            // reports a bad config to the user before a server is spawned.
            client::client(
                // clap guarantees both for a client run (see args.rs).
                &args.conn.expect("--conn is required"),
                &args.sqlcmd.expect("--sqlcmd is required"),
                &args.session,
                &args.configfile,
            )
            .await
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}
