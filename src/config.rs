use std::path::PathBuf;
use encoding_rs::Encoding;
use serde::{Deserialize, Deserializer, de};

use crate::constants::{
    CONFIG_FILENAME, DEFAULT_LOGIN_CMDS, DEFAULT_MAX_SESSIONS, DEFAULT_SQLPLUS,
};

fn default_max_sessions() -> usize {
    DEFAULT_MAX_SESSIONS
}

#[derive(Deserialize, Debug)]
pub struct Config {
    #[serde(default)]
    pub sqlplus_cmd: String,

    #[serde(default)]
    pub sqlplus_login: Vec<String>,

    /// Character set of sqlplus input/output, as an encoding label, e.g.
    /// "utf-8" (default when empty), "windows-1252". Should match the
    /// charset part of NLS_LANG used by sqlplus.
    #[serde(default)]
    pub sqlplus_encoding: String,

    /// Maximum number of sqlplus sessions kept open at once (default 10).
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,

    /// false = no log file (default), true = also write the server log to
    /// <exe name>.log next to the exe (recreated on every server start).
    /// Older config files say 0 / 1, which is still accepted.
    #[serde(default, deserialize_with = "bool_or_0_1")]
    pub enable_log_file: bool,
}

/// A flag written as true/false, or as 0/1 (the format older config files use).
fn bool_or_0_1<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Flag {
        Bool(bool),
        Int(i64),
    }

    match Flag::deserialize(deserializer)? {
        Flag::Bool(b) => Ok(b),
        Flag::Int(0) => Ok(false),
        Flag::Int(1) => Ok(true),
        Flag::Int(n) => Err(de::Error::custom(format!(
            "invalid value [{n}]: must be true, false, 1 or 0"
        ))),
    }
}

impl Config {
    /// The config file reference shown at the end of `--help`. Defaults are
    /// taken from the constants, so the text cannot drift from the code.
    pub fn help() -> String {
        let defaults = DEFAULT_LOGIN_CMDS
            .iter()
            .map(|c| format!("      {c}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\
CONFIG FILE:
  TOML file read by the server (--start) and checked by every other run.
  Location: --configfile <path>, else {CONFIG_FILENAME} next to the exe.
  Every option is optional; a missing option uses its default.

  sqlplus_cmd = \"\"
      Command that starts sqlplus, e.g. \"C:/oracle/bin/sqlplus.exe\".
      Empty = \"{DEFAULT_SQLPLUS}\" from PATH.

  sqlplus_login = []
      SQL*Plus lines run after every new database connect, e.g.
      [\"alter session set nls_date_format='YYYY-MM-DD'\"].
      They run before the built-in settings below, which win on conflict.

  sqlplus_encoding = \"\"
      Character set of sqlplus input/output, e.g. \"utf-8\", \"windows-1252\".
      Should match the charset part of NLS_LANG. Empty = utf-8.

  max_sessions = {DEFAULT_MAX_SESSIONS}
      Most sqlplus sessions (processes) kept open at once; 1 or more.
      A new session name beyond the limit is refused until one is released.

  enable_log_file = false
      false = log to the console only, true = also write <exe name>.log next
      to the exe (recreated on every server start). Logged SQL text is not
      masked. (0 and 1 are accepted for false and true.)

  Built-in settings, run after sqlplus_login on every connect:
{defaults}

  Example:
      sqlplus_cmd = \"C:/oracle/bin/sqlplus.exe\"
      sqlplus_login = [\"set pagesize 0\"]
      sqlplus_encoding = \"windows-1252\"
      max_sessions = 5
      enable_log_file = true"
        )
    }

    /// The configured encoding label without surrounding blanks ("" = not set).
    fn encoding_label(&self) -> &str {
        self.sqlplus_encoding.trim()
    }

    /// The configured sqlplus character set (UTF-8 if not set).
    pub fn encoding(&self) -> &'static Encoding {
        Encoding::for_label(self.encoding_label().as_bytes()).unwrap_or(encoding_rs::UTF_8)
    }

    /// The command that starts sqlplus: sqlplus_cmd, or plain "sqlplus"
    /// (found on PATH) when it is empty.
    pub fn sqlplus_command(&self) -> &str {
        let cmd = self.sqlplus_cmd.trim();
        if cmd.is_empty() { DEFAULT_SQLPLUS } else { cmd }
    }

    pub fn read_and_validate(configfile: &Option<String>) -> Result<Config, String> {
        // If configfile provided, use the path to read the config file.
        // Otherwise, use the same location as the exe file location
        // and the default config file name.
        let config_path = if let Some(path) = configfile {
            PathBuf::from(path)
        } else {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.join(CONFIG_FILENAME)))
                .unwrap_or_else(|| PathBuf::from(CONFIG_FILENAME))
        };

        // Read the content of the file.
        let content = match std::fs::read_to_string(&config_path) {
            Ok(content)   => content,
            Err(e)  => {
                return Err(format!("Error reading file [{}]: {}", config_path.display(), e));
            },
        };

        Self::parse_and_validate(&content)
    }

