# TEAM

Lead + worker agent orchestration using wasm1 and msgq.

## Overview

A *team* is a lead agent plus one or more worker agents running as separate wasm1 processes. The lead orchestrates work by posting tasks to `msgq`; workers claim and complete tasks independently. Coordination uses `msgq__await` as a completion gate — no polling, no fs watches.

Workers run as isolated wasm1 Wasm instances on the host (not containers). This replaces the podman-based sandbox model from earlier designs.

---

## Tools

### `team__create`

Launch worker processes asynchronously and record team metadata.

| Param | Type | Required | Notes |
|---|---|---:|---|
| `team_id` | string | no | Auto-generated if omitted. |
| `workers` | object[] | yes | Worker descriptors (see below). |

**Worker descriptor fields:**

| Field | Type | Required | Notes |
|---|---|---:|---|
| `template` | string | yes* | Template name (resolved from `.agent/templates/`). *Required unless `args[]` is used. |
| `prompt` | string | yes* | Initial user prompt for the worker. *Required unless `args[]` is used. |
| `session_id` | string | no | Override generated session ID. |
| `output` | string | no | Path for worker JSONL/log output. |
| `turn_limit` | number | no | Max LLM turns. Minimum enforced at 20. |
| `verbose` | boolean | no | Pass `-v` to worker. |
| `jsonl` | boolean | no | Pass `-j` to worker. |
| `args` | string[] | no | Raw CLI args, bypasses template/prompt fields. |

**Returns:**

```json
{
  "team_id": "team_1234_ab12",
  "status": "active",
  "path": ".agent/msgq/teams/team_1234_ab12.yml",
  "launched_count": 3,
  "failed_count": 0,
  "members": [
    {
      "index": 0,
      "session_id": "1771208672042-143059-84e7",
      "pid": 143059,
      "template": "gp-worker-frontend",
      "output": "tmp/coordination/frontend.log",
      "status": "launched",
      "launched_at": "2026-02-28T12:00:00.000Z"
    }
  ]
}
```

Team state is persisted to `.agent/msgq/teams/<team_id>.yml` immediately after launch.

### `team__destroy`

Gracefully stop all team worker processes.

| Param | Type | Required | Notes |
|---|---|---:|---|
| `team_id` | string | yes | ID returned by `team__create`. |
| `signal` | string | no | Signal sent to workers. Default `SIGQUIT`. |
| `force_after_ms` | number | no | Send `SIGKILL` if worker still alive after this many ms. Default `1500`. |
| `remove_file` | boolean | no | Delete the team YAML file after destroy. Default `false`. |

**Returns:** per-member stop status (`stopped`, `not_found`, `missing_pid`, `signal_sent_still_alive`).

---

## Team Lifecycle

```
lead:  team__create(workers=[...])     → workers start as background processes
       msgq__append × N               → create tasks in msgq
       msgq__await(min_count=N)       → block until all tasks archived
       msgq__list(state=archive)      → collect results
       team__destroy(team_id=...)     → SIGQUIT workers; SIGKILL after grace period
```

---

## Session ID Policy

Worker session IDs use the canonical format:

```
<timestampMs>-<pid>-<hex4>.yml
```

Example: `1771208672042-143059-84e7.yml`

Non-canonical session IDs are rejected for `--session-id`. `team__create` always generates canonical IDs internally.

---

## Example — Pull-Based Lead + 3 Workers

### Lead template (`gp-lead-pull.yaml`)

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: Pull-based lead orchestrator
  model: xai:grok-4-fast-reasoning
  tools:
    - fs__directory__create
    - fs__file__create
    - fs__file__view
    - fs__file__edit
    - msgq__append
    - msgq__list
    - msgq__await
    - team__create
    - team__destroy
spec:
  system_prompt: |
    You are lead orchestrator agent:lead.

    Required workflow:
    1. Create 3 task messages in msgq with recipients agent:frontend, agent:backend, agent:sdet.
    2. Launch a worker team using team__create with templates:
         gp-worker-frontend, gp-worker-backend, gp-worker-sdet
       Provide output log paths under tmp/coordination/*.log.
    3. Wait using msgq__await(state=archive, type=task, min_count=3).
    4. Call team__destroy after all tasks are archived.
    5. Return a final summary including archived task ids.

    Constraints:
    - Pull model: workers claim their own tasks. Do not claim/archive on their behalf.
```

### Worker template (`gp-worker-frontend.yaml`)

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: Frontend worker
  model: xai:grok-4-fast-reasoning
  hooks:
    - name: notify-lead-on-task-complete
      on: task_completed
      jobs:
        notify:
          steps:
            - type: shell
              command: echo "task_completed" >> .agent/msgq/lead_notifications.log
  tools:
    - fs__directory__list
    - fs__file__view
    - fs__file__create
    - fs__file__edit
    - fs__directory__create
    - msgq__claim
    - msgq__await
    - msgq__list
    - msgq__update
    - msgq__archive
spec:
  system_prompt: |
    You are frontend worker agent:frontend.

    Required execution order:
    1. Recovery check: msgq__list(state=assigned, assignee=agent:frontend, type=task, limit=1).
       If an assigned task exists, resume it.
    2. Otherwise: msgq__await(state=pending, recipient=agent:frontend, type=task)
       then msgq__claim(recipient=agent:frontend, assignee=agent:frontend).
    3. Do the work.
    4. msgq__update progress as needed.
    5. msgq__archive(resolution=completed).
```

### End-to-end run command

```bash
wasm1 -t gp-lead-pull \
  "Execute the pull-based workflow: create the team, let workers pull via msgq, wait for 3 archived tasks, destroy the team, return archived ids."
```

---

## Outcome Verification

```bash
# Task queue state
echo "pending=$(find .agent/msgq/pending -maxdepth 1 -name '*.md' | wc -l) \
      assigned=$(find .agent/msgq/assigned -maxdepth 1 -name '*.md' | wc -l) \
      archive=$(find .agent/msgq/archive -maxdepth 1 -name '*.md' | wc -l)"

# Expected: pending=0, assigned=0, archive=3

# List archived tasks
ls -1 .agent/msgq/archive/

# Check no lingering worker processes
ps aux | grep wasm1 | grep -v grep || echo "no workers"
```

---

## Worker Isolation Model

Workers are separate wasm1 processes on the host (ordinary OS processes, each running the Wasm guest). There is no container isolation. Isolation is provided by:

- **Wasm sandbox**: each worker's JS code runs inside the Boa Wasm guest. Real host filesystem access is restricted to the tcow virtual FS via the linker.
- **msgq confinement**: coordination is channelled exclusively through `.agent/msgq/` paths under the workspace root.
- **Tool allowlist**: the worker template's `metadata.tools` restricts which tools the worker LLM can invoke.

For stronger isolation in the future, each worker process can be further confined with OS-level sandboxing (e.g. seccomp, namespaces) without changing the team protocol.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `archive=0` after run completes | Worker hit `turn_limit` before archiving | Increase `turn_limit` in `team__create` worker descriptor or template `max_steps`. |
| Worker exits immediately (status `failed_fast`) | Template not found or prompt error | Check template name resolves in `.agent/templates/`. |
| Lead blocks indefinitely on `msgq__await` | Worker failed before archiving | Inspect worker log at `output` path; re-run or set `timeout_ms` on the await. |
| Orphaned worker process after crash | `team__destroy` not called | `kill $(cat .agent/msgq/teams/<team_id>.yml | grep pid)` or use `team__destroy` with `force_after_ms=0`. |
