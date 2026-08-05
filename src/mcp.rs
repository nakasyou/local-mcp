use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, extract::State};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::Client;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use url::Url;
use uuid::Uuid;

use crate::{approvals, config, sandbox};

const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(30);
const PROTOCOL_VERSION: &str = "2025-06-18";

struct Job {
    session_id: String,
    command: String,
    handle: JoinHandle<Result<String>>,
}

fn jobs() -> &'static Mutex<HashMap<Uuid, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<Uuid, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn serve() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                write_message(&mut stdout, &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}})).await?;
                continue;
            }
        };
        if let Some(response) = handle_request(&request).await {
            write_message(&mut stdout, &response).await?;
        }
    }
    Ok(())
}

#[derive(Clone)]
struct HttpState {
    auth: HttpAuth,
}

#[derive(Clone)]
enum HttpAuth {
    Disabled,
    Static(Arc<str>),
    OAuth(Arc<OAuthVerifier>),
}

pub struct OAuthConfig {
    pub issuer: Url,
    pub resource: Url,
    pub introspection_endpoint: Url,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

struct OAuthVerifier {
    config: OAuthConfig,
    resource_metadata: Url,
    client: Client,
}

pub async fn serve_http(
    bind: SocketAddr,
    bearer_token: Option<String>,
    oauth: Option<OAuthConfig>,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind HTTP server to {bind}"))?;
    let auth = match (bearer_token, oauth) {
        (Some(token), None) => HttpAuth::Static(Arc::from(token)),
        (None, Some(config)) => {
            validate_oauth_config(&config)?;
            let resource_metadata = protected_resource_metadata_url(&config.resource)?;
            let client = Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .context("failed to build OAuth introspection client")?;
            HttpAuth::OAuth(Arc::new(OAuthVerifier {
                config,
                resource_metadata,
                client,
            }))
        }
        (None, None) => HttpAuth::Disabled,
        (Some(_), Some(_)) => unreachable!("authentication modes conflict in clap"),
    };
    let state = HttpState { auth };
    let authentication = match &state.auth {
        HttpAuth::Disabled => "authentication disabled",
        HttpAuth::Static(_) => "static Bearer authentication required",
        HttpAuth::OAuth(_) => "OAuth 2.0 authentication required",
    };
    eprintln!("local-mcp HTTP server listening on http://{bind}/mcp ({authentication})");
    axum::serve(listener, http_router(state))
        .await
        .context("HTTP server failed")
}

fn http_router(state: HttpState) -> Router {
    Router::new()
        .route("/mcp", post(http_post).get(http_get).delete(http_get))
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth_protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/{*resource_path}",
            get(oauth_protected_resource),
        )
        .with_state(state)
}

async fn http_get(State(state): State<HttpState>, headers: HeaderMap) -> Response {
    if !authenticate(&headers, &state.auth).await {
        return unauthorized_response(&state.auth);
    }
    if !has_valid_origin(&headers) {
        return (StatusCode::FORBIDDEN, "Origin is not allowed").into_response();
    }
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

async fn http_post(State(state): State<HttpState>, headers: HeaderMap, body: Bytes) -> Response {
    if !authenticate(&headers, &state.auth).await {
        return unauthorized_response(&state.auth);
    }
    if !has_valid_origin(&headers) {
        return (StatusCode::FORBIDDEN, "Origin is not allowed").into_response();
    }
    if !is_json_content_type(&headers) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json",
        )
            .into_response();
    }
    if !accepts_json(&headers) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "Accept must include application/json and text/event-stream",
        )
            .into_response();
    }

    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return json_response(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": error.to_string()}
            }));
        }
    };
    if !has_supported_protocol_version(&headers, &request) {
        return (
            StatusCode::BAD_REQUEST,
            format!("unsupported MCP-Protocol-Version; expected {PROTOCOL_VERSION}"),
        )
            .into_response();
    }
    match handle_request(&request).await {
        Some(response) => json_response(response),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let (scheme, token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))?;
    scheme.eq_ignore_ascii_case("Bearer").then_some(token)
}

