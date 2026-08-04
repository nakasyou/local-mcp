use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::config::{Session, SubagentProvider};
use crate::{approvals, sandbox};

const DEFAULT_WAIT_MS: u64 = 10_000;
const MAX_WAIT_MS: u64 = 3_600_000;
const MAX_OPEN_AGENTS: usize = 4;

struct Invocation {
    command: Vec<String>,
    environment: Vec<(String, String)>,
}

struct Agent {
    session_id: String,
    task_name: String,
    provider_name: String,
    provider: SubagentProvider,
    cwd: PathBuf,
    messages: Vec<(String, String)>,
    pending: VecDeque<String>,
    trigger_pending: bool,
    handle: Option<JoinHandle<Result<String>>>,
    completion: Option<std::result::Result<String, String>>,
    closed: bool,
}

fn agents() -> &'static Mutex<HashMap<String, Agent>> {
    static AGENTS: OnceLock<Mutex<HashMap<String, Agent>>> = OnceLock::new();
    AGENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(session_id: &str, task_name: &str) -> String {
    format!("{session_id}/{task_name}")
}

pub async fn spawn(session: &Session, args: &Value) -> Result<Value> {
    let task_name = required_string(args, "task_name")?;
    validate_task_name(task_name)?;
    let message = required_message(args)?;
    let (provider_name, provider) =
        select_provider(session, args.get("agent_type").and_then(Value::as_str))?;
    let cwd = resolve_cwd(args, &session.cwd)?;
    let invocation = invocation(&provider, message)?;
    anyhow::ensure!(
        driver_available(&invocation.command[0]),
        "{} CLI is not installed or is not in PATH",
        provider.driver
    );
    {
        let agents = agents().lock().unwrap();
        anyhow::ensure!(
            !agents.contains_key(&key(&session.id, task_name)),
            "agent task already exists: {task_name}"
        );
        let open = agents
            .values()
            .filter(|agent| agent.session_id == session.id && !agent.closed)
            .count();
        anyhow::ensure!(
            open < MAX_OPEN_AGENTS,
            "maximum of {MAX_OPEN_AGENTS} open agents reached"
        );
    }
    let model = provider.model.as_deref().unwrap_or("provider default");
    if !approvals::request(
        &session.id,
        "spawn_agent",
        format!(
            "task: {task_name}\nprovider: {provider_name} ({})\nmodel: {model}\nmode: read-only/plan\nmessage:\n{message}",
            provider.driver
        ),
        cwd.clone(),
    )
    .await?
    {
        anyhow::bail!("user denied subagent spawn")
    }

    let handle = spawn_turn(&provider_name, &provider, message, &cwd)?;
    agents().lock().unwrap().insert(
        key(&session.id, task_name),
        Agent {
            session_id: session.id.clone(),
            task_name: task_name.to_owned(),
            provider_name: provider_name.to_owned(),
            provider,
            cwd,
            messages: vec![("user".to_owned(), message.to_owned())],
            pending: VecDeque::new(),
            trigger_pending: false,
            handle: Some(handle),
            completion: None,
            closed: false,
        },
    );
    approvals::activity(
        &session.id,
        format!("Spawned {task_name} via {provider_name}"),
        None,
    )
    .await;
    Ok(json!({"task_name": task_name, "nickname": provider_name}))
}

pub async fn send_message(session: &Session, args: &Value, trigger_turn: bool) -> Result<Value> {
    let target = required_string(args, "target")?;
    let message = required_message(args)?;
    collect_finished(&session.id).await?;
    let mut agents = agents().lock().unwrap();
    let agent = agents
        .get_mut(&key(&session.id, target))
        .context("unknown agent target")?;
    anyhow::ensure!(!agent.closed, "agent is closed: {target}");
    agent.pending.push_back(message.to_owned());
    agent.trigger_pending |= trigger_turn;
    if trigger_turn && agent.handle.is_none() {
        start_pending_turn(agent)?;
    }
    Ok(json!({"target": target, "queued": true, "triggered": trigger_turn}))
}

pub async fn wait(session: &Session, args: &Value) -> Result<Value> {
    let timeout_ms = args
        .get("timeout_ms")
        .map(|value| {
            value
                .as_u64()
                .context("timeout_ms must be a positive integer")
        })
        .transpose()?
        .unwrap_or(DEFAULT_WAIT_MS);
    anyhow::ensure!(timeout_ms >= 10_000, "timeout_ms must be at least 10000");
    anyhow::ensure!(
        timeout_ms <= MAX_WAIT_MS,
        "timeout_ms must be at most {MAX_WAIT_MS}"
    );
    collect_finished(&session.id).await?;
    if !has_live_agents(&session.id) {
        return Ok(json!({"message":"No live agents.","timed_out":false}));
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let updated = collect_finished(&session.id).await?;
        if !updated.is_empty() {
            return Ok(json!({
                "message": format!("Agent updates available: {}", updated.join(", ")),
                "timed_out": false
            }));
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(json!({"message":"Wait timed out.","timed_out":true}));
        }
        tokio::time::sleep(Duration::from_millis(50).min(deadline - now)).await;
    }
}

