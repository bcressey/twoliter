mod link;
mod make;

use self::link::Link;
use self::make::Make;
use pipesys::server::Server as Serve;

use anyhow::{ensure, Context, Result};
use clap::Parser;
use env_logger::Builder;
use log::{debug, LevelFilter};
use nix::fcntl::{fcntl, F_DUPFD};

const DEFAULT_LEVEL_FILTER: LevelFilter = LevelFilter::Info;

// Don't accept file descriptors 0, 1, or 2 since those correspond to the well-known stdin, stdout,
// and stderr which could confuse the calling process or its children.
const MIN_FD: i32 = 3;

/// A tool for passing file descriptors into builds.
#[derive(Debug, Parser)]
#[clap(about, long_about = None, version)]
pub(crate) struct Args {
    /// Set the logging level. One of [off|error|warn|info|debug|trace]. Defaults to warn. You can
    /// also leave this unset and use the RUST_LOG env variable. See
    /// https://github.com/rust-cli/env_logger/
    #[clap(long = "log-level")]
    pub(crate) log_level: Option<LevelFilter>,

    #[clap(subcommand)]
    pub(crate) subcommand: Subcommand,
}

#[derive(Debug, Parser)]
pub(crate) enum Subcommand {
    /// Serve file descriptors to clients.
    Serve(Serve),

    /// Set job server file descriptors for child process.
    Make(Make),

    /// Link a directory file descriptor to the target path.
    Link(Link),
}

/// Entrypoint for the `pipesys` command line program.
pub(super) async fn run(args: Args) -> Result<()> {
    match args.subcommand {
        Subcommand::Serve(serve_args) => serve_args.serve().await,
        Subcommand::Make(make_args) => make_args.execute().await,
        Subcommand::Link(link_args) => link_args.execute().await,
    }
}

/// use `level` if present, or else use `RUST_LOG` if present, or else use a default.
pub(super) fn init_logger(level: Option<LevelFilter>) {
    match (std::env::var(env_logger::DEFAULT_FILTER_ENV).ok(), level) {
        (Some(_), None) => {
            // RUST_LOG exists and level does not; use the environment variable.
            Builder::from_default_env().init();
        }
        _ => {
            // use provided log level or default for this crate only.
            Builder::new()
                .filter(
                    Some(env!("CARGO_CRATE_NAME")),
                    level.unwrap_or(DEFAULT_LEVEL_FILTER),
                )
                .init();
        }
    }
}

/// Helper function to retrieve file descriptors via an abstract socket.
fn fetch_fds(socket: &str, wanted: usize) -> Result<Vec<i32>> {
    let addr = uds::UnixSocketAddr::from_abstract(socket.as_bytes())
        .with_context(|| format!("failed to create socket {}", socket))?;
    let client = uds::UnixSeqpacketConn::connect_unix_addr(&addr)
        .with_context(|| format!("failed to connect to socket {}", socket))?;

    let mut fd_buf = [-1; 8];
    let (_, _, fds) = client
        .recv_fds(&mut [0u8; 1], &mut fd_buf)
        .with_context(|| format!("failed to receive file descriptors from socket {}", socket))?;

    ensure!(
        fds == wanted,
        format!("received {fds} file descriptors, expected 1")
    );

    // If a received file descriptor has the CLOEXEC flag set, it might close unexpectedly when
    // executing a child process. Duplicate it without that flag to ensure it stays valid.
    let mut dupfds = Vec::with_capacity(fds);
    for fd in fd_buf.iter().filter(|fd| **fd >= MIN_FD) {
        let dupfd = duplicate_fd(*fd)
            .with_context(|| format!("failed to duplicate file descriptor {fd}"))?;
        debug!("duplicated file descriptor {fd} to {dupfd}");
        dupfds.push(dupfd);
    }

    Ok(dupfds)
}

/// Duplicate file descriptors without the CLOEXEC flag set.
fn duplicate_fd(fd: i32) -> Result<i32> {
    let newfd = fcntl(fd, F_DUPFD(MIN_FD))
        .with_context(|| format!("failed to duplicate file descriptor {fd}"))?;
    Ok(newfd)
}