async fn authenticate(headers: &HeaderMap, auth: &HttpAuth) -> bool {
    match auth {
        HttpAuth::Disabled => true,
        HttpAuth::Static(expected) => bearer_token(headers)
            .is_some_and(|token| bool::from(token.as_bytes().ct_eq(expected.as_bytes()))),
        HttpAuth::OAuth(verifier) => {
            let Some(token) = bearer_token(headers) else {
                return false;
            };
            match verifier.introspect(token).await {
                Ok(valid) => valid,
                Err(error) => {
                    eprintln!("OAuth token introspection failed: {error:#}");
                    false
                }
            }
        }
    }
}

impl OAuthVerifier {
    async fn introspect(&self, token: &str) -> Result<bool> {
        let mut request = self
            .client
            .post(self.config.introspection_endpoint.clone())
            .form(&[("token", token), ("token_type_hint", "access_token")]);
        if let Some(client_id) = self.config.client_id.as_deref() {
            request = request.basic_auth(client_id, self.config.client_secret.as_deref());
        }
        let response = request
            .send()
            .await
            .context("introspection request failed")?
            .error_for_status()
            .context("introspection endpoint returned an error")?;
        let body: Value = response
            .json()
            .await
            .context("invalid introspection response")?;
        Ok(is_active_for_resource(
            &body,
            &self.config.issuer,
            &self.config.resource,
        ))
    }
}

fn is_active_for_resource(body: &Value, issuer: &Url, resource: &Url) -> bool {
    if body.get("active").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    if let Some(actual_issuer) = body.get("iss").and_then(Value::as_str)
        && actual_issuer != issuer.as_str()
    {
        return false;
    }
    match body.get("aud") {
        Some(Value::String(audience)) => audience == resource.as_str(),
        Some(Value::Array(audiences)) => audiences
            .iter()
            .filter_map(Value::as_str)
            .any(|audience| audience == resource.as_str()),
        _ => false,
    }
}

fn validate_oauth_config(config: &OAuthConfig) -> Result<()> {
    for (name, url) in [
        ("OAuth issuer", &config.issuer),
        ("OAuth resource", &config.resource),
        (
            "OAuth introspection endpoint",
            &config.introspection_endpoint,
        ),
    ] {
        ensure_http_url(name, url)?;
    }
    anyhow::ensure!(
        config.client_secret.is_none() || config.client_id.is_some(),
        "OAuth client secret requires a client ID"
    );
    Ok(())
}

fn ensure_http_url(name: &str, url: &Url) -> Result<()> {
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.host().is_some(),
        "{name} must be an absolute HTTP(S) URL"
    );
    let is_loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost") || matches!(host, "127.0.0.1" | "::1" | "[::1]")
    });
    anyhow::ensure!(
        url.scheme() == "https" || is_loopback,
        "{name} must use HTTPS unless it has a loopback host"
    );
    Ok(())
}

fn protected_resource_metadata_url(resource: &Url) -> Result<Url> {
    let mut metadata = resource.clone();
    metadata.set_query(None);
    metadata.set_fragment(None);
    let resource_path = resource.path().trim_start_matches('/');
    let path = if resource_path.is_empty() {
        "/.well-known/oauth-protected-resource".to_owned()
    } else {
        format!("/.well-known/oauth-protected-resource/{resource_path}")
    };
    metadata.set_path(&path);
    Ok(metadata)
}

async fn oauth_protected_resource(State(state): State<HttpState>) -> Response {
    let HttpAuth::OAuth(verifier) = &state.auth else {
        return StatusCode::NOT_FOUND.into_response();
    };
    json_response(json!({
        "resource": verifier.config.resource,
        "authorization_servers": [verifier.config.issuer],
        "bearer_methods_supported": ["header"]
    }))
}

