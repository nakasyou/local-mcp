use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(windows)]
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use crate::config::{self, Session};

trait SessionIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> SessionIo for T {}
type SessionStream = Box<dyn SessionIo>;

#[cfg(unix)]
struct SessionListener(UnixListener);
#[cfg(windows)]
struct SessionListener {
    path: PathBuf,
    server: NamedPipeServer,
}

impl SessionListener {
    async fn accept(&mut self) -> Result<SessionStream> {
        #[cfg(unix)]
        {
            return Ok(Box::new(self.0.accept().await?.0));
        }
        #[cfg(windows)]
        {
            // Keep the connecting instance in listener state while awaiting so
            // cancelling this future from `tokio::select!` cannot drop it.
            self.server.connect().await?;
            let server = std::mem::replace(&mut self.server, new_pipe(&self.path, false)?);
            return Ok(Box::new(server));
        }
    }
}

#[cfg(windows)]
fn new_pipe(path: &Path, first: bool) -> Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options.first_pipe_instance(first);
    Ok(options.create(path)?)
}

async fn connect(path: &Path) -> Result<SessionStream> {
    #[cfg(unix)]
    {
        Ok(Box::new(UnixStream::connect(path).await?))
    }
    #[cfg(windows)]
    {
        const ERROR_PIPE_BUSY: i32 = 231;
        const RETRY_DELAY: Duration = Duration::from_millis(10);
        const RETRY_TIMEOUT: Duration = Duration::from_secs(5);

        let deadline = Instant::now() + RETRY_TIMEOUT;
        loop {
            match ClientOptions::new().open(path) {
                Ok(client) => return Ok(Box::new(client)),
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                        && Instant::now() < deadline =>
                {
                    sleep(RETRY_DELAY).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

#[cfg(unix)]
fn bind_listener(path: &Path) -> Result<SessionListener> {
    Ok(SessionListener(UnixListener::bind(path)?))
}

#[cfg(windows)]
fn bind_listener(path: &Path) -> Result<SessionListener> {
    Ok(SessionListener {
        path: path.to_owned(),
        server: new_pipe(path, true)?,
    })
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub id: Uuid,
    pub operation: String,
    pub detail: String,
    pub cwd: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Approval {
        request: Request,
    },
    Activity {
        title: String,
        detail: Option<String>,
    },
}

pub async fn request(
    session_id: &str,
    operation: &str,
    detail: String,
    cwd: PathBuf,
) -> Result<bool> {
    let request = Request {
        id: Uuid::new_v4(),
        operation: operation.to_owned(),
        detail,
        cwd,
    };
    let path = config::socket_path(session_id)?;
    let mut stream = connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `local-mcp start`"))?;
    stream
        .write_all(&serde_json::to_vec(&Message::Approval { request })?)
        .await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    match response.trim() {
        "allow" => Ok(true),
        "deny" => Ok(false),
        value => anyhow::bail!("invalid response from session: {value:?}"),
    }
}

/// Sends a one-way activity update to the `start` screen. Activity reporting is
/// deliberately best-effort: an MCP operation must not fail just because its UI
/// was closed between loading the session and completing the operation.
pub async fn activity(session_id: &str, title: impl Into<String>, detail: Option<String>) {
    let Ok(path) = config::socket_path(session_id) else {
        return;
    };
    let Ok(mut stream) = connect(&path).await else {
        return;
    };
    let message = Message::Activity {
        title: title.into(),
        detail,
    };
    let Ok(bytes) = serde_json::to_vec(&message) else {
        return;
    };
    let _ = stream.write_all(&bytes).await;
    let _ = stream.write_all(b"\n").await;
    let _ = stream.shutdown().await;
}

pub async fn start(session_id: Option<&str>) -> Result<()> {
    let mut session = config::create_session(&std::env::current_dir()?, session_id).await?;
    let path = config::socket_path(&session.id)?;
    #[cfg(unix)]
    {
        let state_dir = path.parent().context("session socket has no parent")?;
        tokio::fs::create_dir_all(state_dir).await?;
        tokio::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).await?;
    }
    remove_stale_socket(&path).await?;

    let mut listener =
        bind_listener(&path).with_context(|| format!("failed to listen at {}", path.display()))?;
    #[cfg(unix)]
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    eprintln!(
        "local-mcp session: {}\ncwd: {}\n\
         Give this session ID to the agent so it can include it in local-mcp tool calls.\n\
         Commands: /permission ask|yolo|allow <directory>|revoke <directory>|list|status\n\
         Press Ctrl-C to stop.",
        session.id,
        session.cwd.display()
    );

    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut pending = VecDeque::<(Request, SessionStream)>::new();
    let mut yolo = false;
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let mut stream = connection?;
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).await?;
                let message: Message = serde_json::from_str(&line).context("invalid session message")?;
                match message {
                    Message::Activity { title, detail } => show_activity(&title, detail.as_deref()),
                    Message::Approval { request } if yolo => {
                        eprintln!("[yolo] allowing {}: {}", request.operation, request.detail);
                        stream.write_all(b"allow\n").await?;
                    }
                    Message::Approval { request } => {
                        show_request(&request)?;
                        pending.push_back((request, stream));
                    }
                }
            }
            line = input.next_line() => {
                let Some(line) = line? else { anyhow::bail!("session input closed") };
                handle_input(line.trim(), &mut session, &mut yolo, &mut pending).await?;
            }
        }
    }
}

