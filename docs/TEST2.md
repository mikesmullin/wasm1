# Integration Smoke Test Run — `docs/HOOKS.md`, `docs/MSGQ.md`, `docs/TEAM.md`

This document defines a manual smoke-test battery for the newly added features:

- template validation + tool allowlist behavior
- hook loading/execution/blocking
- cron hook triggers
- filesystem-backed msgq tools
- team worker orchestration tools

Like `docs/TEST.md`, these are command-driven checks with expected outcomes.

---

## First Pass Results (2026-02-28)

- T1: ✅ PASS (`unsupported apiVersion 'daemon/v2', expected daemon/v1`)
- T2: ✅ PASS (`unsupported kind 'Worker', expected Agent`)
- T3: ✅ PASS (`.agent/hook_cron.log` contains `cron_tick once`)
- T4: ✅ PASS (`wc -l .agent/hook_cron.log` => `3`)
- T5: ✅ PASS (session hooks ran; `.agent/hook_session.log` has `session_start` + `session_end`)
- T6: ✅ PASS (`pre_tool_call` hook blocked `fs__file__create` with reason `smoke block fs__file__create`)
- T7: ✅ PASS (`msgq__append` created `smoke-task-1`; file present in `.agent/msgq/pending/`)
- T8: ✅ PASS (pending → assigned move verified on disk)
- T9: ✅ PASS (ownership enforcement observed: `assignee mismatch` error surfaced)
- T10: ✅ PASS (`msgq__archive` moved file to archive and fired `task_completed` hook)
- T11: ✅ PASS (`msgq__await` returned `reason=timeout`; required a stricter prompt to keep `timeout_ms`)
- T12: ❌ FAIL (`msgq__bcast` hit `message id already exists` due rapid ID collision in fan-out)
- T13: ✅ PASS (`team__create` wrote `.agent/msgq/teams/smoke-team-1.yml`, launched_count=1)
- T14: ✅ PASS (`team__destroy` returned a documented status; worker PID was no longer present after run)

### Notes from first pass

- LLM tool-call argument fidelity is imperfect in some steps (it may omit optional/requested fields), so prompts sometimes needed tightening for deterministic checks.
- `T12` is a real implementation defect, not prompt variance: `msgq__bcast` currently reuses colliding IDs when multiple messages are created within the same millisecond.

### Post-fix retest (2026-02-28)

- Applied ID generation fix in [src/main.rs](src/main.rs) (`msg_id` now includes an atomic sequence + longer hash suffix).
- Re-ran `T12` with clean `.agent/msgq` state: ✅ PASS (`count=3`, three unique IDs, and `find .agent/msgq/pending -name '*.md' | wc -l` => `3`).

---

## Prerequisites

```bash
# In /workspace/tmp/wasm1
cargo build
cargo build --manifest-path guest/Cargo.toml --target wasm32-wasip1

# Required for agent-loop tests (msgq/team/tool dispatch via LLM)
export XAI_API_KEY="..."
```

Optional cleanup between runs:

```bash
rm -rf .agent/msgq
mkdir -p .agent/templates .agent/hooks tmp/coordination
```

---

## Test Fixtures

Create purpose-built templates/hooks for deterministic smoke runs.

### F1 — Template: invalid `apiVersion`

```bash
cat > .agent/templates/smoke-bad-apiver.yaml <<'YAML'
apiVersion: daemon/v2
kind: Agent
metadata:
  model: xai:grok-4-fast-reasoning
spec:
  system_prompt: |
    test
YAML
```

### F2 — Template: invalid `kind`

```bash
cat > .agent/templates/smoke-bad-kind.yaml <<'YAML'
apiVersion: daemon/v1
kind: Worker
metadata:
  model: xai:grok-4-fast-reasoning
spec:
  system_prompt: |
    test
YAML
```

### F3 — Template: hooks + fs/msgq/team tools

