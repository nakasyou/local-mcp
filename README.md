# local-mcp

`local-mcp` exposes basic local-machine capabilities as MCP tools: file reads,
image reads, directory listings, sandboxed file writes and commands, plus explicitly approved
unsandboxed command execution. It intentionally does not provide web search or a
dedicated network-request tool.

Commands are isolated with OpenAI Codex's `codex-rs/sandboxing`: Landlock and the
Linux sandbox helper on Linux, and Seatbelt (`sandbox-exec`) on macOS. Network
access is denied for ordinary commands.

## Usage

```sh
cargo build --release

# Run one persistent MCP server over stdin/stdout (for example through a tunnel):
local-mcp mcp

# Or expose the same server over Streamable HTTP (POST /mcp):
local-mcp mcp-http

# Choose another listen address when needed:
local-mcp mcp-http --bind 127.0.0.1:8080

# Require Authorization: Bearer <token> on every HTTP request. Using the
# environment variable avoids exposing the token in the process argument list:
LOCAL_MCP_BEARER_TOKEN='replace-with-a-secret' local-mcp mcp-http

# Or generate a fresh, cryptographically secure 256-bit token at startup:
local-mcp mcp-http --generate-bearer-token

# Protect a publicly reachable server with OAuth 2.0. The authorization server
# must expose an RFC 7662 introspection endpoint whose active responses include
# this MCP server's canonical URI in the `aud` claim:
LOCAL_MCP_OAUTH_ISSUER='https://auth.example.com/' \
LOCAL_MCP_OAUTH_RESOURCE='https://mcp.example.com/mcp' \
LOCAL_MCP_OAUTH_INTROSPECTION_ENDPOINT='https://auth.example.com/oauth/introspect' \
LOCAL_MCP_OAUTH_CLIENT_ID='local-mcp' \
LOCAL_MCP_OAUTH_CLIENT_SECRET='replace-with-a-secret' \
local-mcp mcp-http --bind 0.0.0.0:3000

# In another terminal, start a session in the project directory:
cd ./some-project
local-mcp start

# Or choose a stable session ID (letters, numbers, "-", "_", and "."):
local-mcp start my-project

# Give the printed session ID to the agent in your prompt. The agent includes it
# in each local-mcp tool call.

# In the approvals UI, allow every unsandboxed call for the session:
/permissions yolo

# Manage the current session from the start screen:
/permission ask
/permission yolo
/permission allow ../another-project
/permission revoke ../another-project
/permission list
/permission status
```

With Nix, `curl` and `bash` are included in the runtime environment. Linux builds
also include `bwrap`:

```sh
nix run github:OWNER/local-mcp
nix develop
nix build
```

The session working directory is the directory where `local-mcp start` was run;
there is no separate persistent cwd setting. Sandboxed calls are always allowed
and have no network access. `without_sandbox`
runs with the service user's full host permissions and network access, so it asks
the approvals process before every call. `/permissions yolo` disables those
prompts only for the lifetime of that session; `/permissions ask`
turns prompts back on. The singular `/permission ...` spelling is also accepted.
Every tool takes a `session_id`. The agent can call `session_info` with the ID
from the prompt to confirm the working directory and sandbox roots. One MCP
server process can therefore serve multiple independently configured sessions.
`local-mcp mcp` uses stdin/stdout, while `local-mcp mcp-http` serves the stateless
Streamable HTTP JSON response transport at `http://127.0.0.1:3000/mcp` by default.
HTTP notifications receive `202 Accepted`. The HTTP server binds only to loopback
by default. Set `LOCAL_MCP_BEARER_TOKEN` or pass `--bearer-token` to require an
`Authorization: Bearer <token>` header on every HTTP request. Authentication is
disabled when neither is set. Alternatively, `--generate-bearer-token` creates a
URL-safe 256-bit token from the operating system's secure random source and prints
it once at startup.

For OAuth 2.0, configure `LOCAL_MCP_OAUTH_ISSUER`,
`LOCAL_MCP_OAUTH_RESOURCE`, and `LOCAL_MCP_OAUTH_INTROSPECTION_ENDPOINT` together.
The optional client ID and secret authenticate `local-mcp` to the introspection
endpoint with HTTP Basic authentication. OAuth mode publishes RFC 9728 Protected
Resource Metadata at `/.well-known/oauth-protected-resource` and the MCP
path-specific well-known URI, and advertises that metadata in `401 Unauthorized`
responses. Each access token is checked through RFC 7662; only active tokens whose
`aud` field contains the exact configured resource URI are accepted. The
authorization server itself remains a separate service and must provide OAuth 2.1
authorization and metadata endpoints to MCP clients.

Browser requests with an `Origin`
header are accepted only for `localhost`, `127.0.0.1`, and `::1` origins.
`get_image` returns PNG, JPEG, GIF, WebP, BMP, TIFF, and AVIF files as native MCP
image content. Relative image paths are resolved from the session working directory.

Each session uses its own local IPC endpoint: an explicitly permission-restricted
Unix domain socket on Unix, or a named pipe using Windows' default security
descriptor. Both the MCP server and the start UI block on I/O, so idle operation
and pending approvals do not use polling timers.

The `start` screen also receives live activity from MCP calls. It shows file and
image reads, directory listings, file edits with unified diffs and line counts,
and command start/completion with output, in a compact Codex-style timeline.
`execute` returns its normal result for commands that finish within 30 seconds.
Longer commands continue in the background and return a `job_id`; use `poll_job`
to check for completion or `stop_job` to terminate them. Use `start_command`
when a command should run in the background immediately without the 30-second
foreground wait.

On Linux, the build produces `local-mcp` and its sibling `codex-linux-sandbox`;
install or copy both into the same directory, and ensure `bwrap` (bubblewrap) is
available in `PATH`. On macOS, only `local-mcp` is needed; sandboxed commands use
the system `/usr/bin/sandbox-exec`. Windows uses named-pipe IPC and direct argv
execution; it does not currently provide the filesystem/network sandbox enforced
by Linux and macOS. Consequently, `execute` and `start_command` require approval
on Windows unless the session is in yolo mode, while `write_file` writes directly
to the requested host path. The Windows named pipe uses the
[default security descriptor](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights),
which grants full control to LocalSystem, administrators, and the creator owner,
and read access to Everyone and anonymous users; unlike Unix, `local-mcp` does not
install an explicit per-user ACL. Windows builds use the MSVC Rust target and
require Visual Studio Build Tools with the "Desktop development with C++"
workload. Build from a Developer PowerShell with
`cargo build --locked --release`.
