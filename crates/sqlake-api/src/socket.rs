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
use crate::snapshot::{ConnectionInfo, Status};

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
    if !is_a_name(session) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{session}` is not a session name: one path component, and not `.` or `..`"),
        ));
    }
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

/// Whether a name can be one component of the runtime directory and nothing
/// more.
///
/// Checked, because a name is typed by hand and exported by shell profiles, so
/// `$SQLAKE_SESSION=$PWD` is an ordinary mistake rather than an attack. A name
/// holding a `/` puts the socket outside the directory whose `0700` is what
/// keeps other users out — and [`Listener::bind`] would have created and
/// chmodded whatever directory it landed in on the way.
fn is_a_name(session: &str) -> bool {
    !session.is_empty() && session != "." && session != ".." && !session.contains(['/', '\0'])
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
            // Only a socket is a dead session. Anything else at this path was
            // put there by something that is not sqlake, and unlinking it
            // would be this deleting a file on a guess.
            if !is_a_socket(&path) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} is not a socket, and will not be removed to make room for one",
                        path.display()
                    ),
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
                Err(error) => {
                    tracing::warn!(%error, "accepting an agent connection");
                    // The failures that matter here are the ones that persist
                    // — the process out of descriptors — and they fail
                    // instantly, so a bare retry would spin a core and fill
                    // the log file for as long as the condition lasted.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
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
                // Refused is also what connecting to a plain file gives, and
                // that file is somebody else's. Only a socket is a session
                // that died.
                if is_a_socket(path) {
                    let _ = std::fs::remove_file(path);
                }
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

/// Whether the path itself is a socket.
///
/// `symlink_metadata`, not `metadata`: a symlink pointing at a socket is not
/// the socket, and following one would answer about a file that is not the one
/// about to be unlinked.
fn is_a_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt as _;

    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket())
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
    use sqlake_app::store::{Drivers, Store, Wiring};
    use sqlake_core::id::{ConnId, ProfileId};
    use sqlake_driver_mock::{Behaviour, MockDriver, MockProfiles};

    use super::*;
    use crate::serve::DEFAULT_TIMEOUT;

    #[test]
    fn closing_refuses_to_guess_which_connection() {
        // A read through the wrong connection is a wrong answer somebody can
        // ask again. A close through the wrong one is somebody else's
        // connection gone, so this is the one place the pick has to be
        // unambiguous.
        let two = open(&["mock", "prod"]);
        assert!(choose(&two, None, false).is_ok(), "a read still picks one");

        let refused = choose(&two, None, true).expect_err("closing should refuse");
        let Failure::Unsupported { message } = refused else {
            panic!("{refused:?}");
        };
        // Both names, so whoever reads it can pick one. Not the flag that
        // does the picking: this message crosses the socket to an MCP client
        // too, where `--connect` means nothing.
        assert!(
            message.contains("mock") && message.contains("prod"),
            "{message}"
        );
    }

    #[test]
    fn closing_picks_the_one_that_was_named() {
        let two = open(&["mock", "prod"]);
        assert_eq!(choose(&two, Some("prod"), true), Ok("id-1".into()));
        // And one connection is never ambiguous.
        assert!(choose(&open(&["mock"]), None, true).is_ok());
    }

    fn open(profiles: &[&str]) -> Vec<ConnectionInfo> {
        profiles
            .iter()
            .enumerate()
            .map(|(i, profile)| ConnectionInfo {
                id: format!("id-{i}"),
                profile: (*profile).to_owned(),
                name: (*profile).to_owned(),
                driver: "mock".into(),
                status: Status::Ready,
                capabilities: None,
            })
            .collect()
    }

    #[test]
    fn a_session_with_one_connection_needs_no_choosing() {
        assert_eq!(choose(&open(&["mock"]), None, false), Ok("id-0".into()));
    }

    #[test]
    fn connect_picks_among_a_sessions_connections() {
        // The point of `--connect` when attached: two databases open is
        // ordinary, and the first is not always the one meant.
        let open = open(&["staging", "prod"]);
        assert_eq!(choose(&open, Some("prod"), false), Ok("id-1".into()));
    }

    #[test]
    fn a_profile_the_session_has_not_opened_is_named_in_the_failure() {
        let open = open(&["staging"]);
        assert_eq!(
            choose(&open, Some("prod"), false),
            Err(Failure::NoSuchConnection {
                connection: "prod".into()
            })
        );
    }

    #[test]
    fn a_connection_that_failed_to_open_is_not_the_one_to_read_through() {
        // A session opened with two profiles where the first could not
        // connect: reading through it would answer with that failure, and the
        // database next to it is right there.
        let mut open = open(&["staging", "prod"]);
        open[0].status = Status::Failed {
            reason: "no route to host".into(),
        };
        assert_eq!(choose(&open, None, false), Ok("id-1".into()));
    }

    #[test]
    fn a_session_with_nothing_open_is_not_a_wrong_id() {
        // `NoSuchConnection` would send the caller looking for a typo in an id
        // it never gave.
        assert!(matches!(
            choose(&open(&[]), None, false),
            Err(Failure::Unsupported { .. })
        ));
    }

    /// A session listening on a socket, with one connection already open —
    /// which is the situation attaching exists for.
    async fn session(dir: &tempfile::TempDir) -> (PathBuf, String) {
        let store = Store::spawn(Wiring::new(
            Drivers::new().with(Arc::new(MockDriver::new(Behaviour::instant()))),
            Arc::new(MockProfiles::default()),
        ));
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
        // What a crash leaves behind: a real socket, with nothing behind it.
        drop(
            std::os::unix::net::UnixListener::bind(&path).expect("a socket with nothing behind it"),
        );

        assert!(
            Listener::bind(path.clone()).await.is_ok(),
            "a stale socket made the session unstartable until somebody deleted it"
        );
    }

    #[tokio::test]
    async fn something_that_is_not_a_socket_is_not_unlinked() {
        // The path is only ever sqlake's once the name is checked, but the
        // removal is a deletion and it should never rest on a guess.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("not-a-socket.sock");
        std::fs::write(&path, b"somebody else's").expect("a plain file");

        assert!(Listener::bind(path.clone()).await.is_err());
        // Whatever the platform makes of connecting to one — Linux refuses
        // it, macOS calls it ENOTSOCK — the file is not this to delete.
        drop(Client::attach(&path).await);
        assert!(path.exists(), "a file that was not a socket was deleted");
    }

    #[test]
    fn a_session_name_is_one_path_component() {
        // `$SQLAKE_SESSION=$PWD` is the mistake this is for: it would put the
        // socket outside the directory whose `0700` keeps other users out,
        // after `bind` had chmodded whatever directory it landed in.
        for bad in ["", ".", "..", "a/b", "../escape", "/tmp/absolute"] {
            assert!(!is_a_name(bad), "`{bad}` was taken as a name");
            assert!(socket_path(bad).is_err(), "`{bad}` named a socket");
        }
        assert!(is_a_name("work"));
        assert!(is_a_name("my-project.2"));
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

/// Somewhere to send a request: a session somebody else is running, or a store
/// this process started.
///
/// Both front-ends over this surface need the same choice, and they made it
/// twice before the MCP server arrived: attaching is tried first, because a
/// running session's connections, tunnels and already-answered credential
/// prompts are what a fresh store would have to pay for again — and for a
/// profile that needs a person, cannot.
#[derive(Debug)]
pub enum Backend {
    Attached(Client),
    /// Held in a `Box` because a `Service` owns a store and this enum is
    /// passed around by value.
    Local(Box<Service>),
}

impl Backend {
    /// Attach to the session at `path`, or build a local one.
    ///
    /// `start` is not called when a session answers, which is the point: it
    /// opens connections, and opening them to find out they were not needed is
    /// the cost attaching exists to avoid.
    ///
    /// # Errors
    ///
    /// Only a socket that is there and will not talk. Nothing listening is not
    /// an error — it is the ordinary case, and the answer to it is a local
    /// store.
    pub async fn attach_or_start(
        path: Option<&Path>,
        start: impl FnOnce() -> Service,
    ) -> io::Result<Self> {
        if let Some(path) = path
            && let Some(client) = Client::attach(path).await?
        {
            return Ok(Self::Attached(client));
        }
        Ok(Self::Local(Box::new(start())))
    }

    /// Whether this is a session that outlives the caller.
    ///
    /// What decides `Needs::LiveSession`: a store that dies with the process
    /// cannot hold a connection or a query for anybody to ask about later.
    #[must_use]
    pub const fn is_attached(&self) -> bool {
        matches!(self, Self::Attached(_))
    }

    /// Answer one request, wherever this is pointed.
    ///
    /// # Errors
    ///
    /// A socket that stopped answering. A local store cannot fail this way —
    /// its failures are [`Failure`]s inside the response, which is the same
    /// shape an attached one produces for the same reasons.
    pub async fn request(&mut self, request: &Request) -> io::Result<Response> {
        match self {
            Self::Attached(client) => client.request(request).await,
            Self::Local(service) => Ok(service.answer(request).await),
        }
    }

    /// The store behind a local backend, for the one thing a caller has to do
    /// to it directly: open the connection it will read through.
    #[must_use]
    pub const fn local(&self) -> Option<&Service> {
        match self {
            Self::Local(service) => Some(service),
            Self::Attached(_) => None,
        }
    }
}

/// Which of a session's connections to act on.
///
/// Shared by every front-end over this surface rather than written once per
/// front-end: the rules are protocol policy, and two copies would answer
/// differently about which connection an agent just wrote to.
///
/// `destructive` is what closing sets. Everything else reads, and a read
/// through the wrong connection is a wrong answer somebody can ask again; a
/// close through the wrong one is somebody else's connection gone.
///
/// # Errors
///
/// [`Failure`] describing which of the two ways it found nothing: a profile
/// that names no open connection, or a session with none at all.
/// The tail of the refusal [`choose`] gives when more than one connection
/// matches.
///
/// A constant rather than a phrase written twice: a front-end that has a way to
/// say which — the CLI has `--connect` — appends it to this refusal, and it
/// finds the refusal by this text. Two copies would drift, and the drift is
/// silent: the hint just stops appearing.
pub const AMBIGUOUS: &str = "name one to say which";

pub fn choose(
    open: &[ConnectionInfo],
    connect: Option<&str>,
    destructive: bool,
) -> Result<String, Failure> {
    // Everything when nothing was named: a session with two connections open
    // is ordinary, and narrowing that is what naming a profile is for.
    let asked_for = |c: &&ConnectionInfo| connect.is_none_or(|p| c.profile == p);
    if destructive && open.iter().filter(asked_for).count() > 1 {
        let names: Vec<&str> = open
            .iter()
            .filter(asked_for)
            .map(|c| c.profile.as_str())
            .collect();
        return Err(Failure::Unsupported {
            message: format!(
                "more than one connection matches ({}) — {AMBIGUOUS}",
                names.join(", ")
            ),
        });
    }
    // A ready one ahead of the rest, because a session whose first connection
    // failed to open still has a working second: taking one by position alone
    // would answer with that failure instead of with the database.
    let chosen = open
        .iter()
        .find(|c| asked_for(c) && c.status == Status::Ready)
        .or_else(|| open.iter().find(asked_for));
    chosen.map(|c| c.id.clone()).ok_or_else(|| match connect {
        // The id a caller could have meant is the profile it named, so that is
        // what the failure carries. Which connections *are* open is one
        // `connection_list` away and does not belong in this answer.
        Some(profile) => Failure::NoSuchConnection {
            connection: profile.to_owned(),
        },
        // Not "no such connection": the session is reachable and has none, so
        // there was never an id to get wrong.
        None => Failure::Unsupported {
            message: "the session has no connections open".to_owned(),
        },
    })
}
