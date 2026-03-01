# MSGQ

A filesystem-backed message bus for tasks, handoffs, and agent-to-agent coordination.

## Design Goal

One minimal bus model instead of many specialised plugins.

Benefits:

- Small tool surface area
- Easy shell-level inspection and debugging
- Atomic claim semantics using file moves

---

## Directory Layout

All paths are relative to the current working directory (host filesystem — **not** the tcow virtual FS):

```text
.agent/msgq/
  pending/    ← unclaimed messages
  assigned/   ← claimed messages in progress
  archive/    ← completed/failed/cancelled messages
  teams/      ← team metadata written by team__create
```

---

## Message Format

Each item is a Markdown file with YAML frontmatter.

```markdown
---
id: task-frontend-001
type: task
sender: agent:lead
recipient: agent:frontend
priority: high
status: pending
assignee: null
blockedBy: []
payload:
  description: Build the index page
history: []
created_at: 2026-02-28T12:00:00.000Z
---

Additional free-form body text goes here.
```

### Core fields

| Field | Default | Notes |
|---|---|---|
| `id` | auto-generated | Unique identifier within the bus. |
| `type` | `note` | Semantic category: `task`, `note`, etc. |
| `sender` | `agent:unknown` | Originating agent id. |
| `recipient` | `broadcast` | Target agent id or `broadcast`. |
| `priority` | `normal` | One of `low`, `normal`, `high`. |
| `status` | `pending` | Managed by the runtime; do not set manually. |
| `assignee` | `null` | Set by `msgq__claim`. |
| `blockedBy` | `[]` | IDs of messages that must be archived before this one is eligible for claim. |
| `payload` | `{}` | Arbitrary structured data. |
| `history` | `[]` | Append-only event log written by claim/update/archive operations. |

---

## Atomic Claim Rule

Claim lock acquisition is implemented as an atomic file move:

```bash
mv .agent/msgq/pending/<id>.md .agent/msgq/assigned/<id>.md
```

- Exit code `0` → claim succeeded.
- Non-zero → item missing or already claimed by another agent.

---

## Tool API

### `msgq__append`

Create a new message in `pending/`.

| Param | Type | Required | Notes |
|---|---|---:|---|
| `id` | string | no | Auto-generated if omitted. |
| `type` | string | no | Default `note`. |
| `sender` | string | no | Default `agent:unknown`. |
| `recipient` | string | no | Default `broadcast`. |
| `priority` | string | no | `low` \| `normal` \| `high`. Default `normal`. |
| `blockedBy` | string[] | no | Message IDs that must be archived first. |
| `payload` | object | no | Structured data. |
| `body` | string | no | Free-text body appended after frontmatter. |

### `msgq__claim`

Claim one pending message and move it to `assigned/`.

| Param | Type | Required | Notes |
|---|---|---:|---|
| `id` | string | no | Claim a specific message. Omit to claim next eligible. |
| `assignee` | string | no | Agent claiming the message. Default `agent:unknown`. |
| `recipient` | string | no | Filter to messages with this recipient. |
| `type` | string | no | Filter by message type. |

**Claim ordering (no ID specified):**
1. Filter to eligible (unclaimed, unblocked).
2. Sort by `priority` descending.
3. Break ties by `created_at` ascending (oldest first).

### `msgq__list`

List messages with optional filters.

| Param | Type | Required | Notes |
|---|---|---:|---|
| `state` | string | no | `pending` \| `assigned` \| `archive`. Default `pending`. |
| `recipient` | string | no | Filter by recipient. |
| `assignee` | string | no | Filter by assignee. |
| `type` | string | no | Filter by type. |
| `limit` | number | no | Max results. Default `100`. |

Returns an array of summary objects (no body, no full payload).

### `msgq__await`

Block until the filtered queue view changes or a minimum count is reached.

