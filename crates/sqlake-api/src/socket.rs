//! The socket a running session answers on, and the client that reaches it.
//!
//! A Unix socket, never TCP. This process holds live database connections and
//! resolved credentials, so "listening" is a decision about who can reach those
//! — and the answer is the user who started it and nobody else. There is no
//! flag to change that, because there is no configuration of a TCP listener
//! that would be safe enough to offer.
//!
//! Newline-delimited JSON, one request per line and one response per line. The
//! connection stays open, so a caller asking three things pays for one connect.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::protocol::{Failure, Request, Response};
use crate::serve::Service;

/// The session a name resolves to when nobody said.
pub const DEFAULT_SESSION: &str = "default";

/// Which session this process is talking about.
///
/// `--session`, then `$SQLAKE_SESSION`, then `default`. Per-project addressing
/// is a naming convention on top of this rather than a feature: a shell that
/// exports `SQLAKE_SESSION=thing` in one directory has it, and nothing here has
/// to know what a project is.
#[must_use]
pub fn session_name(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_owned)
        .or_else(|| {
            std::env::var("SQLAKE_SESSION")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_SESSION.to_owned())
}

/// The longest socket path this bothers to try.
///
/// `sockaddr_un` holds 104 bytes on macOS and 108 on Linux, and a path over
/// that is refused by `bind` and by `connect` alike. Checked here rather than
/// left to either, because the two would report it at different moments and a
/// client would read "cannot connect" as "no session is running" — then start a
/// second store against the database the session already has open.
const MAX_PATH: usize = 100;

/// Where a session of this name listens.
///
/// # Errors
///
/// If neither a runtime directory nor a home directory can be found, or if the
/// path that gives is longer than a Unix socket can be named by.
pub fn socket_path(session: &str) -> io::Result<PathBuf> {
    let dir = sqlake_config::paths::runtime_dir().map_err(io::Error::other)?;
    let path = dir.join(format!("{session}.sock"));
    if path.as_os_str().len() > MAX_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the socket path is {} bytes, and a Unix socket can be named by at most {MAX_PATH}: {}",
                path.as_os_str().len(),
                path.display()
            ),
        ));
    }
    Ok(path)
}

/// A bound socket, which is removed when this is dropped.
#[derive(Debug)]
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Bind the socket for this session.
    ///
    /// A path that is already there is only in the way if something is behind
    /// it: a socket file outlives the process that made it, and refusing to
    /// start over one would mean every crash needs a manual `rm` before the
    /// client works again (D8). So a file that nothing answers on is removed
    /// and rebound, and one that answers is a session already running.
    ///
    /// # Errors
    ///
    /// If the directory cannot be made, if a session of this name is already
    /// listening, or if binding fails.
    pub async fn bind(path: PathBuf) -> io::Result<Self> {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        // The directory is what actually keeps other users out. Between `bind`
        // and a `chmod` on the socket there is a window where the socket's own
        // mode is whatever the umask allowed, and a directory nobody else can
        // enter closes it — so this permission is load-bearing, not belt and
        // braces.
        restrict(dir)?;

        if path.exists() {
            if UnixStream::connect(&path).await.is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("a session is already listening on {}", path.display()),
                ));
            }
            std::fs::remove_file(&path)?;
        }

        let listener = UnixListener::bind(&path)?;
        restrict(&path)?;
        Ok(Self { listener, path })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Answer on this runtime until the returned guard is dropped.
    #[must_use]
    pub fn spawn_on(
        self,
        runtime: &tokio::runtime::Runtime,
        service: Arc<Service>,
    ) -> ListenerHandle {
        let path = self.path.clone();
        ListenerHandle {
            path,
            task: runtime.spawn(self.serve(service)),
        }
    }

    /// Answer requests until the process ends.
    ///
    /// One task per connection, because a caller waiting thirty seconds for a
    /// cold connection must not be why another caller's `connection list` is
    /// slow. They share one store, which is the point of attaching.
    pub async fn serve(self, service: Arc<Service>) {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    let service = Arc::clone(&service);
                    tokio::spawn(async move { converse(stream, &service).await });
                }
                // Accept failing is not a reason to stop answering: a single
                // client hitting a file-descriptor limit would otherwise take
                // the session's socket down with it.
                Err(error) => tracing::warn!(%error, "accepting an agent connection"),
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Best effort. The next `bind` removes a file nothing answers on
        // anyway, so failing here costs nothing.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Keeps a session answering for as long as it is held.
