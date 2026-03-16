# Template Schema (`daemon/v1`)

This document defines the `wasm1` agent template format. The schema is compatible with
`subd`'s `daemon/v1` format.

## Minimal template

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: General purpose helper
  model: xai:grok-4-fast-reasoning
spec:
  system_prompt: |
    You are a helpful assistant.
```

## Top-level fields

| Field | Type | Required | Notes |
|---|---|---:|---|
| `apiVersion` | string | yes | Must be `daemon/v1`. |
| `kind` | string | yes | Must be `Agent`. |
| `metadata` | object | yes | Runtime + behavior config. |
| `spec` | object | yes | Prompt and execution-facing data. |

## `metadata` fields

| Field | Type | Required | Notes |
|---|---|---:|---|
| `description` | string | no | Human-friendly description. |
| `model` | string | no | Provider-prefixed model string, e.g. `xai:grok-4-fast-reasoning` or `copilot:gpt-4o`. If omitted, host falls back to `XAI_MODEL` then built-in default. |
| `context_window` | number | no | Context window size in tokens. Known xAI models are resolved automatically from a built-in table; Copilot models currently require this field for explicit window reporting. Used to display `[CTX]` usage percentages. |

### Provider auth environment variables

- `xai:*` models require `XAI_API_KEY`.
- `copilot:*` models require either:
  - `GITHUB_COPILOT_API_TOKEN` and optional `COPILOT_API_URL`, or
  - `COPILOT_TOKEN` (or `COPILOT_API_KEY`) and optional `COPILOT_API_URL`, or
  - `COPILOT_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN` (host exchanges it for a Copilot session token).
| `labels` | string[] | no | Optional tags for discovery and filtering. |
| `hooks` | array | no | Template-local hooks merged with repo/user hooks. See [HOOKS.md](HOOKS.md). |
| `validate` | string | no | JavaScript validation function body run against the final assistant reply (`reply` arg). Must return truthy to accept. |
| `max_validation_fails` | number | no | Maximum validation retries before failing. Absent = unlimited retries (still bounded by `max_steps`). |
| `tools` | array | no | Tool allowlist for the session. Absent = all tools. |
| `shell` | object | no | Shell execution policy. |

## `metadata.shell` fields

| Field | Type | Required | Notes |
|---|---|---:|---|
| `allow` | array | no | Ordered list of regexp patterns matched against the full command string. First match wins. Absent (or empty) = all shell commands denied. |
| `timeout_secs` | number | no | Kill child processes after N seconds. Default: wait indefinitely. |

## `spec` fields

| Field | Type | Required | Notes |
|---|---|---:|---|
| `system_prompt` | string | no | EJS-rendered system message. See [EJS in system_prompt](#ejs-in-system_prompt) below. |
| `max_steps` | number | no | Maximum tool-call iterations. Absent = unlimited. |

---

## Final output validation (`metadata.validate`)

When `metadata.validate` is set, wasm1 enforces final-output shape before finishing the run.

- The validator is evaluated as `new Function('reply', validateCode)(reply)`.
- `reply` is the assistant's final text (`finish_reason: stop`).
- Truthy return value passes validation.
- Falsy return value (or validator exception) injects a correction user message and retries.
- Retry stops when validation passes, `metadata.max_validation_fails` is reached, or `max_steps` is reached.

Correction prompt injected on failure:

```text
Your reply failed validation because the validation function returned: <result>. Please review the javascript validation function code provided, and adapt your reply to conform strictly.
```

The validator source is also appended to `spec.system_prompt` so the model can see the exact rule.

Example:

```yaml
metadata:
  validate: |
    const out = JSON.parse(reply);
    if (!out || typeof out !== 'object') return false;
    if (!['noop', 'notified', 'failed'].includes(out.status)) return false;
    if (typeof out.message !== 'string') return false;
    return out;
  max_validation_fails: 3
```

---

## MiniJinja in `system_prompt`

`spec.system_prompt` is rendered using [MiniJinja](https://github.com/mitsuhiko/minijinja) before being sent to the model.
This allows dynamic content, file includes, and shell output to be embedded in prompts.

### Available helpers

| Name | Signature | Returns | Description |
|---|---|---|---|
| `readStdin` | `readStdin()` | `string` | Reads stdin content when `-i` (read stdin) flag is passed on the CLI. |
| `shell` | `shell(cmd: string)` | `string` | Runs `cmd` via shell and returns trimmed stdout. |
| `includePrompt` | `includePrompt(path: string)` | `string` | Reads and returns a workspace-relative file. Path traversal outside workspace is rejected. |
| `process` | — | object | Host process info (`cwd`, `env`, `platform`, `shell`). |
| `os` | — | object | Host OS info (`release`). |

### MiniJinja example

```yaml
spec:
  system_prompt: |
    CWD: {{ process.cwd }}
    Platform: {{ process.platform }}

    Shared rules:
    {{ includePrompt('.agent/snippets/shared/rules.md') }}

    Tool help:
    {{ shell('cat .agent/snippets/tool-help.md') }}

    Conversation history:
    {{ readStdin() }}
