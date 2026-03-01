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
| `cron_tick` | `cron watch` (each interval) or `cron once` (single trigger) | `trigger: "watch"\|"once"`, `tick_at`, `agent_name`, `hook_name`, `cron.stateKey`, `cron.nowMs`, `cron.state` |

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
wasm1 cron once -v
```

### `cron watch`

Fires `cron_tick` events on each interval and loops indefinitely. Exits gracefully on `SIGINT` after the current trigger finishes.

```bash
wasm1 cron watch
wasm1 cron watch -v
```

Intended for use with a process supervisor such as systemd.

### Cron loop interval config (`config.yaml`)

`cron watch` evaluates hooks on a fixed loop interval configured in workspace-root `config.yaml`:

```yaml
cron:
  interval_ms: 60000
```

- Default: `60000` (1 minute)
- If `config.yaml` is missing at cron startup, runtime creates it automatically with this default.
- This interval is an upper bound on scheduling resolution: hooks cannot run more frequently than the watch loop checks.

Example: even if a hook returns `nextRunInMs: 10`, if `interval_ms` is `600000` (10 minutes), that hook will not be reconsidered until the next 10-minute loop tick.

### Verbose mode (`-v`)

When `-v` is passed to `cron once` or `cron watch`, runtime prints colored progress output:

- startup:
  - each discovered `cron_tick` hook and whether it is enabled
  - total cron hook count
- each cron iteration, per hook:
  - `RUN` or `SKIP`
  - skip reason (for example, disabled or `nextRunAt` still in the future)
  - for `RUN`, completion status and hook duration in ms
- iteration summary:
  - hooks ran vs skipped
  - success vs failure counts
  - total elapsed time for the iteration

---

## Cron state file (`.agent/cron/state.yaml`)

Cron hooks persist scheduling state in:

```text
.agent/cron/state.yaml
```

Top-level shape is a map with one key per hook identity:

```yaml
samy-tracker:samy-tracker-cron-tick:
  lastRunAt: 1772351291154
  nextRunAt: 1772351411154
  nextRunInMs: 120000
  notes: "retry after Discord warmup"
```

- Key format: `<agent_name>:<hook.name>`
- `lastRunAt`: set by runtime at end of each hook run (unix ms)
- `nextRunAt`: absolute unix ms when hook should run next
- Any extra keys returned by the hook's LLM output are stored verbatim

`cron watch` uses `nextRunAt` to decide whether each hook is due; hooks with `nextRunAt > now` are skipped for that tick.

---

## Cron LLM scheduling contract

For `cron_tick` hooks that execute `llm` steps, runtime parses the final LLM output as JSON (when possible).

Recommended fields in structured output:

- `nextRunInMs` (number): relative delay in milliseconds; runtime computes `nextRunAt = now + nextRunInMs`
- `nextRunAt` (number): absolute unix ms (used if `nextRunInMs` is absent)

If both are present, `nextRunInMs` takes precedence.

Use `nextRunInMs` whenever possible (avoids clock math in prompts).

Examples:

```json
{"status":"noop","message":"not time yet","nextRunInMs":120000}
```

```json
{"status":"failed","message":"discord still warming up","nextRunInMs":5000,"attempt":2}
```

```json
{"status":"notified","message":"sent alert","nextRunInMs":120000}
```

### Example: pass cron state into LLM prompt

Use hook payload variables to give the model continuity between runs:

```yaml
hooks:
  - name: samy-tracker-cron-tick
    on: cron_tick
    jobs:
      run:
        steps:
          - type: llm
            template: samy-tracker
            prompt: |
              go
              nowMs: ${{ cron.nowMs }}
              stateKey: ${{ cron.stateKey }}
              priorState: ${{ cron.state }}
```

In the template system prompt, instruct the model to parse `nowMs` + `priorState` and decide whether to run now or wait.

Example decision rule:

- if `priorState.lastRunAt` exists and `nowMs - priorState.lastRunAt < 120000`, return:

```json
{"status":"noop","message":"waiting for next interval","nextRunInMs":120000 - (nowMs - priorState.lastRunAt)}
```

This enables stable continuity in `cron watch` without requiring external schedulers per hook.

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
