use anyhow::{Context, Result};
use clap::Parser;
use log::warn;
use std::fs::OpenOptions;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use uds::{tokio::UnixSeqpacketListener, UnixSocketAddr};

#[derive(Clone, Debug, Parser)]
pub struct Server {
    /// Listen on this abstract socket.
    #[clap(long = "socket")]
    socket: String,

    /// Expect clients with this UID.
    #[clap(long = "client-uid")]
    client_uid: u32,

    /// Send file descriptors from these paths.
    #[clap(long = "path")]
    paths: Option<Vec<PathBuf>>,

    /// Send file descriptors from the current process.
    #[clap(long = "fd")]
    fds: Option<Vec<RawFd>>,
}

impl Server {
    pub fn for_paths<S, P>(socket: S, client_uid: u32, paths: &[P]) -> Self
    where
        S: AsRef<str>,
        P: AsRef<Path>,
    {
        let socket = socket.as_ref().to_string();
        let paths = Some(paths.iter().map(|p| PathBuf::from(p.as_ref())).collect());
        let fds = None;
        Self {
            socket,
            client_uid,
            paths,
            fds,
        }
    }

    pub fn for_fds<S: AsRef<str>>(socket: S, client_uid: u32, fds: &[RawFd]) -> Self {
        let socket = socket.as_ref().to_string();
        let paths = None;
        let fds = Some(fds.to_vec());
        Self {
            socket,
            client_uid,
            paths,
            fds,
        }
    }

    pub async fn serve(&self) -> Result<()> {
        let addr = UnixSocketAddr::from_abstract(self.socket.as_bytes())
            .with_context(|| format!("failed to create socket {}", self.socket))?;
        let mut listener = UnixSeqpacketListener::bind_addr(&addr)
            .with_context(|| format!("failed to bind to socket {}", self.socket))?;

        let mut serve_fds = Vec::new();
        let mut file_handles = Vec::new();

        if let Some(paths) = &self.paths {
            for path in paths.iter() {
                let f = OpenOptions::new()
                    .create(false)
                    .read(true)
                    .write(false)
                    .open(path)
                    .with_context(|| format!("could not open {}", path.display()))?;

                // We need to send the raw file descriptor, but for it to remain valid we can't
                // drop the file we opened to get it, so we save the file objects as well.
                let fd = f.as_raw_fd();
                serve_fds.push(fd);
                file_handles.push(f);
            }
        }

        if let Some(fds) = &self.fds {
            serve_fds.extend(fds);
        }

        loop {
            let (mut conn, _) = listener.accept().await.with_context(|| {
                format!("failed to accept connection on socket {}", self.socket)
            })?;

            let peer_creds = conn.initial_peer_credentials().with_context(|| {
                format!(
                    "failed to obtain peer credentials on socket {}",
                    self.socket
                )
            })?;

            let peer_uid = peer_creds.euid();
            if peer_uid != self.client_uid {
                warn!("ignoring connection from peer with UID {}", peer_uid);
                continue;
            }

            let s = self.clone();
            let fds = serve_fds.clone();
            tokio::spawn(async move {
                conn.send_fds(b"fds", &fds)
                    .await
                    .with_context(|| format!("failed to send file descriptors over {}", s.socket))
            });
        }
    }
}
