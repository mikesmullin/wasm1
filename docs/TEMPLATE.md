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

## Session Snapshot Schema

`wasm1` persists session snapshots under `.agent/sessions/<session_id>.yaml`.
Sessions support collaborative lanes: multiple agents sharing one ordered timeline, each with its own
system prompt, tool policy, and context subscription.

### Collaborative participants (`metadata.collaborators`)

```yaml
metadata:
  collaborators:
    - desk-light-toggle   # root agent (always first)
    - toolshed            # collaborator — activates toolshed hooks
```

`collaborators` is a list of template names. It determines which templates' `metadata.hooks` are active in the session. The root agent's hooks are always active. Any other template listed here also has its hooks activated.

Agent IDs for each participant are generated at session creation and recorded in `spec.system_prompts` (see below).

### Per-agent system prompts (`spec.system_prompts`)

System prompts are stored per agent, keyed by agent ID:

```yaml
spec:
  system_prompts:
    agent-desk-light-toggle-1774753727414-480c25: |
      You are a desk-light automation assistant.
    agent-toolshed-1774753727733-6e9b85: |
      You are toolshed, a tool-recommendation specialist.
```

The agent ID format is `agent-<template>-<ts>-<short_hash>`. The appropriate system prompt is selected
per inference request based on the active agent lane.

### Per-agent tool policy (`spec.tools`)

Tools and their policy are stored per agent:

```yaml
spec:
  tools:
    agent-desk-light-toggle-1774753727414-480c25:
      - shell_execute         # simple string = tool enabled, no inline policy
    agent-toolshed-1774753727733-6e9b85:
      - shell_execute:
          allow:
            - '^skills'
            - '^echo'
          auto_approve:
            - '^skills($|\s)'
            - '^echo($|\s)'
```

Tool policy is frozen at session creation from each agent's template. Agents cannot amend their tool list mid-session. When a collaborator is added to the session its tool policy is snapshotted at that point.

### Message model

Each timeline entry in `spec.messages[]` stores:

- `id`: stable event id (`event-<ts>-<short_hash>` or `hook-<ts>-<short_hash>`)
- `role`: `user`, `assistant`, `tool`, or `hook`
- `verbatim`: provider-safe payload (only this is forwarded to LLM API)
- `meta`: local orchestration/audit fields (never forwarded to LLM API)

Common `meta` fields:

| Field | Description |
|---|---|
| `visible` | Global operator gate — human-controlled boolean |
| `sent` | Provider inclusion gate; `false` = pending approval |
| `subscribers` | List of `agent_id` values that receive this item in context assembly |
| `origin` | Source: `user`, `llm`, `tool`, `hook`, `validation` |
| `kind` | Entry subtype: `tool_call_batch`, `tool_result` |

For assistant tool-call batches, per-call approval state is stored under `meta.calls.<call_id>`:

```yaml
meta:
  kind: tool_call_batch
  calls:
    call_abc123:
      sent: false
      approval:
        status: pending    # pending | approved | rejected | modified
        reviewed_at: null
        reason: null
        modified_args: null
```

For tool result entries, approval state is at `meta.approval`:

```yaml
meta:
  kind: tool_result
  sent: false
  approval:
    status: pending        # pending | approved | rejected | modified
    reviewed_at: null
    reason: null
    modified_content: null  # if set + status=modified, sent to LLM instead of verbatim content
```

### Hook execution records in `spec.messages[]`

Hook execution events are stored as `role: hook` entries inline in the messages timeline (not in a separate list). They are never forwarded to LLM API. They carry the full hook audit record:

```yaml
- id: hook-1774753727416-4235d9
  role: hook
  verbatim:
    hook_name: desk-light-cron-trigger
    event: on_cron
    considered: true
    executed: true
    blocked: false
    order: 1
    step: 0
    phase: on_cron
    input_verbatim: '...'
    output_verbatim: ''
    reason: null
  meta:
    origin: hook
    phase: on_cron
    sent: false
    visible: true
    subscribers: [agent-desk-light-toggle-...]
```

### Hook checkpoint (`spec.hook_state`)

```yaml
spec:
  hook_state:
    last_event: filter_inf_res
    last_hook_id: hook-1774753750927-f93ae5
    last_timestamp: '1774753750927'
    blocked: false
```

This is updated after each hook execution phase and used to resume hook chains across step boundaries.

## `metadata` fields

| Field | Type | Required | Notes |
|---|---|---:|---|
| `description` | string | no | Human-friendly description. |
| `model` | string | no | Provider-prefixed model string, e.g. `xai:grok-4-fast-reasoning` or `copilot:gpt-4o`. If omitted, host falls back to `XAI_MODEL` then built-in default. |
| `context_window` | number | no | Context window size in tokens. Known xAI models are resolved automatically from a built-in table; Copilot models currently require this field for explicit window reporting. Used to display `[CTX]` usage percentages. |
| `labels` | string[] | no | Optional tags for discovery and filtering. |
| `hooks` | array | no | Template-local hooks. Active only when this template is the root agent or listed in `collaborators`. See [HOOKS.md](HOOKS.md). |
| `collaborators` | string[] | no | Template names whose hooks should be active in sessions using this template. |
| `validate` | string | no | JavaScript validation function body run against the final assistant reply (`reply` arg). Must return truthy to accept. |
| `max_validation_fails` | number | no | Maximum validation retries before failing. Absent = unlimited retries (still bounded by `max_steps`). |
| `tools` | array | no | Tool policy for this agent. Entries may be plain tool names or single-key maps with `allow`/`auto_approve` sublists. Absent = all tools enabled. |
| `shell` | object | no | Shell execution policy (legacy; prefer inline tool policy). |

