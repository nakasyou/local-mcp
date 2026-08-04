use std::collections::VecDeque;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use uuid::Uuid;

use crate::config::{self, Session};
use crate::subagents;

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
    let mut stream = UnixStream::connect(&path)
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
    let Ok(mut stream) = UnixStream::connect(path).await else {
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
    let state_dir = path.parent().context("session socket has no parent")?;
    tokio::fs::create_dir_all(state_dir).await?;
    tokio::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700)).await?;
    remove_stale_socket(&path).await?;

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("failed to listen at {}", path.display()))?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    eprintln!(
        "local-mcp session: {}\ncwd: {}\n\
         Give this session ID to the agent so it can include it in local-mcp tool calls.\n\
         Commands: /permission help, /provider help\n\
         Press Ctrl-C to stop.",
        session.id,
        session.cwd.display()
    );

    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut pending = VecDeque::<(Request, UnixStream)>::new();
    let mut yolo = false;
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let (mut stream, _) = connection?;
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
                if let Err(error) = handle_input(line.trim(), &mut session, &mut yolo, &mut pending).await {
                    eprintln!("Command failed: {error:#}");
                }
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

async fn remove_stale_socket(path: &PathBuf) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to remove stale session socket"),
    }
}

async fn handle_input(
    input: &str,
    session: &mut Session,
    yolo: &mut bool,
    pending: &mut VecDeque<(Request, UnixStream)>,
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
        command if provider_args(command, "add").is_some() => {
            let values = provider_args(command, "add").unwrap();
            anyhow::ensure!(
                (2..=3).contains(&values.len()),
                "usage: /provider add <name> <opencode|codex|claude|gemini> [model]"
            );
            let name = values[0];
            let driver = values[1];
            config::validate_provider_name(name)?;
            anyhow::ensure!(
                matches!(driver, "opencode" | "codex" | "claude" | "gemini"),
                "unsupported provider driver: {driver}"
            );
            let provider = config::SubagentProvider {
                driver: driver.to_owned(),
                model: values.get(2).map(|value| (*value).to_owned()),
            };
            session.subagent_providers.insert(name.to_owned(), provider);
            if session.default_subagent_provider.is_none() {
                session.default_subagent_provider = Some(name.to_owned());
            }
            config::save_session(session).await?;
            eprintln!("Configured provider: {name} ({driver})");
        }
        command if provider_args(command, "remove").is_some() => {
            let values = provider_args(command, "remove").unwrap();
            anyhow::ensure!(values.len() == 1, "usage: /provider remove <name>");
            let name = values[0];
            anyhow::ensure!(
                session.subagent_providers.remove(name).is_some(),
                "unknown provider: {name}"
            );
            if session.default_subagent_provider.as_deref() == Some(name) {
                session.default_subagent_provider =
                    session.subagent_providers.keys().next().cloned();
            }
            config::save_session(session).await?;
            eprintln!("Removed provider: {name}");
        }
        command if provider_args(command, "default").is_some() => {
            let values = provider_args(command, "default").unwrap();
            anyhow::ensure!(values.len() == 1, "usage: /provider default <name>");
            let name = values[0];
            anyhow::ensure!(
                session.subagent_providers.contains_key(name),
                "unknown provider: {name}"
            );
            session.default_subagent_provider = Some(name.to_owned());
            config::save_session(session).await?;
            eprintln!("Default provider: {name}");
        }
        "/provider list" | "/providers list" | "/provider status" | "/providers status" => {
            show_providers(session)
        }
        "/provider" | "/providers" | "/provider help" | "/providers help" => {
            eprintln!("/provider add <name> <opencode|codex|claude|gemini> [model]");
            eprintln!("/provider default <name> | remove <name> | list");
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

fn provider_args<'a>(command: &'a str, action: &str) -> Option<Vec<&'a str>> {
    ["/provider", "/providers"].into_iter().find_map(|prefix| {
        command
            .strip_prefix(&format!("{prefix} {action} "))
            .map(str::split_whitespace)
            .map(Iterator::collect)
    })
}

fn show_permissions(session: &Session) {
    eprintln!("Sandbox roots:");
    for path in &session.permitted_directories {
        eprintln!("  {}", path.display());
    }
}

fn show_providers(session: &Session) {
    eprintln!("Subagent providers:");
    if session.subagent_providers.is_empty() {
        eprintln!("  (none; use /provider add)");
    }
    for (name, provider) in &session.subagent_providers {
        let marker = if session.default_subagent_provider.as_deref() == Some(name) {
            " (default)"
        } else {
            ""
        };
        let model = provider.model.as_deref().unwrap_or("provider default");
        let availability = if subagents::driver_available(&provider.driver) {
            "installed"
        } else {
            "missing"
        };
        eprintln!(
            "  {name}: {} / {model} [{availability}]{marker}",
            provider.driver
        );
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

fn show_next(pending: &VecDeque<(Request, UnixStream)>) -> Result<()> {
    if let Some((request, _)) = pending.front() {
        show_request(request)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_management_commands() {
        assert_eq!(
            provider_args("/provider add fast opencode openai/gpt", "add"),
            Some(vec!["fast", "opencode", "openai/gpt"])
        );
        assert_eq!(
            provider_args("/providers default fast", "default"),
            Some(vec!["fast"])
        );
        assert_eq!(provider_args("/permission list", "add"), None);
    }
}
