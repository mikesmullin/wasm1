# PLAN3 — wasm1: Step-wise Agent Loop + Tool Approval Queue

**Author:** gdedit team (for wasm1 maintainer)  
**Date:** 2026-03-15  
**Status:** Draft – awaiting review before implementation begins

---

## Problem Statement

`wasm1` currently runs a full agentic loop in a single process invocation: it keeps calling the LLM, dispatching tool calls, and feeding results back until a final answer is produced (or max_steps is reached), then prints that final answer to stdout.

But this prevents user from mutating the ((agent session) / (context window)) from outside of the running process. That further limits integration with external applications.

## Motivation

This plan changes wasm1 to run **step-wise** — one turn of the agent loop per process invocation. The session YAML file on disk becomes the authoritative state store, replacing in-process accumulation. Every invocation reads from it, executes one turn, and writes back to it.

This unlocks:
- Fine-grained human-in-the-loop control (the host application can mutate session YAML between steps).
- Resumable sessions.
- A simplified interface — one input/output path (the session YAML) instead of a mix of stdin/stdout/stderr.
- Interception and approval of tool call results before the LLM receives them.

---

## Part 1: Step-wise Execution Model

### 1.1 One turn per invocation

A "turn" is defined as exactly one LLM request + (if the response is a tool call) one tool execution + the result appended to the session. The process then exits.

The loop that currently lives inside the host (or guest) and iterates until `finish_reason: stop` is **removed**. The loop is now external — driven by the host application (gdedit or any shell script) calling `wasm1` repeatedly.

### 1.2 Session YAML as the only I/O

On startup:
1. Load the session YAML from disk (or create a new one if none exists).
2. Rebuild `spec.messages[]` state from the file.
3. Send the current context (system prompt + visible messages) to the LLM provider.
4. Append the LLM's response (assistant message, or tool call + tool result) to `spec.messages[]`.
5. Write the updated session YAML back to disk.
6. Exit.

**No content is printed to stdout** (except the single one-line summary described in §1.4).  
Verbose diagnostics (formerly `-v` / stderr), perf metrics, log lines — all go into `spec.messages[]` as entries with `role: "meta"` (or a new dedicated type) so gdedit can display them without polluting stdout.

### 1.3 Session file location and naming

Sessions are stored under `.agent/sessions/` in the workspace root (unchanged from today), named `<session_id>.yaml`.

Session ID format (unchanged): `<timestampMs>-<pid>-<hex4>`.

### 1.4 Stdout contract (machine-readable one-liner)

On every exit, wasm1 prints exactly one line to stdout:

```
<session_id> <last_status> <session_file>
```

Where:
- `session_id` — basename of the session file (without `.yaml`), e.g. `1741980000000-12345-ab12`.
- `last_status` — current value of `metadata.status` (see §1.5), e.g. `IDLE`, `RUNNING`, `SUCCESS`, `FAIL`.
- `session_file` — workspace-relative path to the session file, e.g. `.agent/sessions/1741980000000-12345-ab12.yaml`.

Example:
```
1741980000000-12345-ab12 IDLE .agent/sessions/1741980000000-12345-ab12.yaml
```

The caller (gdedit) parses this line to know where the session file is and whether to prompt the user for approval, auto-advance, or stop.

### 1.5 `metadata.status` field

The session YAML `metadata` block uses the existing `status` field with BehaviorTree-style values:

| State | Meaning |
|---|---|
| `RUNNING` | Default state for a new session and the state set at process start. By process end, wasm1 must have transitioned to one of the other three states. |
| `SUCCESS` | The agent turn ended with `finish_reason: stop`. |
| `IDLE` | The agent turn ended without `finish_reason: stop`, or no `finish_reason` was produced. This is the paused/waiting state, including cases where human operator approval is still required before the loop can progress. |
| `FAIL` | Something unexpected prevented the turn from reaching its natural end (`finish_reason: stop` + all tool responses received). This also includes validation failures such as after exhausting the configured retry policy from the template. |

Finer-grained approval status (pending vs. sent, and the reason) is tracked in local-only metadata (see §1.6), not in the top-level state machine. wasm1 decides whether to include an entry in the LLM context by checking local-only `sent` and `visible` gates and then sending that entry's provider-facing payload.