    /// Parses the TOML text of a config file and checks the values.
    fn parse_and_validate(content: &str) -> Result<Config, String> {
        // Parse TOML.
        let config: Config = toml::from_str(content)
            .map_err(|e| format!("Error parsing config file: {e}"))?;

        if config.max_sessions < 1 {
            return Err("Invalid max_sessions value [0]: must be 1 or more.".to_string());
        }

        let label = config.encoding_label();
        if !label.is_empty() && Encoding::for_label(label.as_bytes()).is_none() {
            return Err(format!(
                "Unknown sqlplus_encoding [{}] (examples: utf-8, windows-1252).",
                config.sqlplus_encoding
            ));
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_text: &str) -> Config {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn help_documents_every_option_and_the_real_defaults() {
        let help = Config::help();
        for key in ["sqlplus_cmd", "sqlplus_login", "sqlplus_encoding", "max_sessions", "enable_log_file"] {
            assert!(help.contains(key), "{key} missing");
        }
        assert!(help.contains("max_sessions = 10"));
        assert!(help.contains("enable_log_file = false"));
        assert!(help.contains("set define off"));
        assert!(help.contains(CONFIG_FILENAME));
    }

    #[test]
    fn sqlplus_command_falls_back_to_the_default() {
        assert_eq!(parse("").sqlplus_command(), "sqlplus");
        assert_eq!(parse("sqlplus_cmd = '   '").sqlplus_command(), "sqlplus");
        assert_eq!(parse("sqlplus_cmd = ' C:/ora/sqlplus.exe '").sqlplus_command(), "C:/ora/sqlplus.exe");
    }

    #[test]
    fn encoding_is_trimmed_and_defaults_to_utf8() {
        assert_eq!(parse("").encoding(), encoding_rs::UTF_8);
        assert_eq!(parse("sqlplus_encoding = ' windows-1252 '").encoding(), encoding_rs::WINDOWS_1252);
    }

    #[test]
    fn log_file_flag_accepts_bool_and_0_1() {
        let flag = |text: &str| Config::parse_and_validate(text).map(|c| c.enable_log_file);
        assert_eq!(flag(""), Ok(false));
        assert_eq!(flag("enable_log_file = true"), Ok(true));
        assert_eq!(flag("enable_log_file = false"), Ok(false));
        assert_eq!(flag("enable_log_file = 1"), Ok(true));
        assert_eq!(flag("enable_log_file = 0"), Ok(false));
        assert!(flag("enable_log_file = 2").unwrap_err().contains("true, false, 1 or 0"));
        assert!(flag("enable_log_file = 'yes'").is_err());
    }

    #[test]
    fn max_sessions_must_be_a_positive_number() {
        let max = |text: &str| Config::parse_and_validate(text).map(|c| c.max_sessions);
        assert_eq!(max(""), Ok(10));
        assert_eq!(max("max_sessions = 3"), Ok(3));
        assert!(max("max_sessions = 0").unwrap_err().contains("1 or more"));
        assert!(max("max_sessions = -1").is_err()); // not a usize
    }

    #[test]
    fn unknown_encoding_is_rejected() {
        let result = Config::parse_and_validate("sqlplus_encoding = 'klingon'");
        assert!(result.unwrap_err().contains("Unknown sqlplus_encoding"));
        assert!(Config::parse_and_validate("sqlplus_encoding = 'utf-8'").is_ok());
    }
}