fn unauthorized_response(auth: &HttpAuth) -> Response {
    let challenge = match auth {
        HttpAuth::OAuth(verifier) => format!(
            "Bearer resource_metadata=\"{}\"",
            verifier.resource_metadata
        ),
        _ => "Bearer".to_owned(),
    };
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
        "Bearer token is missing or invalid",
    )
        .into_response()
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn has_valid_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    origin
        .to_str()
        .ok()
        .and_then(|value| value.parse::<Uri>().ok())
        .and_then(|uri| uri.host().map(str::to_owned))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || matches!(host.as_str(), "127.0.0.1" | "::1" | "[::1]")
        })
}

fn has_supported_protocol_version(headers: &HeaderMap, request: &Value) -> bool {
    if request.get("method").and_then(Value::as_str) == Some("initialize") {
        return true;
    }
    headers
        .get("mcp-protocol-version")
        .map(|value| value.as_bytes() == PROTOCOL_VERSION.as_bytes())
        .unwrap_or(true)
}

fn accepts_json(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let accepts = |expected: &str| {
        value.split(',').any(|item| {
            item.trim()
                .split(';')
                .next()
                .is_some_and(|mime| mime.eq_ignore_ascii_case(expected) || mime == "*/*")
        })
    };
    accepts("application/json") && accepts("text/event-stream")
}

fn json_response(value: Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

async fn handle_request(request: &Value) -> Option<Value> {
    let id = request.get("id")?.clone();
    Some(match dispatch(request).await {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(error) => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":format!("{error:#}")}})
        }
    })
}

async fn write_message(stdout: &mut tokio::io::Stdout, message: &Value) -> Result<()> {
    stdout
        .write_all(serde_json::to_string(message)?.as_bytes())
        .await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

async fn dispatch(request: &Value) -> Result<Value> {
    match request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "local-mcp", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Every tool call requires the local-mcp session_id supplied by the user. Call session_info with that ID to inspect its working directory and sandbox roots."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call_tool(request.get("params").unwrap_or(&Value::Null)).await,
        method => anyhow::bail!("method not found: {method}"),
    }
}

fn tools() -> Value {
    #[cfg(not(windows))]
    let write_file_description = "Write a UTF-8 file in the Codex sandbox. Relative paths use the session working directory.";
    #[cfg(windows)]
    let write_file_description = "Write a UTF-8 file directly on the Windows host without a Codex sandbox. Relative paths use the session working directory.";
    #[cfg(not(windows))]
    let execute_description = "Execute argv without a shell in the Codex sandbox. Returns the normal result when it finishes within 30 seconds; otherwise returns a job_id for use with poll_job or stop_job. Network is disabled and approval is not required.";
    #[cfg(windows)]
    let execute_description = "Execute argv without a shell directly on the Windows host. Returns the normal result when it finishes within 30 seconds; otherwise returns a job_id for use with poll_job or stop_job. This has the user's filesystem and network access and requires approval unless the session is in yolo mode.";
    #[cfg(not(windows))]
    let start_command_description = "Start argv immediately as a background job in the Codex sandbox and return a job_id without waiting for completion. Network is disabled and approval is not required.";
    #[cfg(windows)]
    let start_command_description = "Start argv immediately as a background job directly on the Windows host and return a job_id without waiting for completion. This has the user's filesystem and network access and requires approval unless the session is in yolo mode.";

    let mut tools = json!([
        {"name":"session_info","description":"Show a local-mcp session's ID, working directory, and allowed sandbox roots.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"read_file","description":"Read a UTF-8 file from the local machine. Relative paths use the session working directory.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"path":{"type":"string"}},"required":["session_id","path"]}},
        {"name":"get_image","description":"Read a local image and return it as MCP image content. Relative paths use the session working directory.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"path":{"type":"string","description":"Path to a PNG, JPEG, GIF, WebP, BMP, TIFF, or AVIF image."}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"list_directory","description":"List entries in a local directory. Relative paths use the session working directory.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"path":{"type":"string"}},"required":["session_id","path"]}},
        {"name":"write_file","description":write_file_description,"inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"path":{"type":"string"},"content":{"type":"string"}},"required":["session_id","path","content"]}},
        {"name":"execute","description":execute_description,"inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"]}},
        {"name":"start_command","description":start_command_description,"inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"]}},
        {"name":"poll_job","description":"Poll a background command returned by execute or start_command. Returns running while active, or the command result once completed.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"job_id":{"type":"string","format":"uuid"}},"required":["session_id","job_id"],"additionalProperties":false}},
        {"name":"stop_job","description":"Stop a background command returned by execute or start_command.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"job_id":{"type":"string","format":"uuid"}},"required":["session_id","job_id"],"additionalProperties":false}},
        {"name":"without_sandbox","description":"Execute argv directly on the host with full user permissions and network access. Every call requires approval unless the session is in yolo mode.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"]}}
    ]);
    for tool in tools.as_array_mut().unwrap() {
        if let Some(session_id) = tool
            .pointer_mut("/inputSchema/properties/session_id")
            .and_then(Value::as_object_mut)
        {
            session_id.remove("format");
        }
    }
    tools
}