pub async fn list(session: &Session) -> Result<Value> {
    collect_finished(&session.id).await?;
    let agents = agents().lock().unwrap();
    let values = agents
        .values()
        .filter(|agent| agent.session_id == session.id && !agent.closed)
        .map(|agent| {
            json!({
                "agent_name": agent.task_name,
                "agent_type": agent.provider_name,
                "agent_status": status(agent),
                "last_task_message": agent.messages.iter().rev().find(|(role, _)| role == "user").map(|(_, text)| text)
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({"agents": values}))
}

pub async fn close(session: &Session, args: &Value) -> Result<Value> {
    let target = required_string(args, "target")?;
    collect_finished(&session.id).await?;
    let mut agents = agents().lock().unwrap();
    let agent = agents
        .get_mut(&key(&session.id, target))
        .context("unknown agent target")?;
    let previous_status = status(agent);
    if let Some(handle) = agent.handle.take() {
        handle.abort();
    }
    agent.closed = true;
    agent.pending.clear();
    Ok(json!({"previous_status": previous_status}))
}

fn has_live_agents(session_id: &str) -> bool {
    agents()
        .lock()
        .unwrap()
        .values()
        .any(|agent| agent.session_id == session_id && !agent.closed)
}

async fn collect_finished(session_id: &str) -> Result<Vec<String>> {
    let ready = {
        let mut agents = agents().lock().unwrap();
        agents
            .iter_mut()
            .filter(|(_, agent)| agent.session_id == session_id && !agent.closed)
            .filter_map(|(key, agent)| {
                agent
                    .handle
                    .as_ref()
                    .is_some_and(JoinHandle::is_finished)
                    .then(|| (key.clone(), agent.handle.take().unwrap()))
            })
            .collect::<Vec<_>>()
    };
    let mut updated = Vec::new();
    for (agent_key, handle) in ready {
        let result = handle
            .await
            .context("subagent task failed")?
            .map_err(|error| format!("{error:#}"));
        let mut agents = agents().lock().unwrap();
        let Some(agent) = agents.get_mut(&agent_key) else {
            continue;
        };
        match &result {
            Ok(response) => agent
                .messages
                .push(("assistant".to_owned(), response.clone())),
            Err(error) => agent
                .messages
                .push(("assistant".to_owned(), format!("Error: {error}"))),
        }
        agent.completion = Some(result);
        updated.push(agent.task_name.clone());
        if agent.trigger_pending {
            start_pending_turn(agent)?;
        }
    }
    Ok(updated)
}

fn start_pending_turn(agent: &mut Agent) -> Result<()> {
    let message = agent.pending.drain(..).collect::<Vec<_>>().join("\n\n");
    anyhow::ensure!(!message.is_empty(), "no queued message for agent");
    let prompt = continuation_prompt(agent, &message);
    agent.handle = Some(spawn_turn(
        &agent.provider_name,
        &agent.provider,
        &prompt,
        &agent.cwd,
    )?);
    agent.messages.push(("user".to_owned(), message));
    agent.completion = None;
    agent.trigger_pending = false;
    Ok(())
}

fn continuation_prompt(agent: &Agent, message: &str) -> String {
    let previous = agent
        .messages
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|(role, text)| {
            let text = text.chars().take(12_000).collect::<String>();
            format!("{role}: {text}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "Continue the same delegated task using the prior context below. Remain in read-only analysis mode.\n\n{previous}\n\nuser: {message}"
    )
}

fn status(agent: &Agent) -> Value {
    if agent.closed {
        Value::String("shutdown".to_owned())
    } else if agent.handle.is_some() {
        Value::String("running".to_owned())
    } else if let Some(completion) = &agent.completion {
        match completion {
            Ok(response) => json!({"completed": response}),
            Err(error) => json!({"errored": error}),
        }
    } else {
        Value::String("interrupted".to_owned())
    }
}

fn spawn_turn(
    provider_name: &str,
    provider: &SubagentProvider,
    prompt: &str,
    cwd: &Path,
) -> Result<JoinHandle<Result<String>>> {
    let invocation = invocation(provider, prompt)?;
    let provider_name = provider_name.to_owned();
    let model = provider.model.clone();
    let cwd = cwd.to_owned();
    Ok(tokio::spawn(async move {
        sandbox::run_unrestricted_with_env(&invocation.command, &cwd, None, &invocation.environment)
            .await
            .and_then(|output| render_output(&provider_name, model.as_deref(), output))
    }))
}

fn select_provider(
    session: &Session,
    requested: Option<&str>,
) -> Result<(String, SubagentProvider)> {
    let name = requested
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| session.default_subagent_provider.clone())
        .context("no agent_type supplied and no default provider configured; use /provider add")?;
    let provider = session
        .subagent_providers
        .get(&name)
        .with_context(|| format!("unknown configured provider: {name}"))?
        .clone();
    Ok((name, provider))
}

fn invocation(provider: &SubagentProvider, prompt: &str) -> Result<Invocation> {
    anyhow::ensure!(!prompt.trim().is_empty(), "message must not be empty");
    anyhow::ensure!(
        prompt.len() <= 65_536,
        "message must be at most 65536 bytes"
    );
    let mut environment = Vec::new();
    let mut command = match provider.driver.as_str() {
        "opencode" => vec![
            "opencode", "run", "--pure", "--agent", "plan", "--format", "default",
        ],
        "codex" => vec![
            "codex",
            "exec",
            "--ephemeral",
            "--ignore-user-config",
            "--sandbox",
            "read-only",
            "--color",
            "never",
        ],
        "claude" => vec![
            "claude",
            "--print",
            "--safe-mode",
            "--strict-mcp-config",
            "--tools",
            "Read,Glob,Grep",
            "--permission-mode",
            "plan",
            "--output-format",
            "text",
        ],
        "gemini" => vec![
            "gemini",
            "--approval-mode",
            "plan",
            "--output-format",
            "text",
        ],
        driver => anyhow::bail!("unsupported provider driver: {driver}"),
    }
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    if provider.driver == "opencode" {
        environment.push((
            "OPENCODE_CONFIG_CONTENT".to_owned(),
            json!({"agent":{"plan":{"permission":{"*":"deny","read":"allow","glob":"allow","grep":"allow","list":"allow","lsp":"allow","webfetch":"allow","websearch":"allow","edit":"deny","bash":"deny","task":"deny","external_directory":"deny","question":"deny"}}}}).to_string(),
        ));
    }
    if let Some(model) = &provider.model {
        command.extend(["--model".to_owned(), model.clone()]);
    }
    if provider.driver == "gemini" {
        command.push("--prompt".to_owned());
    }
    command.push(prompt.to_owned());
    Ok(Invocation {
        command,
        environment,
    })
}

fn render_output(provider: &str, model: Option<&str>, output: sandbox::Output) -> Result<String> {
    if output.status == 0 {
        Ok(output.stdout.trim_end().to_owned())
    } else {
        anyhow::bail!(
            json!({
                "provider": provider,
                "model": model,
                "stdout": output.stdout.trim_end(),
                "stderr": output.stderr.trim_end(),
                "exit_code": output.status
            })
            .to_string()
        )
    }
}

pub fn driver_available(executable: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(executable).is_file())
    })
}

