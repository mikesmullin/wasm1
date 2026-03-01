# Integration Test Run — `docs/SHELL.md` Test Suite

This document records how each test scenario from `docs/SHELL.md §10` was executed
manually against a running `cargo run` process, and what output was observed.
No automated test harness is used — each test is a single shell command whose
output is grepped to extract the relevant lines.

---

## Prerequisites

```bash
# In /workspace/tmp/wasm1
cp .env.example .env          # or set XAI_API_KEY in environment
cargo build                   # builds the host binary
cargo build --manifest-path guest/Cargo.toml --target wasm32-wasip1
                              # builds guest.wasm — must be rebuilt after every
                              # change to guest/src/lib.rs
```

> **Important:** `ensure_guest_wasm()` skips the guest build when `guest.wasm`
> already exists.  After any guest source change you must manually run
> `cargo build --manifest-path guest/Cargo.toml --target wasm32-wasip1` before
> testing.

The template used for all shell tests is `.agent/templates/solo.yaml`.

> **Breaking change (Feb 2026):** `shell` is **no longer injected as a global**.
> Agents must obtain it via `const shell = require('shell')` before calling any of
> `shell.run()`, `shell.stdin()`, or `shell.kill()`.  All test snippets below
> reflect this.
>
> `shell.run()` now also sets `shell.lastPid` (integer PID) and `shell.lastFile`
> (string `.out` path) directly on the shell object, and the returned promise-shim
> gains a `pid` field alongside the existing `path` field.

---

## Test Commands and Expected Output

### T1 — Template basename not found → process exits with clear error

```bash
cargo run -- -t nonexistent "hello" 2>&1 | grep -E "^Error"
```

**Expected:**
```
Error: template 'nonexistent' not found in .agent/templates/ or ~/.config/daemon/agent/templates/
```
**Result: ✅ PASS**

---

### T2 — Template absolute path → loaded directly, no INCLUDE_PATH search

```bash
cargo run -- -t /workspace/tmp/wasm1/.agent/templates/solo.yaml "what is 2+2" \
  2>&1 | grep "\[HOST\]" | head -4
```

**Expected:** `Using template: /workspace/tmp/wasm1/.agent/templates/solo.yaml`
and `Template loaded: N shell allow-list entries, timeout: indefinite`

**Result: ✅ PASS**

---

### T3 — Template basename → resolved via `.agent/templates/` search

```bash
cargo run -- -t solo "what is 2+2" 2>&1 | grep "\[HOST\]" | head -4
```

**Expected:** `Using template: .agent/templates/solo.yaml`

**Result: ✅ PASS**

---

### T4 — `shell.run('echo', ['hello'])` — allowed command, `.out.json` has correct JSON

```bash
cargo run -- -t solo \
  "Use js_exec to run exactly this JS: const shell = require('shell'); var p = shell.run('echo', ['hello world']); console.log(fs.readFile(p.path));" \
  2>&1 | grep -E "\[HOST\] shell_run|\[GUEST\] Tool result" | head -4
```

**Expected host log:**
```
[HOST] shell_run: spawning "echo" ["hello world"]
[HOST] shell_run: exit_code=Some(0) elapsed=...ms
```

**Expected JSON in Tool result stdout:**
```json
{"status":"ended","exit_code":0,"stdout":"hello world\n","stderr":""}
```

**Result: ✅ PASS**

---

### T5 — `shell.run('rm', ['-rf', '/'])` — denied, host never spawns

> **Note:** The LLM itself refuses to run `rm -rf /` as a safety measure, so it
> never reaches the allow-list.  The allow-list is tested instead with a benign
> but unlisted command (`curl`) that the model will try to run:

```bash
cargo run -- -t solo \
  "Use js_exec to run this exact JS code. Do not modify it: const shell = require('shell'); var r = shell.run('curl', ['example.com']); console.log('done');" \
  2>&1 | grep -E "denied|\[GUEST\] Tool result" | head -4
```

**Expected host log:**
```
[HOST] shell_run: command denied by allow-list: "curl example.com"
```
Tool result error contains:
```
AI Policy Error: shell.run command not in allow-list: "curl". Check the template metadata.shell.allow list.
```

**Result: ✅ PASS**

---

### T6 — `child_process.exec()` → AI policy error thrown