```

---

## Available tools

### JavaScript sandbox

| Tool name | Description |
|---|---|
| `js_exec` | Execute JavaScript in the sandboxed Boa ES2020 interpreter. Globals: `console.log`, `fs.readFile(path)`, `fs.writeFile(path, content)`, `fs.readdir(dir)`, `require(path)`. Real host filesystem is NOT accessible. |

### Virtual filesystem (tcow)

| Tool name | Description |
|---|---|
| `fs__file__view` | Read the full contents of a `.tcow` virtual-FS file. |
| `fs__file__create` | Create or overwrite a `.tcow` virtual-FS file. |
| `fs__file__edit` | Replace the first occurrence of `oldString` with `newString` in a `.tcow` file. |
| `fs__directory__list` | List entries under a directory in the `.tcow` virtual FS. |

### Message queue (host filesystem)

See [MSGQ.md](MSGQ.md) for full parameter reference.

| Tool name | Description |
|---|---|
| `msgq__append` | Create a new message in `.agent/msgq/pending/`. |
| `msgq__claim` | Atomically claim a pending message (moves to `assigned/`). |
| `msgq__list` | List messages with state/field filters. |
| `msgq__await` | Block until a filtered queue view changes or a minimum count is reached. |
| `msgq__update` | Update an assigned message and append a history event. |
| `msgq__archive` | Move a message to `archive/` with a resolution. |
| `msgq__bcast` | Fan out one payload to multiple recipients as individual pending messages. |

### Team orchestration

See [TEAM.md](TEAM.md) for full parameter reference.

| Tool name | Description |
|---|---|
| `team__create` | Launch worker wasm1 processes asynchronously; persist team metadata to `.agent/msgq/teams/`. |
| `team__destroy` | Send stop signal to all team worker processes; optional SIGKILL escalation. |

When `metadata.tools` is absent, all tools above are available to the model.

---

## LLM tool-call JSON formats

The model returns one of:

```jsonc
// Tool call — named tool with structured args
{"type":"tool_call","tool":"fs__file__view","args":{"filePath":"notes.md"},"thought":"..."}
{"type":"tool_call","tool":"fs__file__create","args":{"filePath":"out.txt","content":"hello"},"thought":"..."}
{"type":"tool_call","tool":"fs__file__edit","args":{"filePath":"out.txt","oldString":"hello","newString":"world"},"thought":"..."}
{"type":"tool_call","tool":"fs__directory__list","args":{"path":""},"thought":"..."}

// js_exec — args.code or top-level code field both accepted
{"type":"tool_call","tool":"js_exec","args":{"code":"console.log(1+1)"},"thought":"..."}

// Final answer
{"type":"final","answer":"42","thought":"..."}
```

---

## Example templates

### Minimal (all tools, no shell)

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: General purpose helper
  model: xai:grok-4-fast-reasoning
```

### FS-only agent

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: File editor
  model: xai:grok-4-fast-reasoning
  tools:
    - fs__file__view
    - fs__file__create
    - fs__file__edit
    - fs__directory__list
spec:
  max_steps: 20
```

### Shell-enabled automation agent

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: Shell agent
  model: xai:grok-4-fast-reasoning
  # context_window is optional — omit for known xAI models (auto-detected).
  # Set only for custom or unlisted models:
  # context_window: 131072
  tools:
    - js_exec
    - fs__file__view
    - fs__file__create
    - fs__file__edit
    - fs__directory__list
  shell:
    allow:
      - '^bash\b'
      - '^python3\b'
      - '^git\s+(status|log|diff)\b'
    timeout_secs: 60
spec:
  max_steps: 50
  system_prompt: |
    You are a shell automation agent.
```
### Lead + worker team

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: Lead orchestrator
  model: xai:grok-4-fast-reasoning
  tools:
    - fs__file__create
    - fs__file__view
    - msgq__append
    - msgq__list
    - msgq__await
    - team__create
    - team__destroy
spec:
  max_steps: 30
  system_prompt: |
    You are a lead orchestrator. Use team__create to launch workers,
    msgq to coordinate tasks, and team__destroy when done.
