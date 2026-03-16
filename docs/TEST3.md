# STEPWISE Smoke Test Run — `docs/STEPWISE.md` (Implemented Slice)

This document defines a manual smoke-test battery for the STEPWISE functionality implemented so far:

- one-turn-per-invocation behavior scaffolding
- session `status` transitions in snapshot metadata
- resume mode via `-s/--session`
- CLI guardrails for `-s`/`-t`/prompt combinations
- resumable session YAML parse/load path

Like `docs/TEST.md` and `docs/TEST2.md`, these are command-driven checks with expected outcomes.

---

## Scope of this smoke pass

This pass verifies only what is implemented now.

Not covered yet (tracked for later):

- strict one-line-only stdout contract (current host logs are still printed)
- strict enforcement of no-extra-logs mode

---

## First Pass Results (2026-03-15)

### Status Matrix (Latest)

| Group | Test | Status | Notes |
|---|---|---|---|
| Core STEPWISE | S1 | PASS | CLI usage includes `-s <session_id>` |
| Core STEPWISE | S2 | PASS | `-t` and `-s` are mutually exclusive |
| Core STEPWISE | S3 | PASS | Prompt rejected when using `-s` |
| Core STEPWISE | S4 | PASS | Missing session file read failure |
| Core STEPWISE | S5 | PASS | Malformed YAML parse failure |
| Core STEPWISE | S6 | PASS | Resume prompt restored correctly |
| Core STEPWISE | S7 | PASS | `metadata.status` accepted on resume |
| Auto-approve | A2 | PASS | Tool call auto-approved (`approved`, `sent:true`) |
| Auto-approve | A3 | PASS | Tool result auto-approved (`approved`, `sent:true`) |
| Manual approval | M1 | PASS | 1/2 approved executes only approved call |
| Manual approval | M2 | PASS | 2/2 approved executes both calls |
| Manual approval | M3 | PASS | 0/2 approved executes none; call IDs stable |
| Manual approval | M4 | PASS | Tool-result approval leads to non-empty ACK final |

- S1: ✅ PASS (usage line includes `-s <session_id>`)
- S2: ✅ PASS (`-t/--template and -s/--session are mutually exclusive`)
- S3: ✅ PASS (`prompt input is not accepted when -s/--session is used`)
- S4: ✅ PASS (missing file returns `failed to read session file` + `No such file or directory`)
- S5: ✅ PASS (malformed YAML returns `failed to parse session YAML`)
- S6: ✅ PASS (resume restored prompt: `hello from resume`)
- S7: ✅ PASS (`metadata.status` field accepted; resume parse succeeded)
- A2: ✅ PASS (auto-approved tool_call emitted with `status: approved` + `sent: true`)
- A3: ✅ PASS (resume produced auto-approved `tool_result` with `sent: true`)

### Notes from first pass

- S6 completed successfully beyond auth checks in this environment; assertions were based on prompt restoration to avoid environment-specific provider auth variance.
- Running S6 mutates the session fixture (status transitions to `SUCCESS`), so S7 asserts presence of `status:` rather than a fixed `status: IDLE` literal.

---

## Prerequisites

```bash
# In /workspace/tmp/wasm1
cargo check -q
```

Optional cleanup between runs:

```bash
rm -rf .agent/sessions
mkdir -p .agent/sessions
```

---

## Test Fixtures

### F1 — malformed session YAML

```bash
mkdir -p .agent/sessions
cat > .agent/sessions/smoke-bad-yaml.yaml <<'YAML'
metadata:
  id: smoke-bad-yaml
spec:
  messages:
    - role: user
      content: "hello"
  this is not valid yaml
YAML
```

### F2 — valid minimal resumable session YAML

```bash
mkdir -p .agent/sessions
cat > .agent/sessions/smoke-resume-ok.yaml <<'YAML'
apiVersion: daemon/v1
kind: Agent
metadata:
  id: smoke-resume-ok
  name: smoke
  model: xai:grok-4-fast-reasoning
  status: IDLE
spec:
  system_prompt: |
    You are a smoke session.
  messages:
    - role: user
      content: "hello from resume"
YAML
```

---

## Smoke Tests

### S1 — CLI usage includes STEPWISE `-s` path when no args are provided

```bash
cargo run -- 2>&1 | grep -E "usage:|-s <session_id>"
```

Expected: usage line includes `-s <session_id>`.

---

### S2 — `-t` and `-s` are mutually exclusive

```bash
cargo run -- -t smoke -s smoke-resume-ok 2>&1 | grep -E "mutually exclusive"
```

Expected: error contains `-t/--template and -s/--session are mutually exclusive`.

---

### S3 — prompt is rejected when `-s` is used

```bash
cargo run -- -s smoke-resume-ok "unexpected prompt" 2>&1 | grep -E "prompt input is not accepted"
```

Expected: error contains `prompt input is not accepted when -s/--session is used`.