### Provider auth environment variables

- `xai:*` models require `XAI_API_KEY`.
- `copilot:*` models require either:
  - `GITHUB_COPILOT_API_TOKEN` and optional `COPILOT_API_URL`, or
  - `COPILOT_TOKEN` (or `COPILOT_API_KEY`) and optional `COPILOT_API_URL`, or
  - `COPILOT_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN` (host exchanges it for a Copilot session token).

## `metadata.tools` format

Tools may be listed as plain names or with inline policy:

```yaml
metadata:
  tools:
    - shell_execute:           # single-key map with policy
        allow:                 # shell command allow-list (regex, default-deny if empty)
          - '^openrgb($|\s)'
          - '^govee($|\s)'
        auto_approve:          # auto-approve without human review (subset of allow)
          - '^openrgb($|\s)'
          - '^govee($|\s)'
    - fs__file__view           # plain name — tool enabled, no inline policy
```

- `allow`: ordered list of regex patterns matched against the full command string. First match wins. If empty/absent: all shell commands denied.
- `auto_approve`: patterns for commands that execute without pausing for human approval. Must be a subset of `allow`.

## `metadata.shell` fields (legacy)

| Field | Type | Required | Notes |
|---|---|---:|---|
| `allow` | array | no | Merged with `metadata.tools.shell_execute.allow` (OR behavior). |
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

### Shell emulator

| Tool name | Description |
|---|---|
| `shell_execute` | Execute a single shell command via the constrained host emulator. Input: `{"command":"..."}`. Built-ins: `echo`, `cat`, `ls`, `pwd`, `cd`. External commands spawn host processes subject to the template allow-list. See [SHELL.md](SHELL.md). |

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
{"type":"tool_call","tool":"shell_execute","args":{"command":"ls -la"},"thought":"..."}

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
  tools:
    - shell_execute:
        allow:
          - '^git\s+(status|log|diff)\b'
          - '^python3\b'
        auto_approve:
          - '^git\s+status\b'
    - fs__file__view
    - fs__file__create
    - fs__file__edit
    - fs__directory__list
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

Session files share `apiVersion: daemon/v1` / `kind: Agent` with templates. The `metadata` block is the template `metadata` merged with runtime fields, and `spec` gains `messages`, `system_prompts`, `tools`, and hook fields:

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  id: 1774753727414-351661-aa06
  name: desk-light-toggle
  model: xai:grok-4-1-fast-reasoning
  status: IDLE              # FSM state (see below)
  cwd: /
  workdir: /
  created: '1774753727414'
  last_pid: 351662
  labels: []
  collaborators:
    - desk-light-toggle
    - toolshed
  last_transition:
    action: tool_call
    from: IDLE
    to: IDLE
    timestamp: '1774753750928'
spec:
  system_prompts:
    agent-desk-light-toggle-1774753727414-480c25: |
      You are a desk-light automation assistant.
    agent-toolshed-1774753727733-6e9b85: |
      You are toolshed, a tool-recommendation specialist.
  tools:
    agent-desk-light-toggle-1774753727414-480c25:
      - shell_execute:
          allow:
            - '^openrgb($|\s)'
            - '^govee($|\s)'
          auto_approve:
            - '^openrgb($|\s)'
            - '^govee($|\s)'
    agent-toolshed-1774753727733-6e9b85:
      - shell_execute:
          allow:
            - '^skills'
          auto_approve:
            - '^skills($|\s)'
  messages:
    - id: event-1774753727414-69824d
      role: user
      verbatim:
        content: "toggle desk light color (if red, make blue. if blue, make red)"
        timestamp: '1774753727414'
      meta:
        visible: true
        sent: false
        origin: user
        subscribers:
          - agent-desk-light-toggle-1774753727414-480c25
    - id: hook-1774753727416-4235d9
      role: hook
      verbatim:
        hook_name: desk-light-cron-trigger
        event: on_cron
        executed: true
        # ... (full hook audit record)
      meta:
        visible: true
        sent: false
        origin: hook
    - id: event-1774753750927-50669f
      role: assistant
      verbatim:
        content: ''
        tool_calls:
          - id: call_53929654
            type: function
            function:
              name: shell_execute
              arguments: '{"command":"openrgb --list-devices"}'
      meta:
        visible: true
        kind: tool_call_batch
        calls:
          call_53929654:
            sent: false
            approval:
              status: pending
              reviewed_at: null
              reason: null
              modified_args: null
        origin: llm
        subscribers:
          - agent-desk-light-toggle-1774753727414-480c25
  hook_state:
    last_event: filter_inf_res
    last_timestamp: '1774753750927'
    blocked: false
```

### FSM — session states

| State | Meaning |
|---|---|
| `IDLE` | Turn ended without final answer; waiting for next step (tool approval, resume, etc.) |
| `SUCCESS` | LLM returned `finish_reason: stop`. Session complete. |
| `FAIL` | Error or exhausted retry policy. |

### Session recovery

Sessions in `IDLE` state can be resumed at any time with `wasm1 -s <session_id>`. The runtime reads the session YAML, rebuilds state from `spec.messages`, and executes the next eligible step.

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