```bash
cat > .agent/templates/smoke-orchestrator.yaml <<'YAML'
apiVersion: daemon/v1
kind: Agent
metadata:
  description: Smoke orchestrator
  model: xai:grok-4-fast-reasoning
  tools:
    - fs__file__create
    - fs__file__view
    - fs__directory__list
    - msgq__append
    - msgq__claim
    - msgq__list
    - msgq__await
    - msgq__update
    - msgq__archive
    - msgq__bcast
    - team__create
    - team__destroy
  hooks:
    - name: smoke-session-start
      on: session_start
      jobs:
        log:
          steps:
            - type: shell
              command: echo "session_start" >> .agent/hook_session.log

    - name: smoke-session-end
      on: session_end
      jobs:
        log:
          steps:
            - type: shell
              command: echo "session_end" >> .agent/hook_session.log

    - name: smoke-block-tool
      on: pre_tool_call
      when:
        tool_name: fs__file__create
      jobs:
        guard:
          steps:
            - type: shell
              command: echo '{"blocked":true,"reason":"smoke block fs__file__create"}'
spec:
  max_steps: 20
  system_prompt: |
    You are a smoke-test agent. Follow user instructions exactly and use only requested tools.
YAML
```

### F4 — Repo hook for `cron_tick`

```bash
cat > .agent/hooks/smoke-cron.yaml <<'YAML'
hooks:
  - name: smoke-cron-log
    on: cron_tick
    jobs:
      log:
        steps:
          - type: shell
            command: echo "cron_tick ${{ trigger }}" >> .agent/hook_cron.log
YAML
```

### F5 — Team worker template

```bash
cat > .agent/templates/smoke-worker.yaml <<'YAML'
apiVersion: daemon/v1
kind: Agent
metadata:
  description: Worker smoke template
  model: xai:grok-4-fast-reasoning
  tools:
    - msgq__await
    - msgq__claim
    - msgq__update
    - msgq__archive
spec:
  max_steps: 8
  system_prompt: |
    You are worker agent:smoke.
YAML
```

---

## Smoke Tests

### T1 — Reject unsupported `apiVersion`

```bash
cargo run -- -t smoke-bad-apiver "hello" 2>&1 | grep -E "unsupported apiVersion|Error"
```

**Expected:** error mentions unsupported `apiVersion` and expected `daemon/v1`.

---

### T2 — Reject unsupported `kind`

```bash
cargo run -- -t smoke-bad-kind "hello" 2>&1 | grep -E "unsupported kind|Error"
```

**Expected:** error mentions unsupported `kind` and expected `Agent`.

---

### T3 — `cron once` triggers `cron_tick` hooks

```bash
rm -f .agent/hook_cron.log
cargo run -- cron once
cat .agent/hook_cron.log
```

**Expected:** one line containing `cron_tick once`.

---

### T4 — `cron watch` keeps firing `cron_tick`

```bash
rm -f .agent/hook_cron.log
cargo run -- cron watch &
pid=$!
sleep 125
kill -INT "$pid"
wc -l .agent/hook_cron.log
```

**Expected:** at least 2 lines in `.agent/hook_cron.log`.

---

### T5 — `before_agent_start` / session hooks execute

Use a prompt that does not need blocked tools.

```bash
rm -f .agent/hook_session.log
cargo run -- -t smoke-orchestrator "Use fs__directory__list with path='' and then provide a final answer." \
  2>&1 | grep -E "Hooks loaded|Agent loop complete|blocked"
cat .agent/hook_session.log
```

**Expected:**
- host run completes
- `.agent/hook_session.log` contains both `session_start` and `session_end` lines

---

### T6 — Blocking hook prevents selected tool

This validates `pre_tool_call` blocking for `fs__file__create`.

```bash
cargo run -- -t smoke-orchestrator "Call fs__file__create with filePath='smoke.txt' and content='x'." \
  2>&1 | grep -E "smoke block fs__file__create|Tool result|error"
```

**Expected:** tool execution is blocked with reason `smoke block fs__file__create`.

---

### T7 — `msgq__append` + `msgq__list`

```bash
rm -rf .agent/msgq
cargo run -- -t smoke-orchestrator \
  "Call msgq__append with id='smoke-task-1', type='task', recipient='agent:smoke', payload={'k':'v'}. Then call msgq__list(state='pending') and summarize ids only." \
  2>&1 | grep -E "smoke-task-1|Tool result|Final answer"

find .agent/msgq -maxdepth 2 -type f | sort
```

**Expected:** `smoke-task-1.md` exists under `.agent/msgq/pending/` and appears in list output.

---

### T8 — `msgq__claim` moves pending → assigned