---

### S4 — missing resume session file returns read failure

```bash
cargo run -- -s smoke-missing 2>&1 | grep -E "failed to read session file|No such file"
```

Expected: read error referencing `.agent/sessions/smoke-missing.yaml`.

---

### S5 — malformed session YAML returns parse failure

```bash
cargo run -- -s smoke-bad-yaml 2>&1 | grep -E "failed to parse session YAML"
```

Expected: parse error referencing `smoke-bad-yaml.yaml`.

---

### S6 — valid resume file is loaded and reaches provider/auth stage

```bash
cargo run -- -s smoke-resume-ok 2>&1 | grep -E "Starting agent with prompt|xai provider requires XAI_API_KEY"
```

Expected:

- shows `Starting agent with prompt: "hello from resume"`
- then fails fast on missing `XAI_API_KEY` in environments where key is not set

If `XAI_API_KEY` is set locally, this test may proceed further; in that case, treat prompt restoration as the primary assertion.

---

### S7 — `status` field from resume YAML is accepted (no schema rejection)

```bash
grep -n "^  status: " .agent/sessions/smoke-resume-ok.yaml
cargo run -- -s smoke-resume-ok 2>&1 | grep -E "failed to parse session YAML|Starting agent with prompt"
```

Expected:

- fixture contains a `status:` field
- runtime does not reject the schema due to unknown/invalid `state`
- runtime does not reject the schema due to unknown/invalid `status`

---

## Execution Notes

Record exact output snippets for each test and then replace `PENDING` with `PASS`/`FAIL` above.

### Captured output snippets (2026-03-15)

- S1:
  - `Error: usage: cargo run -- [clean|cron <once|watch>|-t <template> <prompt>|-s <session_id>]`
- S2:
  - `Error: -t/--template and -s/--session are mutually exclusive`
- S3:
  - `Error: prompt input is not accepted when -s/--session is used`
- S4:
  - `Error: failed to read session file: /workspace/tmp/wasm1/.agent/sessions/smoke-missing.yaml`
  - `No such file or directory (os error 2)`
- S5:
  - `Error: failed to parse session YAML: /workspace/tmp/wasm1/.agent/sessions/smoke-bad-yaml.yaml`
- S6:
  - `[HOST] Starting agent with prompt: "hello from resume"`
- S7:
  - `7:  status: SUCCESS`
  - `[HOST] Starting agent with prompt: "hello from resume"`

---

## Auto-Approve Smoke Tests (2026-03-16)

### A1 — Fixture: auto-approve template

```bash
mkdir -p .agent/templates
cat > .agent/templates/smoke-auto-approve.yaml <<'YAML'
apiVersion: daemon/v1
kind: Agent
metadata:
  model: xai:grok-4-1-fast-reasoning
  tools:
    - fs__directory__list
  auto_approve:
    - tool: fs__directory__list
spec:
  system_prompt: |
    Use the requested tool.
YAML
```

### A2 — First run writes auto-approved tool_call meta (no manual edit)

```bash
out=$(cargo run -- -t smoke-auto-approve "Use fs__directory__list on path '.' and return only that tool call." | tail -n 1)
echo "$out"
sid=$(echo "$out" | awk '{print $1}')
rg -n "tool_call_batch|status: approved|sent: true|fs__directory__list" ".agent/sessions/${sid}.yaml"
```

Expected:

- assistant tool-call batch exists
- per-call approval is `status: approved`
- per-call `sent: true`

Observed (verified):

- `.agent/sessions/1773642891117-117774-ad1c.yaml` contains:
  - `kind: tool_call_batch`
  - `status: approved`
  - `sent: true`

### A3 — Resume once without edits; tool_result is also auto-approved

```bash
cargo run -- -s "$sid" | tail -n 1
rg -n "role: tool|kind: tool_result|status: approved|sent: true|tool_call_id" ".agent/sessions/${sid}.yaml"
```

Expected:

- tool result entry exists
- tool result approval is `status: approved`
- tool result `sent: true`
- no YAML edits between A2 and A3

Current status:

- Verified PASS using session `.agent/sessions/1773643707037-123684-7222.yaml`:
  - tool result entry exists (`role: tool`, `kind: tool_result`)
  - `approval.status: approved`
  - `sent: true`
  - no manual YAML edits between A2 and A3

### A3 debugging notes (resolved)

- Rebuilt guest Wasm after host/guest schema drift (`guest/src/lib.rs` missing `pending_tool_calls` in default seed initializer).
- Fixed host snapshot clobber paths so decision/checkpoint writes are not overwritten by empty fallback snapshots.
- Resume mode now reloads auto-approve rules from the session template name when `-s` is used without `-t`.

---

## Manual Approval Regression Smoke (2026-03-16)

These tests validate the post-fix behavior for manual tool-call/tool-result approvals, including mixed parallel approvals.