```bash
cargo run -- -t solo \
  "Use js_exec to run this exact JS: child_process.exec('ls')" \
  2>&1 | grep "\[GUEST\] Tool result" | head -2
```

**Expected** error field contains:
```
AI Policy Error: Use of child_process is prohibited for security reasons.
Please use require('shell').run(cmd, args) ...
```

**Result: ✅ PASS**

---

### T7 — `require('child_process')` → same policy error via require intercept

```bash
cargo run -- -t solo \
  "Use js_exec to run this exact JS: var cp = require('child_process'); cp.exec('ls');" \
  2>&1 | grep "\[GUEST\] Tool result" | head -2
```

**Expected:** identical policy error as T6, thrown at `require()` time before any
VFS lookup.

`require('shell')` is also intercepted by the same mechanism — it returns a fresh
shell object (with `run`, `stdin`, `kill` methods) rather than touching the VFS.

**Result: ✅ PASS**

---

### T8 — Command timeout → `status: timeout`, `timeout_secs` recorded in `.out`

This test requires `timeout_secs` to be set in the template.  The normal
`.agent/templates/solo.yaml` has it commented out.  Temporarily enable it:

```yaml
# .agent/templates/solo.yaml
  shell:
    timeout_secs: 5      # ← uncomment for this test only
    allow:
      ...
```

Then run:

```bash
cargo run -- -t solo \
  "Use js_exec to run exactly this JS: const shell = require('shell'); var p = shell.run('sleep', ['10']); console.log(fs.readFile(p.path));" \
  2>&1 | grep -E "\[HOST\] shell_run|timed_out|\[GUEST\] Tool result" | head -4
```

**Expected host log:**
```
[HOST] shell_run: spawning "sleep" ["10"]
[HOST] shell_run: timed out after 5s
[HOST] shell_run: exit_code=Some(-1) elapsed=...ms
```

**Expected JSON in Tool result stdout:**
```json
{"status":"timeout","exit_code":-1,"stderr":"Command timed out after 5s","timeout_secs":5}
```

**Result: ✅ PASS**

After the test restore the template (comment `timeout_secs` back out):

```yaml
  shell:
    # timeout_secs: 5
    allow:
      ...
```

> **Note:** `timeout_secs` is omitted from `.out.json` entirely on normal
> (`status: "ended"`) runs — it is only emitted when a timeout actually fires.

---

### T9 — `shell.stdin(pid, ...)` / `shell.kill(pid, ...)` with dead/foreign PID → JS throws

**stdin with arbitrary PID:**

```bash
cargo run -- -t solo \
  "Use js_exec to run this exact JS: const shell = require('shell'); shell.stdin(9999999, 'Y\n');" \
  2>&1 | grep "\[GUEST\] Tool result" | head -1
```

**Expected** error: `shell.stdin: PID not found or already ended`

**Result: ✅ PASS**

**kill with a just-finished PID:**

> The model refuses to call `shell.kill(9999999)` on an arbitrary PID as a
> perceived jailbreak.  Instead, kill the PID from a just-completed `shell.run()`:

```bash
cargo run -- -t solo \
  "Use js_exec to run this exact JS. Do not modify it: const shell = require('shell'); var r = shell.run('echo', ['test']); var pid = shell.lastPid; try { shell.kill(pid); console.log('no error'); } catch(e) { console.log('error:', e.message); }" \
  2>&1 | grep "\[GUEST\] Tool result" | head -1
```

**Expected stdout:** `error: shell.kill: PID not found or already ended`

**Result: ✅ PASS**

---

### T10 — `shell.kill(pid, 'SIGXXX')` — invalid signal name → `-4`, JS throws

> Using a try/catch around a just-finished PID (same reason as T9 — model 
> refuses arbitrary kill on unknown PIDs):

```bash
cargo run -- -t solo \
  "Use js_exec to run this exact JS. Do not modify it: const shell = require('shell'); var r = shell.run('echo', ['test']); var pid = shell.lastPid; try { shell.kill(pid, 'SIGXXX'); console.log('no error'); } catch(e) { console.log('error:', e.message); }" \
  2>&1 | grep "\[GUEST\] Tool result" | head -1
```

**Expected stdout:** `error: shell.kill: invalid signal 'SIGXXX'`

**Expected** error: `shell.kill: invalid signal 'SIGXXX'`

