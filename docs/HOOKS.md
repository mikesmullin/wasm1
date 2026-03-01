# HOOKS

Event-driven automations that run at specific lifecycle points in the agent runtime.

## What Hooks Are

Hooks attach behaviour to named events without modifying core agent logic.

Use hooks for:

- Policy checks and guardrails (blocking hooks)
- Side effects: logging, notifications, memory workflows (non-blocking hooks)

Keep blocking hooks fast and deterministic.

## Load Order and Override Rule

Hook definitions are merged with this precedence (highest first):

1. Template hooks (`metadata.hooks`)
2. User hooks (`~/.config/daemon/agent/hooks/*.yaml`)
3. Repo hooks (`.agent/hooks/*.yaml`)

If two hooks share the same identity (`on` + `name`), the higher-precedence definition wins.

---

## Hook Shape

```yaml
hooks:
  - name: mem-save-on-user-prompt
    on: user_prompt_submit
    when:
      channel: cli
    jobs:
      memorize-user-facts:
        steps:
          - id: proposed
            type: llm
            template: memory-user-extract
            stdin: |
              User: ${{ user_message }}
            prompt: go

          - id: existing
            type: shell
            command: bun plugins/memory/scripts/retrieve-existing-memories.mjs
            stdin: ${{ steps.proposed.output }}

          - id: diff
            type: llm
            template: memory-maintain
            prompt: go
            data:
              proposed_memories: ${{ parseJSON(steps.proposed.output).facts }}
              existing_memories: ${{ parseJSON(steps.existing.output) }}

          - id: apply
            type: shell
            command: bun plugins/memory/scripts/apply-memory-events.mjs
            stdin: ${{ steps.diff.output }}
```

---

## Execution Model

### Jobs

- `jobs.<job>.needs` defines DAG dependencies between jobs.
- Jobs without `needs` can start immediately.
- Jobs with all `needs` satisfied may run in parallel.
- Steps inside each job run serially.

### Steps

- `id` is optional, but required for stable downstream references.
- Step output is always a single string from stdout.
- Reference output downstream via `steps.<id>.output`.

---

## Expressions

### Syntax

Use `${{ ... }}` in hook fields.

### Available Roots

- Event payload keys directly, e.g. `${{ user_message }}`
- Step output by id, e.g. `${{ steps.proposed.output }}`

### Functions

- `parseJSON(string)` — parse a JSON string into an object or array. Strict: invalid JSON fails the step.

---

## Type Rules

- `command`, `prompt`, `stdin`, and all `env` values must resolve to strings.
- `data` values may resolve to any JSON-compatible type.

---

## Supported Step Types

| Type | Description |
|---|---|
| `shell` | Execute a shell command. Output is captured from stdout. |
| `llm` | Run a nested wasm1 template session. Output is the final answer. |

---

## `when` Conditions

`when` filters hooks by payload values. All keys must match.

| Matcher | Example |
|---|---|
| Exact | `when: { channel: cli }` |
| Any-of | `when: { response_channel: [cli, api] }` |
| Wildcard | `when: { tool_name: "shell__*" }` |

---

## Hook Events

### Cron events

| Event | Fired by | Payload extras |
|---|---|---|
| `cron_tick` | `cron watch` (each interval) or `cron once` (single trigger) | `trigger: "watch"\|"once"`, `tick_at` |

### Session lifecycle

| Event | Blocking | Payload extras |
|---|---|---|
| `session_start` | no | — |
| `session_end` | no | `exit_reason` |

### Agent lifecycle

| Event | Blocking | Payload extras |
|---|---|---|
| `before_agent_start` | yes | `template`, `prompt` |
| `agent_terminated_stop` | no | `step_count`, `finish_reason` |

### User interaction

| Event | Blocking | Payload extras |
|---|---|---|
| `user_prompt_submit` | yes | `user_message`, `channel` |
| `permission_request` | yes | `tool_name`, `tool_input` |

### LLM output

| Event | Blocking | Payload extras |
|---|---|---|
| `assistant_response_emit` | yes | `assistant_message` |

### Tool execution

| Event | Blocking | Payload extras |
|---|---|---|
| `pre_tool_call` | yes | `tool_name`, `tool_input` |
| `post_tool_call` | no | `tool_name`, `tool_input`, `tool_output` |
| `post_tool_failure` | no | `tool_name`, `tool_input`, `error_message` |

### Task lifecycle

| Event | Blocking | Payload extras |
|---|---|---|
| `task_completed` | yes | `task_id`, `task_type`, `result_summary`, `files_changed` |

---

## Hook Payload

All hooks receive a base JSON payload on stdin:

```json
{
  "hook": "event_name",
  "session_id": "sess_abc123",
  "timestamp": "2026-02-15T17:45:00Z",
  "agent_id": "main",
  "workspace": "/path/to/project",
  "reason": "optional prior context"
}
```

Event-specific fields are merged in as documented above.

---

## Blocking Behavior

A blocking hook may prevent the triggering action from proceeding.

- Return a non-zero exit code or write `{"blocked": true, "reason": "..."}` to stdout to block.
- Post-event hooks (non-blocking) never prevent the original action.

---

## `cron` Subcommands

The `cron` system fires `cron_tick` events on a schedule, allowing hooks and agents to perform periodic work.

### `cron once`

Fires a single `cron_tick` event then exits. Useful for testing or one-shot scheduled tasks.

```bash
wasm1 cron once
```

### `cron watch`

Fires `cron_tick` events on each interval and loops indefinitely. Exits gracefully on `SIGINT` after the current trigger finishes.

```bash
wasm1 cron watch
```

Intended for use with a process supervisor such as systemd.

---

## systemd Service

A `wasm1-cron.service` unit file is provided at the workspace root for running `cron watch` as a managed service.

```ini
[Unit]
Description=wasm1 cron watch
After=network.target

[Service]
Type=simple
WorkingDirectory=/path/to/workspace
ExecStart=/path/to/wasm1 cron watch
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
```

Copy and adapt it, then enable with:

```bash
systemctl --user enable --now wasm1-cron.service
```

systemd acts as a process monitor: if `cron watch` crashes, systemd restarts it automatically after `RestartSec`.

---

## Example: per-session audit log hook

```yaml
hooks:
  - name: audit-tool-calls
    on: post_tool_call
    jobs:
      log:
        steps:
          - type: shell
            command: |
              echo "$(date -u +%FT%TZ) tool=${{ tool_name }}" >> .agent/audit.log
```

## Example: block dangerous shell commands

```yaml
hooks:
  - name: block-rm-rf
    on: pre_tool_call
    when:
      tool_name: "js_exec"
    jobs:
      guard:
        steps:
          - id: check
            type: shell
            command: |
              echo '${{ tool_input }}' | grep -q 'rm -rf' && echo '{"blocked":true,"reason":"rm -rf denied"}' || echo '{"blocked":false}'