async fn call_tool(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("missing tool name")?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let session_id = required_session_id(&args)?;
    let session = config::load_session(&session_id).await?;
    match name {
        "session_info" => {
            approvals::activity(&session.id, "Read session info", None).await;
            text_result(serde_json::to_string_pretty(&session)?)
        }
        "get_image" => {
            let path = resolve_path(&session.cwd, required_path(&args, "path")?);
            let result = get_image(&path).await;
            report_result(
                &session.id,
                format!("Read image {}", display_path(&path, &session.cwd)),
                &result,
            )
            .await;
            result
        }
        "read_file" => {
            let path = resolve_path(&session.cwd, required_path(&args, "path")?);
            let result = tokio::fs::read_to_string(&path)
                .await
                .context("failed to read file");
            report_result(
                &session.id,
                format!("Read {}", display_path(&path, &session.cwd)),
                &result,
            )
            .await;
            text_result(result?)
        }
        "list_directory" => {
            let path = resolve_path(&session.cwd, required_path(&args, "path")?);
            let result = list_directory(&path).await;
            report_result(
                &session.id,
                format!("Listed {}", display_path(&path, &session.cwd)),
                &result,
            )
            .await;
            text_result(result?)
        }
        "write_file" => write_file(&args, &session).await,
        "execute" => execute(&args, &session).await,
        "start_command" => start_command(&args, &session).await,
        "poll_job" => poll_job(&args, &session).await,
        "stop_job" => stop_job(&args, &session).await,
        "without_sandbox" => without_sandbox(&args, &session).await,
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

async fn report_result<T>(session_id: &str, title: String, result: &Result<T>) {
    let detail = result
        .as_ref()
        .err()
        .map(|error| format!("└ Error: {error:#}"));
    approvals::activity(session_id, title, detail).await;
}

fn display_path<'a>(path: &'a Path, session_cwd: &Path) -> std::borrow::Cow<'a, str> {
    path.strip_prefix(session_cwd)
        .unwrap_or(path)
        .to_string_lossy()
}

fn text_result(text: String) -> Result<Value> {
    Ok(json!({"content":[{"type":"text","text":text}]}))
}

async fn get_image(path: &Path) -> Result<Value> {
    let path = tokio::fs::canonicalize(&path)
        .await
        .with_context(|| format!("cannot resolve image {}", path.display()))?;
    let metadata = tokio::fs::metadata(&path).await?;
    anyhow::ensure!(
        metadata.is_file(),
        "image path is not a file: {}",
        path.display()
    );
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("cannot read image {}", path.display()))?;
    let mime_type = image_mime_type(&bytes)
        .with_context(|| format!("unsupported image format: {}", path.display()))?;
    Ok(json!({
        "content": [{
            "type": "image",
            "data": STANDARD.encode(bytes),
            "mimeType": mime_type
        }]
    }))
}

fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Some("image/tiff")
    } else if bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(&bytes[8..12], b"avif" | b"avis")
    {
        Some("image/avif")
    } else {
        None
    }
}

fn required_path(args: &Value, name: &str) -> Result<PathBuf> {
    args.get(name)
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context(format!("missing {name}"))
}