### 1.6 `spec.messages[]` schema extension

`spec.messages[]` keeps existing role names (`assistant`, `tool`, `user`, etc.). Instead of introducing new role names for approval/perf/local diagnostics, each existing entry is augmented with two sub-objects:

- `verbatim`: the provider-facing payload that wasm1 may send to the AI Provider, more or less as-is.
- `meta`: local-only state that is never sent to the provider directly.

This preserves the existing event/message taxonomy while cleanly separating provider context from operator/runtime state.

```yaml
# Assistant message
- role: assistant
  verbatim:
    content: "I'll inspect the project files."
    timestamp: 2026-03-15T12:00:00Z
  meta:
    visible: true
    sent: true
    perf:
      ttft_s: 1.2
      tokens: 340
      duration_s: 1.2

# Assistant tool-call batch (LLM output). `tool_calls` may contain one or more calls.
- role: assistant
  verbatim:
    content: ""
    tool_calls:
      - id: call_abc123
        type: function
        function:
          name: shell__execute
          arguments:
            command: ls -l
      - id: call_def456
        type: function
        function:
          name: fs__file__view
          arguments:
            filePath: README.md
    timestamp: 2026-03-15T12:00:00Z
  meta:
    visible: true
    kind: tool_call_batch
    calls:
      call_abc123:
        sent: false        # false = pending human review; true = included in LLM context
        approval:
          status: pending  # pending | approved | rejected | modified
          reviewed_at: null
          reason: null     # human-readable note (optional)
          modified_args: null
      call_def456:
        sent: false
        approval:
          status: pending
          reviewed_at: null
          reason: null
          modified_args: null
    perf:
      duration_s: 0.01

# Tool result — written after tool execution
- role: tool
  verbatim:
    tool_call_id: call_abc123
    name: shell__execute
    content: "total 8\n-rw-r--r-- 1 user user 123 Mar 15 notes.txt"
    timestamp: 2026-03-15T12:00:01Z
  meta:
    visible: true
    sent: false          # false = pending human review of result
    kind: tool_result
    approval:
      status: pending    # pending | approved | rejected | modified
      reviewed_at: null
      reason: null
      modified_content: null  # if set and status=modified, sent to LLM instead
    perf:
      duration_s: 0.02
```

wasm1 includes an entry in the LLM context only when approval gates are satisfied.

- For a non-tool message (for example `user`, plain `assistant`, `tool` result), include only when **both** `meta.sent: true` and `meta.visible: true`.
- For an assistant message with `verbatim.tool_calls`, evaluate each call id independently using `meta.calls.<tool_call_id>.sent` and `meta.visible`.

In all cases the provider receives `verbatim` payload (or a modified form derived from `meta.approval.modified_args` / `meta.approval.modified_content`). The `meta` object is local-only and is never passed through verbatim.

---

## Part 2: CLI Changes

### 2.1 Resuming an existing session

New flag: `-s` / `--session <session_id>`

```bash
wasm1 -s 1741980000000-12345-ab12
```

- If `-s` is provided, wasm1 loads the named session file instead of creating a new one and resumes from the YAML already on disk.
- When `-s` is used, no prompt input is accepted or assumed.
- If `-s` is omitted, wasm1 creates a new session.

`-t` / `--template` and `-s` / `--session` are mutually exclusive:

- `-t` means create a new session from a template.
- `-s` means resume an existing session.

### 2.2 Invocation and continuation

No new sub-command is needed. wasm1 infers the correct action from the session state:

```bash
# Start a new session and run first turn
wasm1 -t ontologist "Initial user prompt here"

# Continue an existing session one turn
# (wasm1 reads current metadata.status from the YAML and acts accordingly;
# no prompt is accepted in this mode)
wasm1 -s <session_id>
```

All approval decisions (approve, reject, modify tool calls or results) are made by the caller (gdedit or the user) by **directly editing the session YAML file** before the next invocation. wasm1 reads `meta.approval.status` on each pending entry at startup and acts accordingly. No CLI sub-commands are needed for approval mutations.

