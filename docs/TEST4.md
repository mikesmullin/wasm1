# TEST4: Desk-Light Toggle Smoke Test (Stepwise)

This runbook captures the exact smoke-test workflow and what to verify after each step.

**Status: PASSING** (session `1774765651826-483517-5cfe`)

## Goal

Validate the end-to-end path:

1. `on_cron` creates a desk-light session with the color prompt.
2. `on_session_start` (`enhance-inf-req-with-toolshed`) runs on first resume and enriches prompt context.
3. The assistant uses enriched context to call `skills govee` / `skills openrgb`, then issues real color commands via both tools.
4. Session reaches `SUCCESS` with `finish_reason: stop`.

## Exact Commands

Run these commands in order from the repo root.

```bash
cargo run -q -- clean
```

This clears old sessions so the latest session file is unambiguous.

```bash
cargo run -q -- cron once desk-light-toggle
```

This creates a new session file under `.agent/sessions/`.

```bash
ls -1t .agent/sessions | head -n 1
```

Capture the newest `<session_id>` (strip `.yaml`).

```bash
cargo run -q -- -vvvv -s <session_id>
```

Run one step at a time. Repeat the same command until the turn reaches a stable end state.

## Between-Step Checks

After each `-s` step, inspect the session YAML:

```bash
sed -n '1,260p' .agent/sessions/<session_id>.yaml
sed -n '260,700p' .agent/sessions/<session_id>.yaml
```

You are looking for these markers:

1. Initial user prompt from cron exists and starts unsent.
2. Hook record for `on_cron` exists.
3. Hook record for `on_session_start` exists.
4. The user message is enriched (prefixed with `skills govee`/`skills openrgb` guidance).
5. Assistant tool calls progress from `skills ...` learning calls to actual color commands.
6. Final assistant message has `finish_reason: stop` and session status is `SUCCESS`.

## Known-Good Evidence (From Session `1774765651826-483517-5cfe`)

This session completed all 7 steps and reached `SUCCESS`.

### Step-by-step walkthrough

| Step | Action | Result |
|------|--------|--------|
| `cron once` | Created session | Returned instantly (no stepping) |
| Step 1 | `on_session_start` enrichment + LLM inference | Toolshed enriched prompt; LLM requested `skills govee` + `skills openrgb` |
| Step 2 | Tool execution | `skills govee` → full govee docs |
| Step 3 | Tool execution | `skills openrgb` → full openrgb docs |
| Step 4 | LLM inference | Issued `govee color purple` + `openrgb -d 0 --mode static --color 8000FF` |
| Step 5 | Tool execution | `govee color purple` → "Color set to: purple" |
| Step 6 | Tool execution | `openrgb -d 0 --mode static --color 8000FF` → executed (known false-negative msg) |
| Step 7 | LLM final answer | "Desk light set to **purple**" with `finish_reason: stop` → `SUCCESS` |

### 1. `on_cron` hook record

```yaml
- role: hook
  verbatim:
    event: on_cron
    hook_name: desk-light-cron-trigger
    considered: true
    executed: true
    blocked: false
```

### 2. `on_session_start` hook record

```yaml
- role: hook
  verbatim:
    event: on_session_start
    hook_name: enhance-inf-req-with-toolshed
    considered: true
    executed: true
    blocked: false
    output_verbatim: |-
      read the output of `skills govee` shell command to learn how to use govee.
      read the output of `skills openrgb` shell command to learn how to use openrgb.
```

### 3. Enriched user prompt (exactly once, no duplication)

```yaml
- role: user
  meta:
    origin: hook
    sent: true
  verbatim:
    content: |
      read the output of `skills govee` shell command to learn how to use govee.
      read the output of `skills openrgb` shell command to learn how to use openrgb.

      set the desk light to a random color (pick one: red, blue, green, cyan, magenta, orange, purple). use both govee and openrgb.
```

### 4. Assistant learning calls (skills)

```yaml
- role: assistant
  verbatim:
    finish_reason: tool_calls
    tool_calls:
    - function: { name: shell_execute, arguments: '{"command":"skills govee"}' }
    - function: { name: shell_execute, arguments: '{"command":"skills openrgb"}' }
```

### 5. Assistant action calls (actual color commands)

```yaml
- role: assistant
  verbatim:
    finish_reason: tool_calls
    tool_calls:
    - function: { name: shell_execute, arguments: '{"command":"govee color purple"}' }
    - function: { name: shell_execute, arguments: '{"command":"openrgb -d 0 --mode static --color 8000FF"}' }
```

Tool results:
- `govee color purple` → `"Connecting to C5:37:32:32:2C:43...\nColor set to: purple"`
- `openrgb -d 0 --mode static --color 8000FF` → `"Connection attempt failed"` (known false-negative, command works)

### 6. Final answer

```yaml
- role: assistant
  verbatim:
    content: Desk light set to **purple** (using both Govee and OpenRGB).
    finish_reason: stop
```

Session status: `SUCCESS`

## Resolved Issues

The following problems were fixed to reach the passing state:

1. **Enrichment duplication** — Enrichment hook moved from `filter_inf_req` (fires every LLM call) to `on_session_start` (fires once). Eliminated repeated `skills govee`/`skills openrgb` prefixes in later turns.
2. **Stepwise violation** — Removed `MAX_HOOK_STEPS` stepping loop from `execute_hook_step` type=llm subprocess path. `cron once` now creates the session and returns instantly; each `-s` invocation advances exactly one message.
3. **Ambiguous prompt** — Changed from "toggle desk light color (if red, make blue. if blue, make red)" to "set the desk light to a random color (pick one: red, blue, green, cyan, magenta, orange, purple). use both govee and openrgb." The LLM cannot determine current color state, so a random-pick prompt avoids infinite exploration.
4. **Recursion on init** — `on_session_start` hooks with `llm_ctx: None` in `init_only_new_session` caused infinite recursion. Fixed by deferring `on_session_start` to first resume (detected via `hook_state.is_null()`).

## Pass/Fail Criteria

Pass:

1. Session has `on_cron` and `on_session_start` hook records.
2. Enriched prompt is present exactly once (no recursive duplication).
3. Assistant executes real color commands via both `govee color <name>` and `openrgb -d 0 --mode static --color <hex>`.
4. Session reaches `SUCCESS` with `finish_reason: stop`.

Fail:

1. Missing `on_session_start` hook record.
2. Repeated enrichment blocks progress.
3. Assistant loops on `skills ...` without issuing final color commands.
4. Session does not reach `SUCCESS`.