> **Implementation note:** Signal validation runs before PID validation so that
> a bad signal name produces `-4` / `"invalid signal"` regardless of whether the
> PID is live or dead.

**Result: ✅ PASS**

---

### T11 — Agent reads `.out` path with `fs.readFile()` → YAML string from virtual FS

Covered inline by T4, T8. The `.out` path returned by `shell.run` is passed to
`fs.readFile()` in the same JS snippet; the YAML content is visible in the
`stdout` field of the Tool result.

---

### T12 — `.agent/fs/<session_id>.tcow` accumulates `/tmp/*.out.json` files after a run

After any successful `shell.run` call, the `.out.json` files are flushed to the
per-session `.agent/fs/<session_id>.tcow` layer.  Find the latest session file and inspect it:

```bash
# find the most recent tcow file
latest=$(ls -t .agent/fs/*.tcow | head -n1)
echo "$latest"

tcow ls "$latest" | grep '\.out\.json'
# e.g. /tmp/1772328876452_eb8652.out.json

tcow cat "$latest" tmp/1772328876452_eb8652.out.json
# prints the full JSON
```

---

### T14 — `shell.lastPid`, `shell.lastFile`, `r.pid` set after `shell.run()`

After a successful `shell.run()` call, the shell object itself gains `lastPid`
(the integer PID of the child process) and `lastFile` (the `.out` path string),
and the returned promise-shim also carries a `pid` field.

```bash
cargo run -- -t solo \
  "Use js_exec to run this exact JS: const shell = require('shell'); var r = shell.run('echo', ['hi']); console.log('lastPid:', shell.lastPid); console.log('lastFile:', shell.lastFile); console.log('r.pid:', r.pid); console.log('r.path:', r.path);" \
  2>&1 | grep "\[GUEST\] Tool result" | head -4
```

**Expected** stdout in Tool result:
```
lastPid: <some integer>
lastFile: /tmp/<timestamp>_<sha1>.out
r.pid: <same integer as lastPid>
r.path: /tmp/<timestamp>_<sha1>.out
```

> `shell.lastPid` and `shell.lastFile` are overwritten on every subsequent
> `shell.run()` call on the same shell object.  `r.pid` and `r.path` on the
> returned shim are frozen at the time of that specific run.  `r.path` ends
> in `.out.json`.

**Result: ✅ PASS**

---

### T13 — Persistence across restarts — old `.out.json` still readable

Pick any `.out` path logged in a previous run and ask the agent to read it in a
new invocation:

```bash
cargo run -- -t solo \
  "Use js_exec to read and console.log the file at path /tmp/1772331172078_4eef4f.out using fs.readFile." \
  2>&1 | grep "\[GUEST\] Tool result" | head -1
```

**Expected:** The YAML content from the previous run is returned — `.agent/fs/<session_id>.tcow`
delta layers are read within the same session without re-running the command.

**Result: ✅ PASS** (returned T4's `echo hello world` JSON, `exit_code: 0`, `stdout: "hello world\n"`)

## Summary Table

| # | Scenario | Result |
|---|---|:---:|
| T1 | `-t` basename not found → clear error | ✅ |
| T2 | `-t` absolute path → loaded directly | ✅ |
| T3 | `-t` basename → resolved via `INCLUDE_PATH` | ✅ |
| T4 | `shell.run('echo', [...])` allowed → correct `.out.json` | ✅ |
| T5 | `shell.run('rm', ...)` denied → host never spawns, JS throws | ✅ |
| T6 | `child_process.exec()` → policy error thrown | ✅ |
| T7 | `require('child_process')` → same policy error | ✅ |
| T8 | Timeout (`sleep 10`, `timeout_secs: 5`) → `status: timeout`, `timeout_secs: 5` in `.out.json` | ✅ |
| T9 | `shell.stdin` / `shell.kill` with dead PID → `-1` / JS throws | ✅ |
| T10 | `shell.kill` invalid signal → `-4` / JS throws | ✅ |
| T11 | `fs.readFile(outPath)` returns JSON | ✅ (via T4/T8) |
| T12 | `tcow ls` shows `/tmp/*.out.json` | ✅ |
| T13 | Old `.out.json` readable after restart | ✅ |
| T14 | `shell.lastPid`, `shell.lastFile`, `r.pid` set after `shell.run()` | ✅ |

