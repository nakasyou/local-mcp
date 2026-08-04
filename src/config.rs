use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub permitted_directories: Vec<PathBuf>,
    #[serde(default)]
    pub subagent_providers: BTreeMap<String, SubagentProvider>,
    #[serde(default)]
    pub default_subagent_provider: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SubagentProvider {
    pub driver: String,
    #[serde(default)]
    pub model: Option<String>,
}

pub fn state_dir() -> Result<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|path| path.join("local-mcp"))
        .context("could not determine a local state directory")
}

pub fn session_path(id: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    Ok(state_dir()?.join("sessions").join(format!("{id}.json")))
}

pub fn socket_path(id: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    Ok(socket_dir().join(format!("{id}.sock")))
}

/// Returns a short, per-user directory for Unix-domain session sockets.
///
/// Socket paths have a platform-specific length limit (104 bytes on macOS),
/// so they cannot live below the regular state directory, which may include
/// a long home-directory path. Session metadata remains in `state_dir()`.
fn socket_dir() -> PathBuf {
    // `TMPDIR` on macOS can itself be long, so use the conventional short
    // system temporary directory rather than `std::env::temp_dir()`.
    let uid = unsafe { libc::geteuid() };
    PathBuf::from("/tmp").join(format!("local-mcp-{uid}"))
}

pub async fn create_session(cwd: &Path, id: Option<&str>) -> Result<Session> {
    let cwd = canonical_directory(cwd)?;
    let id = id
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    validate_session_id(&id)?;
    let previous = if tokio::fs::try_exists(session_path(&id)?).await? {
        Some(load_session(&id).await?)
    } else {
        None
    };
    let session = Session {
        id,
        cwd: cwd.clone(),
        permitted_directories: vec![cwd],
        subagent_providers: previous
            .as_ref()
            .map(|session| session.subagent_providers.clone())
            .unwrap_or_default(),
        default_subagent_provider: previous.and_then(|session| session.default_subagent_provider),
    };
    save_session(&session).await?;
    Ok(session)
}

pub async fn load_session(id: &str) -> Result<Session> {
    let path = session_path(id)?;
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("session {id} was not found; run `local-mcp start` first"))?;
    serde_json::from_slice(&bytes).context("invalid local-mcp session")
}

pub async fn save_session(session: &Session) -> Result<()> {
    let path = session_path(&session.id)?;
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(session)?).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

pub fn validate_session_id(id: &str) -> Result<()> {
    anyhow::ensure!(!id.is_empty(), "session ID must not be empty");
    anyhow::ensure!(id.len() <= 64, "session ID must be at most 64 bytes");
    anyhow::ensure!(
        id.chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character)),
        "session ID may contain only ASCII letters, numbers, '-', '_', and '.'"
    );
    anyhow::ensure!(id != "." && id != "..", "invalid session ID");
    Ok(())
}

pub fn validate_provider_name(name: &str) -> Result<()> {
    anyhow::ensure!(!name.is_empty(), "provider name must not be empty");
    anyhow::ensure!(name.len() <= 64, "provider name must be at most 64 bytes");
    anyhow::ensure!(
        name.chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character)),
        "provider name may contain only ASCII letters, numbers, '-' and '_'"
    );
    Ok(())
}

pub fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    anyhow::ensure!(path.is_dir(), "{} is not a directory", path.display());
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_custom_session_ids() {
        assert!(validate_session_id("my-project_1.dev").is_ok());
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("../escape").is_err());
        assert!(validate_session_id("contains spaces").is_err());
        assert!(validate_session_id(&"x".repeat(65)).is_err());
    }

    #[test]
    fn validates_provider_names() {
        assert!(validate_provider_name("fast_opencode").is_ok());
        assert!(validate_provider_name("claude-review").is_ok());
        assert!(validate_provider_name("has spaces").is_err());
        assert!(validate_provider_name("../escape").is_err());
    }

    #[test]
    fn loads_sessions_created_before_provider_configuration() {
        let session: Session = serde_json::from_value(serde_json::json!({
            "id": "legacy",
            "cwd": "/tmp",
            "permitted_directories": ["/tmp"]
        }))
        .unwrap();

        assert!(session.subagent_providers.is_empty());
        assert!(session.default_subagent_provider.is_none());
    }

    #[test]
    fn puts_sockets_in_a_short_per_user_directory() {
        let path = socket_path("7418eda5-fd07-4e00-ace5-c1ece2f68a02").unwrap();
        assert_eq!(path.parent(), Some(socket_dir().as_path()));
        assert!(path.as_os_str().len() < 104);
    }
}