fn show_activity(title: &str, detail: Option<&str>) {
    eprintln!("\n• {title}");
    if let Some(detail) = detail.filter(|value| !value.is_empty()) {
        for line in detail.lines() {
            eprintln!("  {line}");
        }
    }
}

#[cfg(unix)]
async fn remove_stale_socket(path: &PathBuf) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to remove stale session socket"),
    }
}

#[cfg(windows)]
async fn remove_stale_socket(_path: &PathBuf) -> Result<()> {
    Ok(())
}

async fn handle_input(
    input: &str,
    session: &mut Session,
    yolo: &mut bool,
    pending: &mut VecDeque<(Request, SessionStream)>,
) -> Result<()> {
    match input {
        "/permissions yolo" | "/permission yolo" => {
            *yolo = true;
            eprintln!("Permissions: yolo (all unsandboxed calls are allowed for this session)");
            while let Some((request, mut stream)) = pending.pop_front() {
                eprintln!("[yolo] allowing {}: {}", request.operation, request.detail);
                stream.write_all(b"allow\n").await?;
            }
        }
        "/permissions ask" | "/permission ask" => {
            *yolo = false;
            eprintln!("Permissions: ask");
        }
        "y" | "Y" | "yes" | "YES" if !pending.is_empty() => {
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"allow\n").await?;
            show_next(pending)?;
        }
        "n" | "N" | "no" | "NO" if !pending.is_empty() => {
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"deny\n").await?;
            show_next(pending)?;
        }
        "/permission list" | "/permissions list" => show_permissions(session),
        "/permission status" | "/permissions status" => {
            eprintln!("Permissions: {}", if *yolo { "yolo" } else { "ask" });
            show_permissions(session);
        }
        command if permission_arg(command, "allow").is_some() => {
            let directory = config::canonical_directory(
                PathBuf::from(permission_arg(command, "allow").unwrap()).as_path(),
            )?;
            if !session.permitted_directories.contains(&directory) {
                session.permitted_directories.push(directory.clone());
                session.permitted_directories.sort();
                config::save_session(session).await?;
            }
            eprintln!("Allowed sandbox root: {}", directory.display());
        }
        command if permission_arg(command, "revoke").is_some() => {
            let directory = config::canonical_directory(
                PathBuf::from(permission_arg(command, "revoke").unwrap()).as_path(),
            )?;
            if directory == session.cwd {
                eprintln!("Cannot revoke the session cwd");
            } else {
                session
                    .permitted_directories
                    .retain(|item| item != &directory);
                config::save_session(session).await?;
                eprintln!("Revoked sandbox root: {}", directory.display());
            }
        }
        "/permission" | "/permissions" | "/permission help" | "/permissions help" => {
            eprintln!("/permission ask|yolo|allow <directory>|revoke <directory>|list|status");
        }
        "" => {}
        command if !pending.is_empty() => {
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"deny\n").await?;
            eprintln!("Denied request (unrecognized response: {command})");
            show_next(pending)?;
        }
        command => eprintln!("Unknown command: {command}"),
    }
    Ok(())
}

fn permission_arg<'a>(command: &'a str, action: &str) -> Option<&'a str> {
    ["/permission", "/permissions"]
        .into_iter()
        .find_map(|prefix| {
            command
                .strip_prefix(&format!("{prefix} {action} "))
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
}

fn show_permissions(session: &Session) {
    eprintln!("Sandbox roots:");
    for path in &session.permitted_directories {
        eprintln!("  {}", path.display());
    }
}

fn show_request(request: &Request) -> Result<()> {
    eprintln!(
        "\n[{}] {}\ncwd: {}\n{}",
        request.id,
        request.operation,
        request.cwd.display(),
        request.detail
    );
    eprint!("Allow without sandbox? [y/N] ");
    std::io::stderr().flush()?;
    Ok(())
}

fn show_next(pending: &VecDeque<(Request, SessionStream)>) -> Result<()> {
    if let Some((request, _)) = pending.front() {
        show_request(request)?;
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use tokio::time::timeout;

    fn pipe_path() -> PathBuf {
        PathBuf::from(format!(r"\\.\pipe\local-mcp-test-{}", Uuid::new_v4()))
    }

    #[tokio::test]
    async fn cancelling_accept_preserves_the_pipe_server() -> Result<()> {
        let path = pipe_path();
        let mut listener = bind_listener(&path)?;

        assert!(
            timeout(Duration::from_millis(10), listener.accept())
                .await
                .is_err()
        );
        let _client = ClientOptions::new().open(&path)?;
        let _server = timeout(Duration::from_secs(1), listener.accept()).await??;

        Ok(())
    }

    #[tokio::test]
    async fn connecting_retries_while_all_pipe_instances_are_busy() -> Result<()> {
        let path = pipe_path();
        let mut listener = bind_listener(&path)?;
        let _first_client = ClientOptions::new().open(&path)?;

        let client_path = path.clone();
        let waiting_client = tokio::spawn(async move { connect(&client_path).await });
        sleep(Duration::from_millis(50)).await;
        let _first_server = listener.accept().await?;
        let _second_client = timeout(Duration::from_secs(1), waiting_client).await???;

        Ok(())
    }
}