The original invocation style (`wasm1 -t template "prompt"`) continues to work for backwards compatibility: it creates a new session, runs the first turn, and exits after that one turn.

### 2.3 Removed flags

The following flags are **no longer meaningful** and should be removed or made no-ops:
- `-v` / `--verbose` — verbose content now goes into the local-only `meta` portion of `spec.messages[]` entries.
- `-j` / `--jsonl` — JSONL streaming output is replaced by the session YAML file.

This flag is retained:
- `-i` / read stdin for prompt. Still supported and commonly used, but only meaningful when creating a new session from a template (the stdin content is appended as the initial user message). For step-wise continuation of an existing session via `-s`, stdin is ignored.

---

## Part 3: Tool Call Approval Gates

### 3.1 Tool call input approval

When the LLM requests a tool call:
1. wasm1 appends or updates the assistant message containing `verbatim.tool_calls[]` and writes per-call gates under `meta.calls.<tool_call_id>` with `approval.status: pending` and `sent: false`.
2. wasm1 writes the session YAML with `metadata.status: IDLE`.
3. wasm1 exits, printing the one-liner to stdout.

The caller (gdedit) then:
- Displays the pending tool call to the user.
- Waits for user action.
- Writes approval decision into `meta.approval.status` (or `meta.approval.modified_args`) in the session YAML.
- Sets `meta.sent: true` when the tool call is approved or modified, or leaves `meta.sent: false` when it is rejected.
- Invokes `wasm1 -s <session_id>` to continue.

On the next invocation, wasm1 reads the assistant tool-call batch:
- If `meta.calls.<id>.approval.status: approved` → execute that tool call (using original args, or `meta.calls.<id>.approval.modified_args` if set).
- If `meta.calls.<id>.approval.status: rejected` → do not execute that call; inject a policy rejection user message for that call id.
- Calls in the same batch can have mixed outcomes (approved/rejected/modified).

### 3.2 Tool result approval

After tool execution, before feeding the result back to the LLM:
1. wasm1 appends a `tool_result` entry whose `meta.approval.status: pending` and `meta.sent: false`.
2. wasm1 writes session YAML with `metadata.status: IDLE`.
3. wasm1 exits.

The caller (gdedit) displays the tool result and waits for user action, then:
- Sets `meta.approval.status: approved` | `rejected` | `modified`.
- If `modified`, sets `meta.approval.modified_content` to the content to send instead.
- Sets `meta.sent: true` when the result is approved or modified, or leaves `meta.sent: false` when it is rejected.
- Invokes `wasm1 -s <session_id>`.

On the next invocation, wasm1 reads the result entry:
- `approved` → add `verbatim.content` to LLM context.
- `modified` → add `meta.approval.modified_content` to LLM context.
- `rejected` → add a censor error message to LLM context:
  ```
  policy error: the tool was executed successfully, but the user censored the response 
  from this tool because it may have contained sensitive information.
  ```

### 3.3 Allowlists and auto-approval

Approval behaviour is governed by two independent allowlists declared in the agent template, forming a layered security model:

#### Layer A — tool availability allowlist (existing, per-tool)

Controls which tools the LLM is even told about (i.e. which tool definitions appear in each API request). An LLM won't know to request a tool that is not on this list.

```yaml
metadata:
  tools:
    - js_exec
    - fs__file__view
    - shell__execute:
        allowlist:
          - pattern: "^ls(\\s|$)"   # only `ls` variants allowed at Layer A
          - pattern: "^date$"
```

This is the existing shell_execute command allowlist — unchanged.

#### Layer B — auto-approval allowlist (new, per-tool-name)

Controls which tool calls are **automatically approved** without pausing for human review. Anything not matched here requires a human to edit `meta.approval.status` in the session YAML before wasm1 will proceed.

```yaml
metadata:
  auto_approve:
    - tool: fs__file__view     # plain tool name — auto-approve all calls to this tool
    - tool: shell__execute
      pattern: "^date$"        # optional regexp matched against the command string
    - tool: js_exec
      pattern: "^console\\.log" # only auto-approve benign logging calls
```

