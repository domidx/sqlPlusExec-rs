//! The named pipe the client and the server talk over (Windows). On other
//! platforms only a stand-in exists, so that the rest of the program still
//! builds and its tests run there; it fails at runtime with `Unsupported`.

use std::io;
use std::sync::OnceLock;

use crate::constants::FALLBACK_PIPE_NAME;

/// The pipe name: \\.\pipe\<exe name without extension>-rs
/// (e.g. sqlplus_exec.exe -> \\.\pipe\sqlplus_exec-rs). The client starts the
/// server from the very same exe, so both sides always agree on the name.
pub fn pipe_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .map(|stem| format!(r"\\.\pipe\{stem}-rs"))
            .unwrap_or_else(|| FALLBACK_PIPE_NAME.to_string())
    })
}

#[cfg(windows)]
mod platform {
    use super::*;
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use tokio::time::sleep;

    use crate::constants::{ERROR_PIPE_BUSY, PIPE_BUSY_RETRY};

    /// The client end of the pipe.
    pub type PipeClient = NamedPipeClient;

    /// Accepts clients on the named pipe, always keeping a fresh pipe instance
    /// ready so that a new client is never refused while another is served.
    pub struct PipeListener {
        next: NamedPipeServer,
    }

    impl PipeListener {
        /// Creates the pipe. Fails if another server already owns it.
        pub fn bind() -> io::Result<Self> {
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .create(pipe_name())?;
            Ok(Self { next })
        }

        /// Waits for a client and returns its connected pipe instance.
        pub async fn accept(&mut self) -> io::Result<NamedPipeServer> {
            self.next.connect().await?;
            let fresh = ServerOptions::new().create(pipe_name())?;
            Ok(std::mem::replace(&mut self.next, fresh))
        }
    }

    /// Opens the pipe as a client. Fails (typically "not found") when no server
    /// is running; waits and retries only while all instances are busy.
    pub async fn connect_client() -> io::Result<PipeClient> {
        loop {
            match ClientOptions::new().open(pipe_name()) {
                Ok(client) => return Ok(client),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    sleep(PIPE_BUSY_RETRY).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    use tokio::io::DuplexStream;

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "named pipes are only available on Windows",
        ))
    }

    pub type PipeClient = DuplexStream;

    pub struct PipeListener;

    impl PipeListener {
        pub fn bind() -> io::Result<Self> {
            unsupported()
        }

        pub async fn accept(&mut self) -> io::Result<DuplexStream> {
            unsupported()
        }
    }

    pub async fn connect_client() -> io::Result<PipeClient> {
        unsupported()
    }
}

pub use platform::{connect_client, PipeClient, PipeListener};