fn required_string<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing {name}"))
}

fn required_message(args: &Value) -> Result<&str> {
    let message = required_string(args, "message")?;
    anyhow::ensure!(!message.trim().is_empty(), "message must not be empty");
    Ok(message)
}

fn validate_task_name(name: &str) -> Result<()> {
    anyhow::ensure!(!name.is_empty(), "task_name must not be empty");
    anyhow::ensure!(name.len() <= 64, "task_name must be at most 64 bytes");
    anyhow::ensure!(
        name.chars().all(|character| character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '_'),
        "task_name may contain only lowercase ASCII letters, digits, and underscores"
    );
    Ok(())
}

fn resolve_cwd(args: &Value, session_cwd: &Path) -> Result<PathBuf> {
    let path = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                session_cwd.join(path)
            }
        })
        .unwrap_or_else(|| session_cwd.to_owned());
    std::fs::canonicalize(&path).with_context(|| format!("cannot resolve cwd {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn session() -> Session {
        Session {
            id: "test".to_owned(),
            cwd: std::env::current_dir().unwrap(),
            permitted_directories: Vec::new(),
            subagent_providers: BTreeMap::from([(
                "fast".to_owned(),
                SubagentProvider {
                    driver: "opencode".to_owned(),
                    model: Some("openai/test".to_owned()),
                },
            )]),
            default_subagent_provider: Some("fast".to_owned()),
        }
    }

    #[test]
    fn resolves_configured_provider_and_builds_safe_command() {
        let (_, provider) = select_provider(&session(), None).unwrap();
        let invocation = invocation(&provider, "review this repository").unwrap();
        assert!(invocation.command.starts_with(&[
            "opencode".to_owned(),
            "run".to_owned(),
            "--pure".to_owned()
        ]));
        assert!(
            invocation
                .command
                .windows(2)
                .any(|items| items == ["--model", "openai/test"])
        );
        let config: Value = serde_json::from_str(&invocation.environment[0].1).unwrap();
        assert_eq!(config["agent"]["plan"]["permission"]["*"], "deny");
    }

    #[test]
    fn validates_codex_style_task_names() {
        assert!(validate_task_name("security_review_1").is_ok());
        assert!(validate_task_name("HasCaps").is_err());
        assert!(validate_task_name("has-hyphen").is_err());
    }

    #[test]
    fn requires_configured_provider() {
        let mut session = session();
        session.default_subagent_provider = None;
        assert!(select_provider(&session, None).is_err());
        assert!(select_provider(&session, Some("missing")).is_err());
    }
}