///
/// A guard rather than a detached task, so the socket's lifetime is the client
/// process's and is visible where the client is started. Dropping it removes
/// the socket file on this thread: aborting the accept loop drops the
/// [`Listener`] inside it eventually, and "eventually" is not a promise worth
/// making about a file the next run has to deal with.
#[derive(Debug)]
pub struct ListenerHandle {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ListenerHandle {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One client, for as long as it stays connected.
async fn converse(stream: UnixStream, service: &Service) {
    let mut lines = BufReader::new(stream).lines();
    let mut pending = Vec::new();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(error) => {
                tracing::debug!(%error, "an agent connection ended");
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => service.answer(&request).await,
            // Answered rather than dropped: a client that sent something this
            // build does not understand gets told so, instead of watching a
            // socket go quiet and having to guess whether the session died.
            Err(why) => Response::Failed(Failure::Malformed {
                message: why.to_string(),
            }),
        };

        pending.clear();
        if serde_json::to_writer(&mut pending, &response).is_err() {
            return;
        }
        pending.push(b'\n');
        if lines.get_mut().write_all(&pending).await.is_err() {
            return;
        }
    }
}

/// A connection to a running session.
#[derive(Debug)]
pub struct Client {
    stream: BufReader<UnixStream>,
    line: String,
}

impl Client {
    /// Attach to the session at this path, if there is one.
    ///
    /// `None` rather than an error when nothing is listening: having no session
    /// is the ordinary case, and the caller's answer to it is to run the
    /// request itself rather than to report a failure. A socket file with
    /// nothing behind it is removed on the way past — it is not a session, and
    /// leaving it would make every later command pay the same failed connect
    /// (D8).
    ///
    /// # Errors
    ///
    /// If the socket exists and is listening but cannot be connected to.
    pub async fn attach(path: &Path) -> io::Result<Option<Self>> {
        match UnixStream::connect(path).await {
            Ok(stream) => Ok(Some(Self {
                stream: BufReader::new(stream),
                line: String::new(),
            })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                let _ = std::fs::remove_file(path);
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Send one request and read its answer.
    ///
    /// # Errors
    ///
    /// If the session goes away, or answers with something that is not a
    /// response.
    pub async fn request(&mut self, request: &Request) -> io::Result<Response> {
        let mut line = serde_json::to_vec(request).map_err(io::Error::other)?;
        line.push(b'\n');
        self.stream.get_mut().write_all(&line).await?;

        self.line.clear();
        if self.stream.read_line(&mut self.line).await? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the session closed the connection without answering",
            ));
        }
        serde_json::from_str(&self.line).map_err(io::Error::other)
    }
}

/// Owner-only, which on a path holding live credentials is the whole point.
fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;

