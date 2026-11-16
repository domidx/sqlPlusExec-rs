use clap::{ArgGroup, CommandFactory, FromArgMatches, Parser};

use crate::config::Config;

/// What makes a run a control command instead of a client run.
const CONTROL_ARGS: [&str; 6] = ["start", "stop", "status", "release", "kill", "ui"];

/*
 * CLI arguments. clap does all the validation: at most one control
 * command, and --conn/--sqlcmd are required unless one is given.
 */
#[derive(Parser, Debug)]
#[command(about = "sqlplus executor, client/server mode.")]
#[command(group(ArgGroup::new("control").args(CONTROL_ARGS)))]
pub struct Args {
    /// Start server.
    #[arg(long)]
    pub start: bool,

    /// Stop server.
    #[arg(long)]
    pub stop: bool,

    /// Show whether the server is running and connected to a database.
    #[arg(long)]
    pub status: bool,

    /// Release a session: roll back its uncommitted work and mark it free for re-use.
    #[arg(long, value_name = "SESSION")]
    pub release: Option<String>,

    /// Kill a session: terminate its sqlplus process at once, even if it is busy.
    #[arg(long, value_name = "SESSION")]
    pub kill: Option<String>,

    /// Terminal UI: live session list with release/kill/start/stop.
    /// Optional value = refresh interval in seconds (default 5).
    #[arg(long, num_args = 0..=1, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..))]
    pub ui: Option<Option<u64>>,

    /// Session to run in (default: "default"). Each session is a separate sqlplus process.
    #[arg(long, value_name = "SESSION")]
    pub session: Option<String>,

    /// Connection string [username/password@tnsname].
    #[arg(long, required_unless_present_any = CONTROL_ARGS)]
    pub conn: Option<String>,

    /// Multiple values, each representing one line to run in sqlplus session.
    #[arg(long, num_args = 1.., required_unless_present_any = CONTROL_ARGS)]
    pub sqlcmd: Option<Vec<String>>,

    /// Location of config file (default: next to the exe; options are listed below).
    #[arg(long)]
    pub configfile: Option<String>,

//    /// Format of query output.
//    #[arg(long = "outputformat", value_parser = ["csv", "simple", "presto"])]
//    pub outputformat: Option<String>,
}

impl Args {
    /// Parses the command line; on a problem clap prints it and exits with 2.
    pub fn read() -> Args {
        // Same as Args::parse(), plus the config file reference at the end of --help.
        let matches = Args::command()
            .after_long_help(Config::help())
            .get_matches();
        Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("sqlPlusExec").chain(args.iter().copied()))
    }

    fn fails(args: &[&str]) -> String {
        match parse(args) {
            Ok(a) => panic!("expected an error for {args:?}, got {a:?}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_client_run_needs_conn_and_sqlcmd() {
        let a = parse(&["--conn", "u/p@db", "--sqlcmd", "select 1;", "from dual;"]).unwrap();
        assert_eq!(a.sqlcmd.unwrap(), ["select 1;", "from dual;"]);
        assert!(fails(&["--sqlcmd", "select 1;"]).contains("--conn"));
        assert!(fails(&["--conn", "u/p@db"]).contains("--sqlcmd"));
        assert!(fails(&[]).contains("--conn"));
    }

    #[test]
    fn control_commands_need_neither() {
        assert!(parse(&["--start"]).unwrap().start);
        assert!(parse(&["--stop"]).unwrap().stop);
        assert!(parse(&["--status"]).unwrap().status);
        assert_eq!(parse(&["--release", "v1"]).unwrap().release.as_deref(), Some("v1"));
        assert_eq!(parse(&["--kill", "v1"]).unwrap().kill.as_deref(), Some("v1"));
        assert_eq!(parse(&["--ui"]).unwrap().ui, Some(None));
    }

    #[test]
    fn only_one_control_command_at_a_time() {
        assert!(fails(&["--stop", "--status"]).contains("cannot be used with"));
        assert!(fails(&["--start", "--ui"]).contains("cannot be used with"));
        assert!(fails(&["--release", "a", "--kill", "b"]).contains("cannot be used with"));
    }

    #[test]
    fn ui_interval_must_be_one_second_or_more() {
        assert_eq!(parse(&["--ui", "5"]).unwrap().ui, Some(Some(5)));
        assert_eq!(parse(&["--ui", "1"]).unwrap().ui, Some(Some(1)));
        assert!(fails(&["--ui", "0"]).contains("--ui"));
        assert!(fails(&["--ui", "abc"]).contains("--ui"));
    }

    #[test]
    fn help_ends_with_the_config_file_reference() {
        let mut help = Vec::new();
        Args::command().after_long_help(Config::help()).write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        assert!(help.contains("CONFIG FILE:") && help.contains("max_sessions"));
    }
}
