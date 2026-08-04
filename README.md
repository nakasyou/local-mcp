# local-mcp

`local-mcp` exposes basic local-machine capabilities as MCP tools: file reads,
image reads, directory listings, sandboxed file writes and commands, plus explicitly approved
unsandboxed command execution and read-only CLI subagents. It intentionally does not provide
a built-in web search or dedicated network-request tool.

Commands are isolated with OpenAI Codex's `codex-rs/sandboxing`: Landlock and the
Linux sandbox helper on Linux, and Seatbelt (`sandbox-exec`) on macOS. Network
access is denied for ordinary commands.

## Usage

```sh
cargo build --release

# Run one persistent MCP server (for example through a tunnel):
local-mcp mcp

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
from the prompt to confirm the working directory and sandbox roots. One
`local-mcp mcp` process can therefore serve multiple independently configured
sessions.
`get_image` returns PNG, JPEG, GIF, WebP, BMP, TIFF, and AVIF files as native MCP
image content. Relative image paths are resolved from the session working directory.

Each session uses its own permission-restricted Unix domain socket. Both the MCP
server and the start UI block on I/O, so idle operation and pending approvals
do not use polling timers.

The `start` screen also receives live activity from MCP calls. It shows file and
image reads, directory listings, file edits with unified diffs and line counts,
and command start/completion with output, in a compact Codex-style timeline.
`execute` returns its normal result for commands that finish within 30 seconds.
Longer commands continue in the background and return a `job_id`; use `poll_job`
to check for completion or `stop_job` to terminate them. Use `start_command`
when a command should run in the background immediately without the 30-second
foreground wait.

## CLI subagents

Subagents follow Codex's thread-oriented multi-agent shape: `spawn_agent` starts
a named task, `send_message` queues context, `followup_task` triggers another turn,
`wait_agent` waits for completion, `list_agents` returns status and results, and
`close_agent` releases a thread. Follow-up turns receive the recent transcript so
they preserve the delegated task's context across separate CLI invocations.

Providers are not enabled by the server. Configure the providers a session may use
from the `local-mcp start` screen, similarly to permission management:

```text
/provider add fast opencode anthropic/claude-sonnet-4-5
/provider add reviewer claude sonnet
/provider add codex_readonly codex
/provider default fast
/provider list
/provider remove reviewer
```

The first provider becomes the default automatically. A later `spawn_agent` can
select one of these names with `agent_type`, or omit it to use the default. Provider
configuration is saved with a stable session ID and restored on the next
`local-mcp start <session-id>`. Each provider executable must be installed in the
MCP server's `PATH` and authenticated using that CLI's normal setup.

| Provider | Command | Safety mode | Optional `model` value |
| --- | --- | --- | --- |
| OpenCode | `opencode run` | pure plan agent with write/shell/MCP denied | `provider/model` |
| Codex | `codex exec` | `--sandbox read-only` | Codex model name |
| Claude Code | `claude --print` | safe plan mode with read-only built-ins | Claude model name or alias |
| Gemini CLI | `gemini --prompt` | `--approval-mode plan` | Gemini model name |

The task message is sent to the selected external provider. Spawning a thread runs
on the host with network access and the CLI's existing credentials, so local-mcp
requests explicit approval unless the session is in yolo mode. That permission is
inherited by follow-up turns on the same thread, matching Codex's subagent model.
Subagents are started in read-only or plan mode and return advice instead of
modifying the workspace. Close agents when their work is no longer needed; each
local-mcp session can keep up to four agent threads open concurrently.

On Linux, the build produces `local-mcp` and its sibling `codex-linux-sandbox`;
install or copy both into the same directory, and ensure `bwrap` (bubblewrap) is
available in `PATH`. On macOS, only `local-mcp` is needed; sandboxed commands use
the system `/usr/bin/sandbox-exec`. Windows support is not implemented yet.