| Param | Type | Required | Notes |
|---|---|---:|---|
| `state` | string | no | State to watch. Default `pending`. |
| `recipient` | string | no | Filter. |
| `assignee` | string | no | Filter. |
| `type` | string | no | Filter. |
| `limit` | number | no | Max items returned when unblocked. |
| `min_count` | number | no | Return when filtered set size ≥ this value. |
| `timeout_ms` | number | no | Return after this many ms even if condition not met. `0` = wait indefinitely. |
| `poll_ms` | number | no | Polling interval. Default `500`. |

**Behaviour:**

- `min_count` absent + matches already exist → return immediately (`reason: items_available`).
- `min_count` absent + no matches → return when the filtered view changes (`reason: queue_changed`).
- `min_count` set + already satisfied → return immediately (`reason: min_count_reached`).
- `min_count` set + not yet satisfied → poll until satisfied or timeout.

**Common patterns:**

```
# Wait until 3 tasks are archived
msgq__await(state=archive, type=task, min_count=3)

# Wait for first message addressed to lead
msgq__await(state=pending, recipient=agent:lead, type=note, min_count=1, timeout_ms=30000)

# Wait for any change in the pending queue
msgq__await(state=pending)
```

### `msgq__update`

Update an assigned message and append a history event.

| Param | Type | Required | Notes |
|---|---|---:|---|
| `id` | string | yes | ID of the assigned message. |
| `assignee` | string | no | Must match the current assignee. |
| `status` | string | no | `assigned` or `in_progress`. |
| `payload` | object | no | Replaces the existing payload. |
| `body_append` | string | no | Text appended to the existing body. |
| `history_event` | string | no | Label for the history entry. Default `updated`. |

### `msgq__archive`

Move a message to `archive/` with a resolution.

| Param | Type | Required | Notes |
|---|---|---:|---|
| `id` | string | yes | Message ID. |
| `from_state` | string | no | `assigned` or `pending`. Auto-detected if omitted. |
| `assignee` | string | no | Must match current assignee for assigned items. |
| `resolution` | string | no | `completed` \| `failed` \| `cancelled`. Default `completed`. |
| `final_payload` | object | no | Replaces payload at archive time. |

When `resolution=completed`, the `task_completed` hook event fires. See [HOOKS.md](HOOKS.md).

### `msgq__bcast`

Fan out one payload to multiple recipients as individual pending messages.

| Param | Type | Required | Notes |
|---|---|---:|---|
| `recipients` | string[] | yes | List of recipient IDs. One message created per entry. |
| `sender` | string | no | |
| `type` | string | no | |
| `priority` | string | no | |
| `payload` | object | no | |
| `body` | string | no | |

---

## Safety Rules

- **Path confinement**: all operations stay under the current workspace root (host filesystem).
- **Ownership checks**: only the current assignee may `update` or `archive` an assigned item.
- **Idempotent archive intent**: repeated archive calls on already-archived items are safe to reason about.
- **Malformed entries**: parse failures should quarantine the item to `archive/invalid/` in strict flows.

---

## Minimal Workflow Example

```
lead:  msgq__append(id=task-1, type=task, recipient=agent:worker)
worker: msgq__await(state=pending, recipient=agent:worker, type=task)
worker: msgq__claim(recipient=agent:worker, type=task, assignee=agent:worker)
worker: msgq__update(id=task-1, assignee=agent:worker, status=in_progress)
        ... do work ...
worker: msgq__archive(id=task-1, assignee=agent:worker, resolution=completed)
lead:  msgq__await(state=archive, type=task, min_count=1)  → unblocks
```

---

## Shell Inspection

```bash
# Count items by state
echo "pending=$(ls .agent/msgq/pending/*.md 2>/dev/null | wc -l) \
      assigned=$(ls .agent/msgq/assigned/*.md 2>/dev/null | wc -l) \
      archive=$(ls .agent/msgq/archive/*.md 2>/dev/null | wc -l)"

# Dump a specific message
cat .agent/msgq/pending/task-frontend-001.md
```
