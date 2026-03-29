# STEPWISE — wasm1: Step-wise Agent Loop + Tool Approval

## Overview

`wasm1` runs **step-wise** — one turn of the agent loop per process invocation. The session YAML file on disk is the authoritative state store. Every invocation reads from it, executes one turn, writes back to it, and exits.

This enables:
- Fine-grained human-in-the-loop control (edit session YAML between steps).
- Resumable sessions across any number of invocations.
- Interception and approval of both tool calls and tool results before the LLM sees them.

## One step per invocation

Each `wasm1` invocation performs exactly one of:

1. **LLM inference** — if no approved pending tool calls exist, sends context to LLM, appends the response (assistant message or tool-call batch), exits `IDLE`.
2. **One tool execution** — if an approved pending tool call exists, executes that one call, appends the result with approval gate `pending`, exits `IDLE`.
3. **Blocked on approval** — if pending items exist but none are approved, exits `IDLE` immediately.

The external loop (gdedit or a shell script) drives progression by calling `wasm1 -s <id>` repeatedly.

## Stdout contract (machine-readable one-liner)

On every exit, wasm1 prints exactly one line to stdout:

```
<session_id> <last_status> <session_file>
```

- `session_id` — e.g. `1741980000000-12345-ab12`
- `last_status` — `IDLE`, `SUCCESS`, or `FAIL`
- `session_file` — e.g. `.agent/sessions/1741980000000-12345-ab12.yaml`

Example:
```
1741980000000-12345-ab12 IDLE .agent/sessions/1741980000000-12345-ab12.yaml
```

The caller parses this line to know where the session file is and whether to prompt the user for approval, auto-advance, or stop.

## `metadata.status` field

| State | Meaning |
|---|---|
| `IDLE` | Turn ended without final answer; waiting for next step (approval, resume, etc.) |
| `SUCCESS` | LLM returned `finish_reason: stop`. Session complete. |
| `FAIL` | Error or exhausted retry policy. |

Finer-grained approval status is tracked per-message in `meta.approval.status` and per-call in `meta.calls.<id>.approval.status`, not in the top-level FSM.

## `spec.messages[]` schema

Each entry has two sub-objects:

- `verbatim`: provider-facing payload sent to the AI Provider.
- `meta`: local-only state; never sent to provider.

See [TEMPLATE.md](TEMPLATE.md) for complete schema with examples.

Context inclusion rules:
- `meta.visible: false` → excluded from all context (operator gate).
- `meta.sent: false` → excluded from LLM context (approval gate).
- For tool-call batches: evaluated per-call via `meta.calls.<id>.sent`.

---

## CLI

```bash
# Create new session (one turn: runs hooks + LLM inference, exits IDLE)
wasm1 -t <template> "<prompt>"

# Resume existing session (one turn)
wasm1 -s <session_id>

# Execute a shell command in session context (no chat history modification)
wasm1 -s <session_id> exec "<command>"
```

Verbosity flags:
- (none): print final assistant response only
- `-v`: print user/assistant/tool event summary
- `-vv`: full host/guest diagnostics
- `-vvv`: hook execution trace (considered, executed, inputs/outputs)
- `-q`: suppress all stdout output

---

## Tool Call Approval Gates

### Gate 1 — Tool call input

When the LLM requests a tool call, wasm1:
1. Appends the assistant message with per-call approval state (`status: pending`, `sent: false`) unless auto-approved.
2. Writes session YAML with `metadata.status: IDLE`.
3. Exits.

Human operator approves by editing session YAML:
```yaml
meta:
  calls:
    call_abc123:
      sent: true
      approval:
        status: approved   # pending | approved | rejected | modified
```

Then `wasm1 -s <id>` resumes and executes **one** approved call.

### Gate 2 — Tool result

After executing one tool call, wasm1:
1. Appends the tool result with `meta.approval.status: pending` and `meta.sent: false` (unless auto-approved).
2. Writes session YAML with `metadata.status: IDLE`.
3. Exits.

Human operator approves by editing session YAML:
```yaml
meta:
  sent: true
  approval:
    status: approved   # approved | rejected | modified
    modified_content: null  # if modified: content to send to LLM instead
```

Then `wasm1 -s <id>` resumes. If more approved pending calls exist, execute next. Otherwise proceed to next LLM inference.

### Allowlists and auto-approval

Approval behaviour is governed by two independent allowlists declared in the agent template, forming a layered security model:

Two independent lists control tool execution:

#### Layer A — tool allow list (`metadata.tools.<tool>.allow`)

Controls which shell commands the LLM can even request. Default-deny if empty.

```yaml
metadata:
  tools:
    - shell_execute:
        allow:
          - '^openrgb($|\s)'
          - '^govee($|\s)'
```

#### Layer B — auto-approval list (`metadata.tools.<tool>.auto_approve`)

Controls which calls execute without pausing for human review. Must be a subset of `allow`.

```yaml
metadata:
  tools:
    - shell_execute:
        allow:
          - '^openrgb($|\s)'
        auto_approve:
          - '^openrgb($|\s)'   # no human review for openrgb calls
```

- Layer A: can the LLM request this command?
- Layer B: does it run without human approval?
- Commands on Layer A but not Layer B pause for human approval.
- No global `--auto-approve` flag exists.

---

## Typical step-through scenario

```
# 1. Create session (runs on_cron + filter_inf_req hooks, then LLM inference)
wasm1 cron once "^desk-light-toggle$" -v
# → session IDLE, tool call pending approval

# 2. Human approves call in session YAML:
#    meta.calls.call_abc123.sent: true
#    meta.calls.call_abc123.approval.status: approved

# 3. Execute one tool call
wasm1 -s <session_id>
# → tool result appended, result pending approval

# 4. Human approves result in session YAML:
#    meta.sent: true
#    meta.approval.status: approved

# 5. Proceed to next inference
wasm1 -s <session_id>
# → next LLM response
```

Sessions can also be created and stepped through with `-t`/`-s`:

```bash
# Create new session
wasm1 -t desk-light-toggle "go"

# Step forward
wasm1 -s <session_id>
wasm1 -s <session_id>
# ... repeat until SUCCESS
```