    use sqlake_app::action::Action;
    use sqlake_app::store::{Drivers, Store};
    use sqlake_core::id::{ConnId, ProfileId};
    use sqlake_core::result::PageRequest;
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles};

    use super::*;
    use crate::serve::DEFAULT_TIMEOUT;

    /// A session listening on a socket, with one connection already open —
    /// which is the situation attaching exists for.
    async fn session(dir: &tempfile::TempDir) -> (PathBuf, String) {
        let store = Store::spawn(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::default()),
            PageRequest::DEFAULT_LIMIT,
        );
        let conn = ConnId::new();
        store
            .dispatch_and_settle(
                Action::Connect {
                    profile: ProfileId::parse("mock").expect("a usable id"),
                    conn,
                },
                DEFAULT_TIMEOUT,
                |s| s.connection_settled(conn),
            )
            .await
            .expect("the connection settles");

        let path = dir.path().join("test.sock");
        let listener = Listener::bind(path.clone()).await.expect("it binds");
        tokio::spawn(listener.serve(Arc::new(Service::new(store))));
        (path, conn.to_string())
    }

    #[tokio::test]
    async fn a_name_falls_back_to_the_default() {
        assert_eq!(session_name(Some("work")), "work");
        // `$SQLAKE_SESSION` is not exercised here: reading it is one line, and
        // a test for it would have to set a process-wide variable that every
        // other test in this binary shares.
        assert_eq!(
            session_name(None),
            std::env::var("SQLAKE_SESSION")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_SESSION.to_owned())
        );
    }

    #[tokio::test]
    async fn the_socket_and_its_directory_are_owner_only() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("nested").join("s.sock");
        let listener = Listener::bind(path.clone()).await.expect("it binds");

        let socket = std::fs::metadata(&path).expect("the socket exists");
        assert_eq!(socket.permissions().mode() & 0o777, 0o600);
        let parent = std::fs::metadata(path.parent().expect("a parent")).expect("the directory");
        assert_eq!(parent.permissions().mode() & 0o777, 0o700);
        drop(listener);
    }

    #[tokio::test]
    async fn the_socket_goes_when_the_listener_does() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("s.sock");
        let listener = Listener::bind(path.clone()).await.expect("it binds");
        assert!(path.exists());
        drop(listener);
        assert!(!path.exists(), "a socket file outlived its session");
    }

    #[tokio::test]
    async fn a_socket_nothing_answers_on_is_not_a_session() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("stale.sock");
        // What a crash leaves behind: the file, with nothing behind it.
        std::fs::write(&path, b"").expect("a stale file");

        assert!(
            Listener::bind(path.clone()).await.is_ok(),
            "a stale file made the session unstartable until somebody deleted it"
        );
    }

    #[tokio::test]
    async fn two_sessions_cannot_share_one_name() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("s.sock");
        let first = Listener::bind(path.clone()).await.expect("it binds");
        let second = Listener::bind(path.clone()).await;
        assert!(
            second.is_err(),
            "the second session would have taken the first's socket"
        );
        drop(first);
    }

    #[tokio::test]
    async fn there_is_no_session_when_nothing_is_listening() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("absent.sock");
        assert!(Client::attach(&path).await.expect("no error").is_none());
    }

    #[tokio::test]
    async fn attaching_to_a_stale_socket_clears_it_away() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("stale.sock");
        // A real socket with no listener, which is what a process killed
        // before its `Drop` ran leaves behind. `std`'s listener does not
        // remove the file, so dropping it is exactly that state — where
        // `mem::forget` would keep the descriptor open and still answer.
        drop(
            std::os::unix::net::UnixListener::bind(&path).expect("a socket with nothing behind it"),
        );

        assert!(Client::attach(&path).await.expect("no error").is_none());
        assert!(
            !path.exists(),
            "the dead socket was left for the next caller"
        );
    }

    #[tokio::test]
    async fn a_client_reads_the_connection_the_session_already_had_open() {
        // The whole point of attaching: the connection was opened by somebody
        // else's process, and this caller pays none of what it cost to open.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (path, conn) = session(&dir).await;

        let mut client = Client::attach(&path)
            .await
            .expect("no error")
            .expect("a session is listening");
        let response = client
            .request(&Request::ConnectionList {})
            .await
            .expect("it answers");

        match response {
            Response::Connections(connections) => {
                assert_eq!(connections.len(), 1);
                assert_eq!(connections[0].id, conn);
            }
            other => panic!("expected connections, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn one_connection_answers_more_than_one_request() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (path, _) = session(&dir).await;
        let mut client = Client::attach(&path)
            .await
            .expect("no error")
            .expect("a session");

        for _ in 0..3 {
            assert!(matches!(
                client
                    .request(&Request::Schema {})
                    .await
                    .expect("it answers"),
                Response::Schema(_)
            ));
        }
    }

    #[tokio::test]
    async fn a_line_that_is_not_a_request_is_answered_rather_than_dropped() {
        // A client that sent something this build does not understand must not
        // be left watching a silent socket, guessing whether the session died.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let (path, _) = session(&dir).await;

        let mut stream = BufReader::new(UnixStream::connect(&path).await.expect("it connects"));
        stream
            .get_mut()
            .write_all(b"{\"request\": \"no_such_request\"}\n")
            .await
            .expect("it writes");

        let mut line = String::new();
        stream.read_line(&mut line).await.expect("it answers");
        let response: Response = serde_json::from_str(&line).expect("a response");
        assert!(
            matches!(response, Response::Failed(Failure::Malformed { .. })),
            "{response:?}"
        );

        // And the session is still there for the next request.
        assert!(matches!(
            Client::attach(&path)
                .await
                .expect("no error")
                .expect("a session")
                .request(&Request::Schema {})
                .await
                .expect("it answers"),
            Response::Schema(_)
        ));
    }
}