### M0 — Fixture: manual parallel template (no auto-approve)

```bash
mkdir -p .agent/templates
cat > .agent/templates/smoke-manual-parallel.yaml <<'YAML'
apiVersion: daemon/v1
kind: Agent
metadata:
  model: xai:grok-4-1-fast-reasoning
  tools:
    - fs__directory__list
    - fs__file__view
spec:
  system_prompt: |
    You are a strict test agent.
    When asked, emit exactly two tool calls in one response and no prose.
YAML
```

### M1 — One-of-two approved: execute only approved call

```bash
out=$(cargo run -- -t smoke-manual-parallel "Emit exactly two parallel tool calls now: fs__directory__list with path='.' and fs__file__view with filePath='README.md'. Return only tool calls." | tail -n 1)
sid=$(echo "$out" | awk '{print $1}')
echo "$sid"

# Manually edit .agent/sessions/${sid}.yaml:
# - set exactly one call to approval.status=approved and sent=true
# - leave the other call as approval.status=pending and sent=false

cargo run -- -s "$sid" | tail -n 1
rg -n "role: tool|tool_call_id:|call_[0-9]+:|status: pending|status: approved|sent: false|sent: true" ".agent/sessions/${sid}.yaml"
```

Expected:

- exactly one `role: tool` entry is appended
- approved call remains `approved/sent:true`
- unapproved call remains `pending/sent:false`

Observed (PASS):

- verified with `.agent/sessions/1773718784912-99449-f30b.yaml`
- one tool result appended (`tool_call_id: call_84997613`)
- other call remained pending (`call_41669479`)

### M2 — Two-of-two approved: execute both calls in same turn

```bash
out=$(cargo run -- -t smoke-manual-parallel "Emit exactly two parallel tool calls now: fs__directory__list with path='.' and fs__file__view with filePath='README.md'. Return only tool calls." | tail -n 1)
sid=$(echo "$out" | awk '{print $1}')
echo "$sid"

# Manually edit .agent/sessions/${sid}.yaml:
# - set both calls to approval.status=approved and sent=true

cargo run -- -s "$sid" | tail -n 1
rg -n "role: tool|tool_call_id:|status: pending|sent: false" ".agent/sessions/${sid}.yaml"
```

Expected:

- two `role: tool` entries are appended in that resume turn
- each tool result starts as pending output approval (`status: pending`, `sent: false`) unless auto-approved by template

Observed (PASS):

- verified with `.agent/sessions/1773718794673-99574-8857.yaml`
- two tool results appended (`call_86117931`, `call_64160389`)
- both results were pending output approval

### M3 — None-of-two approved: no execution and no call-id churn

```bash
out=$(cargo run -- -t smoke-manual-parallel "Emit exactly two parallel tool calls now: fs__directory__list with path='.' and fs__file__view with filePath='README.md'. Return only tool calls." | tail -n 1)
sid=$(echo "$out" | awk '{print $1}')
echo "$sid"

echo "--- before"
rg -n "id: call_|status: pending|sent: false" ".agent/sessions/${sid}.yaml"

# No manual approval edits
cargo run -- -s "$sid" | tail -n 1

echo "--- after"
rg -n "id: call_|status: pending|sent: false|^  - role: tool" ".agent/sessions/${sid}.yaml"
```

Expected:

- no `role: tool` entries are appended
- original pending call IDs are preserved (no regenerated tool_call batch)

Observed (PASS):

- verified with `.agent/sessions/1773718869162-100774-72f5.yaml`
- no tool execution occurred
- call IDs remained unchanged after resume

### M4 — End-to-end ACK after manual tool_result approval

```bash
cat > .agent/templates/smoke-manual-js.yaml <<'YAML'
apiVersion: daemon/v1
kind: Agent
metadata:
  model: xai:grok-4-1-fast-reasoning
  tools:
    - js_exec
spec:
  system_prompt: |
    First call js_exec exactly once using the user-requested code.
    After tool result is approved, reply with a non-empty final answer that begins with ACK: and includes the computed value.
YAML

out=$(cargo run -- -t smoke-manual-js "Use js_exec with code '40+2'. Then answer with ACK: and the value." | tail -n 1)
sid=$(echo "$out" | awk '{print $1}')
echo "$sid"

# 1) Manually approve the pending tool call in .agent/sessions/${sid}.yaml
cargo run -- -s "$sid" | tail -n 1

# 2) Manually approve the pending tool result in .agent/sessions/${sid}.yaml
cargo run -- -s "$sid" 2>&1 | rg -n "LLM raw response|Final answer|ACK"
```

Expected:

- after second approval, next turn returns non-empty final text acknowledging tool result

Observed (PASS):

- verified with `.agent/sessions/1773719081835-101553-3e8d.yaml`
- runtime trace included:
  - `LLM raw response: {"type":"final","answer":"ACK: 42","thought":null}`
  - `Final answer: ACK: 42`