fn required_session_id(args: &Value) -> Result<String> {
    let value = args
        .get("session_id")
        .and_then(Value::as_str)
        .context("missing session_id; ask the user to run `local-mcp start` and provide its ID")?;
    config::validate_session_id(value)?;
    Ok(value.to_owned())
}

fn resolve_path(session_cwd: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        session_cwd.join(path)
    }
}

fn cwd(args: &Value, session_cwd: &Path) -> Result<PathBuf> {
    let path = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .map(|path| resolve_path(session_cwd, path))
        .unwrap_or_else(|| session_cwd.to_owned());
    std::fs::canonicalize(&path).with_context(|| format!("cannot resolve cwd {}", path.display()))
}

async fn list_directory(path: &Path) -> Result<String> {
    let mut entries = tokio::fs::read_dir(path).await?;
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let suffix = if entry.file_type().await?.is_dir() {
            "/"
        } else {
            ""
        };
        names.push(format!("{}{}", entry.file_name().to_string_lossy(), suffix));
    }
    names.sort();
    Ok(names.join("\n"))
}

async fn write_file(args: &Value, session: &config::Session) -> Result<Value> {
    let absolute = resolve_path(&session.cwd, required_path(args, "path")?);
    let parent = absolute.parent().context("file has no parent directory")?;
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("parent does not exist: {}", parent.display()))?;
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .context("missing content")?;
    let previous = tokio::fs::read_to_string(&absolute)
        .await
        .unwrap_or_default();
    #[cfg(unix)]
    let output = {
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "cat > \"$1\"".to_owned(),
            "local-mcp-write".to_owned(),
            absolute.display().to_string(),
        ];
        sandbox::run(
            &command,
            &parent,
            std::slice::from_ref(&parent),
            Some(content.as_bytes()),
        )
        .await?
    };
    #[cfg(windows)]
    let output = {
        // Windows has no application sandbox here, so avoid depending on a
        // shell utility for the file-edit operation.
        tokio::fs::write(&absolute, content).await?;
        sandbox::Output {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    };
    let result = render_output(output);
    let (added, removed, diff) = render_diff(&previous, content);
    let title = format!(
        "Edited {} (+{added} -{removed})",
        display_path(&absolute, &session.cwd)
    );
    let detail = match &result {
        Ok(_) => (!diff.is_empty()).then_some(diff),
        Err(error) => Some(format!("└ Error: {error:#}")),
    };
    approvals::activity(&session.id, title, detail).await;
    text_result(result?)
}

async fn execute(args: &Value, session: &config::Session) -> Result<Value> {
    let (rendered_command, mut handle) = spawn_sandboxed_command("execute", args, session).await?;

    match tokio::time::timeout(FOREGROUND_TIMEOUT, &mut handle).await {
        Ok(joined) => text_result(joined.context("command task failed")??),
        Err(_) => store_job(session, rendered_command, handle, "Backgrounded").await,
    }
}

async fn start_command(args: &Value, session: &config::Session) -> Result<Value> {
    let (rendered_command, handle) =
        spawn_sandboxed_command("start_command", args, session).await?;
    store_job(session, rendered_command, handle, "Started").await
}

async fn spawn_sandboxed_command(
    operation: &str,
    args: &Value,
    session: &config::Session,
) -> Result<(String, JoinHandle<Result<String>>)> {
    let command = required_command(args)?;
    let cwd = cwd(args, &session.cwd)?;
    #[cfg(windows)]
    if !approvals::request(
        &session.id,
        operation,
        format!("argv: {command:?}"),
        cwd.clone(),
    )
    .await?
    {
        anyhow::bail!("user denied {operation}")
    }
    #[cfg(not(windows))]
    let _ = operation;
    let mut roots = session.permitted_directories.clone();
    if !roots.iter().any(|root| cwd.starts_with(root)) {
        roots.push(cwd.clone());
    }
    let rendered_command = render_command(&command);
    approvals::activity(&session.id, format!("Running {rendered_command}"), None).await;
    let session_id = session.id.clone();
    let task_command = rendered_command.clone();
    let handle = tokio::spawn(async move {
        let result = sandbox::run(&command, &cwd, &roots, None)
            .await
            .and_then(render_output);
        report_command_finished(session_id, &task_command, &result).await;
        result
    });
    Ok((rendered_command, handle))
}