```bash
cargo run -- -t smoke-orchestrator \
  "Call msgq__claim with id='smoke-task-1', assignee='agent:smoke'. Then call msgq__list(state='assigned')." \
  2>&1 | grep -E "smoke-task-1|assigned|Tool result"

ls -1 .agent/msgq/pending .agent/msgq/assigned
```

**Expected:** file disappears from `pending/` and appears in `assigned/`.

---

### T9 — `msgq__update` requires assignee match

```bash
cargo run -- -t smoke-orchestrator \
  "Call msgq__update for id='smoke-task-1' with assignee='agent:wrong' and status='in_progress'. Return exact error text." \
  2>&1 | grep -E "assignee mismatch|Tool result|error"
```

**Expected:** error contains `assignee mismatch`.

Then valid update:

```bash
cargo run -- -t smoke-orchestrator \
  "Call msgq__update for id='smoke-task-1' with assignee='agent:smoke', status='in_progress', history_event='progress'." \
  2>&1 | grep -E "in_progress|Tool result"
```

**Expected:** status update succeeds.

---

### T10 — `msgq__archive` moves to archive and can trigger `task_completed`

Add a temporary task-complete hook:

```bash
cat > .agent/hooks/smoke-task-complete.yaml <<'YAML'
hooks:
  - name: smoke-task-complete-log
    on: task_completed
    jobs:
      log:
        steps:
          - type: shell
            command: echo "task_completed ${{ task_id }}" >> .agent/hook_task_completed.log
YAML

rm -f .agent/hook_task_completed.log
cargo run -- -t smoke-orchestrator \
  "Call msgq__archive with id='smoke-task-1', assignee='agent:smoke', resolution='completed'." \
  2>&1 | grep -E "archive|completed|Tool result"

ls -1 .agent/msgq/archive
cat .agent/hook_task_completed.log
```

**Expected:**
- `smoke-task-1.md` is in `.agent/msgq/archive/`
- `.agent/hook_task_completed.log` contains `task_completed smoke-task-1`

---

### T11 — `msgq__await` with timeout

```bash
cargo run -- -t smoke-orchestrator \
  "Call msgq__await with state='pending', recipient='agent:none', timeout_ms=1500, poll_ms=200. Return reason only." \
  2>&1 | grep -E "timeout|reason|Tool result"
```

**Expected:** return payload includes `reason: timeout`.

---

### T12 — `msgq__bcast` fan-out

```bash
cargo run -- -t smoke-orchestrator \
  "Call msgq__bcast with recipients=['agent:a','agent:b','agent:c'], type='note', payload={'smoke':true}. Then call msgq__list(state='pending')." \
  2>&1 | grep -E "count|agent:a|agent:b|agent:c|Tool result"
```

**Expected:** 3 new pending messages (one per recipient).

---

### T13 — `team__create` writes team metadata

```bash
cargo run -- -t smoke-orchestrator \
  "Call team__create with team_id='smoke-team-1' and one worker using template='smoke-worker', prompt='Wait for one task then stop', output='tmp/coordination/smoke-worker.log'. Return only team_id and launched_count." \
  2>&1 | grep -E "smoke-team-1|launched_count|Tool result"

cat .agent/msgq/teams/smoke-team-1.yml
```

**Expected:** team file exists with member entry and non-empty `pid` for launched worker.

---

### T14 — `team__destroy` stops workers

```bash
cargo run -- -t smoke-orchestrator \
  "Call team__destroy with team_id='smoke-team-1', signal='SIGQUIT', force_after_ms=500. Return member statuses." \
  2>&1 | grep -E "smoke-team-1|stopped|not_found|signal_sent_still_alive|Tool result"
```

**Expected:** member status is one of documented destroy outcomes; team file remains unless `remove_file=true`.

---

## Verification Summary Checklist

- [ ] T1/T2 template validation errors are correct
- [ ] T3/T4 cron hook execution confirmed
- [ ] T5 session hooks run
- [ ] T6 blocking hook prevents selected tool
- [ ] T7–T12 msgq lifecycle works (append/claim/update/archive/await/bcast)
- [ ] T13/T14 team create/destroy lifecycle works

---

## Notes

- For LLM-driven tool invocations, model behavior can vary; prompts here are intentionally imperative for deterministic smoke coverage.
- If a run does not call the intended tool, rerun once with a stricter prompt: “Do not explain. Perform only this exact tool call.”
- Keep this file as an execution log by adding `Result: ✅/❌` under each test when run.