```

---

## `.agent/` directory layout

All wasm1 runtime state lives under `.agent/` in the workspace root (note the leading dot — distinct from `agent/` used in subd).

```text
.agent/
  templates/          ← agent template YAML files (*.yaml)
  hooks/              ← repo-level hook YAML files (*.yaml)
  sessions/           ← session YAML snapshots (<session_id>.yaml)
  fs/                 ← per-session virtual filesystems (<session_id>.tcow)
  msgq/
    pending/          ← unclaimed msgq messages (*.md)
    assigned/         ← claimed messages in progress (*.md)
    archive/          ← completed/failed/cancelled messages (*.md)
    teams/            ← team metadata written by team__create (<team_id>.yml)
  snippets/           ← shared prompt fragments for EJS includes
```

User-global include paths are currently disabled; only workspace-local `.agent/templates` and `.agent/hooks` are loaded.

---

## Sessions

When a run starts, the host creates a **session** from the template. The session is a live copy of the template enriched with runtime state — message history, status, and usage metrics — and persisted to disk after each agent loop iteration.

### Session ID

Every session gets a canonical ID generated at launch:

```
<timestampMs>-<pid>-<hex4>
```

Example: `1771208672042-143059-84e7`

IDs are unique across concurrent runs. Non-canonical IDs are rejected by `--session-id`.

### Storage path

```
.agent/sessions/<session_id>.yaml
```

The file is written after each loop tick and can be inspected or recovered at any time.

### Session file schema

A session file has the same `apiVersion`/`kind` as the template it was created from. The `metadata` block is the template `metadata` merged with runtime fields, and `spec` gains a `messages` array:

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  id: 1771208672042-143059-84e7
  name: solo
  model: xai:grok-4-fast-reasoning
  tools: [js_exec]
  labels: []
  status: success           # FSM state (see below)
  created: 2026-02-28T12:00:00.000Z
  last_pid: 143059
  lastTransition:
    action: complete
    from: running
    to: success
    timestamp: 2026-02-28T12:00:45.123Z
  usage:
    prompt_tokens: 4084
    completion_tokens: 40
    total_tokens: 4191
    model: grok-4-1-fast-reasoning
    timestamp: 2026-02-28T12:00:44.900Z
spec:
  system_prompt: |
    You are a general-purpose assistant. ...
  max_steps: null
  messages:
    - role: user
      content: "find and run the magic 8-ball program file"
    - role: assistant
      content: ""
      tool_calls:
        - id: call_75656196
          type: function
          function:
            name: js_exec
            arguments: '{"code":"require(\"fs\").readdir(\".\")"}'
      finish_reason: tool_calls
      timestamp: 2026-02-28T12:00:10.000Z
    - role: tool
      tool_call_id: call_75656196
      name: js_exec
      content: '{"stdout":"interesting_facts.txt,magic8ball.js","result":"undefined","error":null}'
      timestamp: 2026-02-28T12:00:10.500Z
    - role: assistant
      content: "Found and ran `magic8ball.js` ..."
      finish_reason: stop
      timestamp: 2026-02-28T12:00:44.900Z
```

### FSM — session states

```
pending ──start──► running ──complete──► success
                │          └──fail──────► error
                ├──pause──► paused ──resume──► pending
                └──stop───► stopped

success ──retry──► pending
error   ──retry──► pending
stopped ──run────► running
```

| State | Meaning |
|---|---|
| `pending` | Created; waiting to start (or resumed from paused). |
| `running` | Agent loop is active; awaiting LLM or tool response. |
| `success` | LLM returned `finish_reason: stop`. Terminal unless retried. |
| `error` | Unexpected error halted the loop. Terminal unless retried. |
| `paused` | Paused by `SIGUSR1` or tool call; can be resumed. |
| `stopped` | Stopped by `SIGUSR2` or explicit stop. Can be restarted. |

### Session recovery

On process startup, sessions in `running` state are detected and auto-resumed from the last persisted message. Sessions in `success` or `error` are left as-is unless explicitly retried.

---

## Built-in context window table

The host resolves `context_window` automatically for these model prefixes
(after stripping an optional `xai:` provider prefix):

| Model prefix | Context window |
|---|---:|
| `grok-4-1-fast*` | 2,000,000 |
| `grok-4-fast*` | 2,000,000 |
| `grok-4*` | 256,000 |
| `grok-3-mini*` | 131,072 |
| `grok-3*` | 1,000,000 |
| `grok-2*` | 32,768 |

If the model is unknown and `metadata.context_window` is absent, usage is printed in raw tokens without a percentage.

---

## See also

- [HOOKS.md](HOOKS.md) — hook events, execution model, `cron` subcommands, systemd service
- [MSGQ.md](MSGQ.md) — message queue tool reference, directory layout, message format
- [TEAM.md](TEAM.md) — team orchestration, `team__create` / `team__destroy`, worker isolation model