async fn store_job(
    session: &config::Session,
    rendered_command: String,
    handle: JoinHandle<Result<String>>,
    activity: &str,
) -> Result<Value> {
    let job_id = Uuid::new_v4();
    jobs().lock().unwrap().insert(
        job_id,
        Job {
            session_id: session.id.clone(),
            command: rendered_command.clone(),
            handle,
        },
    );
    approvals::activity(
        &session.id,
        format!("{activity} {rendered_command}"),
        Some(format!("└ job {job_id}")),
    )
    .await;
    text_result(json!({"status":"running","job_id":job_id}).to_string())
}

async fn poll_job(args: &Value, session: &config::Session) -> Result<Value> {
    let job_id = required_job_id(args)?;
    let finished = {
        let jobs = jobs().lock().unwrap();
        let job = jobs.get(&job_id).context("unknown job_id")?;
        anyhow::ensure!(
            job.session_id == session.id,
            "job does not belong to this session"
        );
        job.handle.is_finished()
    };
    if !finished {
        return text_result(json!({"status":"running","job_id":job_id}).to_string());
    }

    let job = jobs().lock().unwrap().remove(&job_id).unwrap();
    let result = job.handle.await.context("background command task failed")?;
    text_result(result?)
}

async fn stop_job(args: &Value, session: &config::Session) -> Result<Value> {
    let job_id = required_job_id(args)?;
    let job = {
        let mut jobs = jobs().lock().unwrap();
        let job = jobs.get(&job_id).context("unknown job_id")?;
        anyhow::ensure!(
            job.session_id == session.id,
            "job does not belong to this session"
        );
        jobs.remove(&job_id).unwrap()
    };
    job.handle.abort();
    let _ = job.handle.await;
    approvals::activity(
        &session.id,
        format!("Stopped {}", job.command),
        Some(format!("└ job {job_id}")),
    )
    .await;
    text_result(json!({"status":"stopped","job_id":job_id}).to_string())
}

fn required_job_id(args: &Value) -> Result<Uuid> {
    let value = args
        .get("job_id")
        .and_then(Value::as_str)
        .context("missing job_id")?;
    Uuid::parse_str(value).context("invalid job_id")
}

fn required_command(args: &Value) -> Result<Vec<String>> {
    args.get("command")
        .and_then(Value::as_array)
        .context("missing command")?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .context("command entries must be strings")
        })
        .collect()
}

async fn without_sandbox(args: &Value, session: &config::Session) -> Result<Value> {
    let command = required_command(args)?;
    let cwd = cwd(args, &session.cwd)?;
    if !approvals::request(
        &session.id,
        "without_sandbox",
        format!("argv: {command:?}"),
        cwd.clone(),
    )
    .await?
    {
        anyhow::bail!("user denied without_sandbox")
    }
    run_and_report(session.id.clone(), command, cwd, true, &[]).await
}

async fn run_and_report(
    session_id: String,
    command: Vec<String>,
    cwd: PathBuf,
    unrestricted: bool,
    roots: &[PathBuf],
) -> Result<Value> {
    let rendered_command = render_command(&command);
    approvals::activity(&session_id, format!("Running {rendered_command}"), None).await;
    let output = if unrestricted {
        sandbox::run_unrestricted(&command, &cwd, None).await
    } else {
        sandbox::run(&command, &cwd, roots, None).await
    };
    let result = output.and_then(render_output);
    report_command_finished(session_id, &rendered_command, &result).await;
    text_result(result?)
}