When a tool call matches a Layer B entry, wasm1 sets `meta.approval.status: approved` itself (for both tool_call input and tool_result output) and does not pause. When no entry matches, wasm1 writes `meta.approval.status: pending`, sets `meta.sent: false`, and exits — waiting for the human operator to update the YAML.

#### Interaction between Layer A and Layer B

- Layer A controls **availability** (can the LLM even ask for this tool?).
- Layer B controls **human-in-the-loop** (does a human need to approve this specific call?).
- A tool can be on Layer A but not Layer B — the LLM can request it, but every use requires human approval.
- A tool can be on both — the LLM can request it, and approved uses run automatically.
- Neither layer is a substitute for the other.

There is no `--auto-approve` global flag. Teams that want fully unattended operation simply add all relevant tools to the Layer B list in their template.

---

## Part 4: Local-Only Metadata (replacing verbose stdout)

All diagnostic output that previously went to stdout/stderr in verbose mode now goes into the local-only `meta` portion of `spec.messages[]` entries. This avoids adding synthetic message types just to carry perf/log/approval state.

Examples of what becomes `meta` data attached to existing entries:
- Performance stats (TTFT, tokens/s, duration).
- Log level / diagnostic annotations.
- Context window usage percentage.
- Shell command timing.
- Validation attempts/failures.
- Approval status and modification history.

Because this metadata is in the YAML, gdedit can render perf/log/operator state in the chat UI without any additional streaming channel.

---

## Part 5: Session File Schema (complete updated shape)

```yaml
apiVersion: daemon/v1
kind: AgentSession
metadata:
  id: 1741980000000-12345-ab12
  name: ontologist
  model: xai:grok-4-fast-reasoning
  status: IDLE
  created: 2026-03-15T12:00:00Z
  last_pid: 12345
  tools: [js_exec, fs__file__view]
  max_steps: null
  labels: []
  description: ""
  last_transition:
    action: tool_call
    from: RUNNING
    to: IDLE
    timestamp: 2026-03-15T12:00:01Z
spec:
  system_prompt: |
    You are a helpful assistant.
  messages:
    - role: user
      verbatim:
        content: "List the files in the project root."
        timestamp: 2026-03-15T12:00:00Z
      meta:
        visible: true
        sent: true
    - role: assistant
      verbatim:
        content: ""
        tool_calls:
          - id: call_abc123
            type: function
            function:
              name: js_exec
              arguments:
                code: "fs.readdir('.')"
        timestamp: 2026-03-15T12:00:01Z
      meta:
        visible: true
        kind: tool_call_batch
        calls:
          call_abc123:
            sent: false
            approval:
              status: pending
              reviewed_at: null
              reason: null
              modified_args: null
        perf:
          ttft_s: 1.1
          tokens: 280
          duration_s: 1.1
```

---

## Part 6: Backwards Compatibility

- Existing `wasm1 -t template "prompt"` continues to work: creates a new session, runs one turn, exits with the one-liner.
- `wasm1 cron watch` is removed, in favor of the more step-wise `wasm1 cron once` -> which is then/now/also renamed/inferred as simply `wasm1 cron`.
- `wasm1 clean` is unchanged.
- The `.tcow` virtual filesystem continues to be used for agent tool I/O.
- `msgq__append` / queue tools continue to work as before.

---

## Summary of Changes

| Area | Change |
|---|---|
| Agent loop | Remove multi-turn loop; exit after every single turn |
| CLI output | Only one stdout line: `<session_id> <status> <session_file>` |
| Session YAML | Use `metadata.status` with STEPWISE values; keep existing role names; add per-entry `verbatim` and local-only `meta` sections with approval, perf, `sent`, and `visible` fields; for `tool_calls`, track approvals per call id in `meta.calls.<tool_call_id>` |
| CLI flags | Add `-s/--session`; keep `-i` for new sessions; deprecate `-v`, `-j` |
| Tool approval | After tool call request: pause with `status: IDLE`; after tool result: pause with `status: IDLE`; re-read YAML-edited approval on next invocation |
| Verbose output | All diagnostics written into per-entry local-only `meta` data in session YAML; nothing to stderr/stdout except the one-liner and any failure/error stderr messages that cause the process to exit early.
