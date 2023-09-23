use super::fetch_fds;

use anyhow::Result;
use clap::Parser;
use std::os::unix::process::CommandExt;
use std::process::Command;

#[derive(Debug, Parser)]
pub(crate) struct Make {
    /// Fetch the file descriptors from this abstract socket.
    #[clap(long = "fd-socket")]
    fd_socket: String,

    /// Execute this command with the job server file descriptors.
    #[clap(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

impl Make {
    pub(crate) async fn execute(&self) -> Result<()> {
        let fds = fetch_fds(&self.fd_socket, 2)?;
        let read_fd = fds[0];
        let write_fd = fds[1];
        let makeflags = format!(
            "-j \
            --jobserver-fds={read_fd},{write_fd} \
            --jobserver-auth={read_fd},{write_fd}"
        );

        let err = Command::new(&self.command[0])
            .args(&self.command[1..])
            .env("CARGO_MAKEFLAGS", makeflags.clone())
            .env("MAKEFLAGS", makeflags.clone())
            .exec();

        Err(err.into())
    }
}