fn render_command(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| shell_word(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn report_command_finished(session_id: String, command: &str, result: &Result<String>) {
    let detail = match result {
        Ok(text) => command_summary(text),
        Err(error) => Some(format!("└ Error: {error:#}")),
    };
    approvals::activity(&session_id, format!("Ran {command}"), detail).await;
}

fn shell_word(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=+".contains(c))
    {
        value.to_owned()
    } else {
        format!("{:?}", value)
    }
}

fn command_summary(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    let stdout = value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    let stderr = value
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    let output = if stdout.is_empty() { stderr } else { stdout };
    if output.is_empty() {
        None
    } else {
        Some(
            output
                .lines()
                .map(|line| format!("└ {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

fn render_diff(old: &str, new: &str) -> (usize, usize, String) {
    let diff = TextDiff::from_lines(old, new);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    let rendered = diff.unified_diff().context_radius(3).to_string();
    (added, removed, rendered.trim_end().to_owned())
}

fn render_output(output: sandbox::Output) -> Result<String> {
    let text = json!({"exit_code":output.status,"stdout":output.stdout,"stderr":output.stderr})
        .to_string();
    if output.status == 0 {
        Ok(text)
    } else {
        anyhow::bail!(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_state(bearer_token: Option<&str>) -> State<HttpState> {
        State(HttpState {
            auth: bearer_token
                .map(|token| HttpAuth::Static(Arc::from(token)))
                .unwrap_or(HttpAuth::Disabled),
        })
    }

    fn oauth_state(introspection_endpoint: Url) -> State<HttpState> {
        let config = OAuthConfig {
            issuer: Url::parse("https://auth.example.com").unwrap(),
            resource: Url::parse("https://mcp.example.com/mcp").unwrap(),
            introspection_endpoint,
            client_id: Some("local-mcp".to_owned()),
            client_secret: Some("introspection-secret".to_owned()),
        };
        State(HttpState {
            auth: HttpAuth::OAuth(Arc::new(OAuthVerifier {
                resource_metadata: protected_resource_metadata_url(&config.resource).unwrap(),
                config,
                client: Client::new(),
            })),
        })
    }

    fn http_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        headers.insert(
            header::ACCEPT,
            "application/json, text/event-stream".parse().unwrap(),
        );
        headers
    }

    #[test]
    fn detects_supported_image_types() {
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(image_mime_type(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(image_mime_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(image_mime_type(b"RIFF\0\0\0\0WEBP"), Some("image/webp"));
        assert_eq!(image_mime_type(b"not an image"), None);
    }

    #[test]
    fn renders_edit_counts_and_unified_diff() {
        let (added, removed, diff) = render_diff("one\ntwo\n", "one\nchanged\nthree\n");

        assert_eq!((added, removed), (2, 1));
        assert!(diff.contains("-two"));
        assert!(diff.contains("+changed"));
        assert!(diff.contains("+three"));
    }

    #[test]
    fn quotes_command_arguments_for_activity_display() {
        assert_eq!(shell_word("README.md"), "README.md");
        assert_eq!(shell_word("hello world"), "\"hello world\"");
    }

    #[cfg(windows)]
    #[test]
    fn describes_windows_command_execution_as_approved_host_access() {
        let tools = tools();
        for name in ["execute", "start_command"] {
            let description = tools
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap()["description"]
                .as_str()
                .unwrap();
            assert!(description.contains("Windows host"));
            assert!(description.contains("requires approval"));
            assert!(description.contains("filesystem and network access"));
        }

        let write_file_description = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "write_file")
            .unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(write_file_description.contains("Windows host"));
        assert!(write_file_description.contains("without a Codex sandbox"));
    }

    #[tokio::test]
    async fn get_image_returns_mcp_image_content() {
        let path = std::env::temp_dir().join(format!("local-mcp-{}.png", uuid::Uuid::new_v4()));
        let bytes = b"\x89PNG\r\n\x1a\nexample";
        tokio::fs::write(&path, bytes).await.unwrap();

        let result = get_image(&path).await.unwrap();
        tokio::fs::remove_file(path).await.unwrap();

        assert_eq!(result["content"][0]["type"], "image");
        assert_eq!(result["content"][0]["mimeType"], "image/png");
        assert_eq!(result["content"][0]["data"], STANDARD.encode(bytes));
    }

    #[tokio::test]
    async fn get_image_resolves_relative_paths_from_session_cwd() {
        let directory = std::env::temp_dir().join(format!("local-mcp-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir(&directory).await.unwrap();
        let path = directory.join("image.gif");
        tokio::fs::write(&path, b"GIF89a").await.unwrap();

        let result = get_image(&resolve_path(&directory, PathBuf::from("image.gif")))
            .await
            .unwrap();
        tokio::fs::remove_dir_all(directory).await.unwrap();

        assert_eq!(result["content"][0]["mimeType"], "image/gif");
    }

    #[tokio::test]
    async fn http_returns_json_rpc_response() {
        let response = http_post(
            http_state(None),
            http_headers(),
            Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["id"], 1);
        assert_eq!(body["result"]["serverInfo"]["name"], "local-mcp");
    }

    #[tokio::test]
    async fn http_accepts_notifications_without_a_response_body() {
        let response = http_post(
            http_state(None),
            http_headers(),
            Bytes::from_static(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        )
        .await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn http_rejects_invalid_headers() {
        let response = http_post(
            http_state(None),
            HeaderMap::new(),
            Bytes::from_static(b"{}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        let mut headers = http_headers();
        headers.insert(header::ORIGIN, "https://example.com".parse().unwrap());
        let response = http_post(http_state(None), headers, Bytes::from_static(b"{}")).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let mut headers = http_headers();
        headers.insert("mcp-protocol-version", "unsupported".parse().unwrap());
        let response = http_post(
            http_state(None),
            headers,
            Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn http_get_reports_that_sse_is_not_available() {
        let response = http_get(http_state(None), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://example.com".parse().unwrap());
        let response = http_get(http_state(None), headers).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn http_requires_configured_bearer_token() {
        let response = http_post(
            http_state(Some("secret-token")),
            http_headers(),
            Bytes::from_static(b"{}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");

        let mut headers = http_headers();
        headers.insert(header::AUTHORIZATION, "Bearer wrong-token".parse().unwrap());
        let response = http_post(
            http_state(Some("secret-token")),
            headers,
            Bytes::from_static(b"{}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mut headers = http_headers();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer secret-token".parse().unwrap(),
        );
        let response = http_post(
            http_state(Some("secret-token")),
            headers,
            Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn http_authenticates_get_requests_too() {
        let response = http_get(http_state(Some("secret-token")), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "bearer secret-token".parse().unwrap(),
        );
        let response = http_get(http_state(Some("secret-token")), headers).await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn oauth_publishes_discovery_and_challenges() {
        let state = oauth_state(Url::parse("https://auth.example.com/introspect").unwrap());
        let _router = http_router(state.0.clone());

        let metadata = oauth_protected_resource(state.clone()).await;
        assert_eq!(metadata.status(), StatusCode::OK);
        let body = axum::body::to_bytes(metadata.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["resource"], "https://mcp.example.com/mcp");
        assert_eq!(
            body["authorization_servers"][0],
            "https://auth.example.com/"
        );

        let response = http_post(state.clone(), http_headers(), Bytes::from_static(b"{}")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            "Bearer resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource/mcp\""
        );
    }

    #[test]
    fn oauth_accepts_only_active_audience_bound_tokens() {
        let issuer = Url::parse("https://auth.example.com/").unwrap();
        let resource = Url::parse("https://mcp.example.com/mcp").unwrap();
        assert!(is_active_for_resource(
            &json!({
                "active": true,
                "iss": issuer,
                "aud": ["another-audience", resource]
            }),
            &issuer,
            &resource
        ));
        assert!(!is_active_for_resource(
            &json!({"active": false, "aud": resource}),
            &issuer,
            &resource
        ));
        assert!(!is_active_for_resource(
            &json!({"active": true, "aud": "https://other.example.com/mcp"}),
            &issuer,
            &resource
        ));
    }

    #[test]
    fn oauth_metadata_url_follows_rfc_9728_path_insertion() {
        let resource = Url::parse("https://mcp.example.com/public/mcp?ignored=yes").unwrap();
        assert_eq!(
            protected_resource_metadata_url(&resource).unwrap().as_str(),
            "https://mcp.example.com/.well-known/oauth-protected-resource/public/mcp"
        );
    }
}
