# PRD: `shell.run()` — Sandboxed Shell Execution for the Wasm Agent

**Version** — 1.0
**Date** — February 2026
**Status** — Draft

---

## 1. Overview

This document describes the design for allowing the AI agent to execute shell commands in a controlled, allow-listed manner from within the Boa JS interpreter running inside the Wasm guest.

The design has two interlocking parts:

1. **`child_process` mock** — Any attempt by the LLM to use Node-style `child_process.*` APIs is intercepted and returns an AI-readable policy error, nudging the model toward the approved API.
2. **`shell.run(cmd, args)` host function** — The approved shell execution API. Commands are validated against a per-agent regexp allow-list defined in a YAML template. Output is written to the virtual `.tcow` filesystem as a YAML file; the agent retrieves results by reading that file path with `fs.readFile()`.

---

## 2. CLI Change: `-t <template>` Flag

The host binary (`cargo run`) gains a new flag:

```
cargo run -- -t solo "your prompt here"
cargo run -- -t /absolute/path/to/my.yaml "your prompt here"
```

| Flag | Type | Default | Description |
|---|---|---|---|
| `-t`, `--template` | basename or absolute path | *(none — shell features disabled)* | Agent template to load. |

### Template resolution

If the value is an **absolute path**, it is used directly. Otherwise the value is treated as a **basename** (with or without `.yaml` extension) and the following `INCLUDE_PATH` is searched in order, stopping at the first match:

1. `.agent/templates/<basename>.yaml` — project-local templates
2. `~/.config/daemon/agent/templates/<basename>.yaml` — user-global templates

If no file is found the process exits with an error.

The template must be valid `daemon/v1` format as specified in `docs/TEMPLATE.md`. The template is loaded and parsed by the host at startup before the Wasm module is instantiated. The `metadata.shell.allow` list from the template is compiled into `Vec<regex::Regex>` and stored in `HostState`.

---

## 3. Template Format

Templates must be valid `daemon/v1` format as specified in `docs/TEMPLATE.md`. wasm1 reads the standard fields (`metadata.description`, `metadata.model`, `spec.system_prompt`, etc.) and additionally reads the `metadata.shell` extension key, which is a wasm1-specific runtime-behavior config:

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: Solo task runner
  model: xai:grok-4-fast-reasoning

  # wasm1 extension — shell execution policy
  shell:
    # Ordered list of regexp patterns. First match wins.
    # Matched against the full command string: "cmd arg1 arg2 ..."
    allow:
      - '^echo\b'
      - '^ls\b'
      - '^cat\s+[^/]'           # cat only relative paths
      - '^git\s+(status|log|diff)\b'
      - '^cargo\s+(build|test|check)\b'
    # If no allow entry matches, the command is rejected.

spec:
  system_prompt: |
    You are a general-purpose shell agent. Use shell.run() for system tasks.
```

`metadata.shell` is placed under `metadata` rather than `spec` because it is runtime/behavior configuration (consistent with `metadata.tools`, `metadata.heartbeat`, etc. in the `daemon/v1` schema), not prompt content.

### `metadata.shell` schema

| Field | Type | Required | Description |
|---|---|---:|---|
| `allow` | `string[]` | no | Ordered list of regexp patterns. Matched against `"cmd arg1 arg2 ..."` (space-joined). First match permits the command. If the list is absent or empty, **all commands are denied**. |

---

## 4. `child_process` Policy Mock (Boa-level)

Before user code runs inside `run_js_in_boa()`, a `child_process` module shim is injected into the Boa context. Every callable on it (`exec`, `execSync`, `spawn`, `spawnSync`, `execFile`, `fork`) is a native function that:

1. Writes to host stderr via `host_log` (prefixed `[STDERR]`):

   ```
   AI Policy Error: Use of child_process.exec() is prohibited for security reasons.
   Please use require('shell').run(cmd, args) which returns a string path to a YAML
   file in the virtual filesystem containing { exit_code, stdout, stderr }.
   ```

2. Throws a JS `Error` so execution stops and the error surfaces in the `JsExecResult.error` field, which the agent loop feeds back to the LLM.

A top-level `require('child_process')` call is intercepted by the guest-side `require()` implementation: if the resolved path is the literal string `child_process` (no file in `.tcow` by that name), the policy error is thrown before any file lookup.

```
agent sees error: "AI Policy Error: Use of child_process.exec() is prohibited..."
  → LLM re-plans using shell.run()
```

---

## 5. `shell.run()`, `shell.stdin()`, and `shell.kill()` — The Approved APIs

### 5.1 JS surface (Boa)

```js
// Both forms work:
const p = shell.run(cmd, args);   // returns a Promise<string>
const p = require('shell').run(cmd, args);
```

- `cmd` — executable name or path (e.g. `"git"`)
- `args` — array of string arguments; optional, defaults to `[]`
- **Returns a `Promise<string>`** that resolves to the `.out` file path when the child process exits.

Because Boa does not have a native async runtime, the Promise is a lightweight shim object with one method: `.then(fn)`. The `.then` callback is called synchronously (the host blocks on process completion before returning). The Promise shape is provided for forwards compatibility and ergonomic familiarity:

```js
// Awaiting (blocks until process exits):
const outPath = await shell.run('git', ['status', '--short']);
const result = fs.readFile(outPath);
console.log(result);
```

**The `.out` file is written to the virtual FS immediately** when `shell.run()` is called — before the process exits — with `state: running`. This allows the agent to skip `await` and poll the file repeatedly for interactive workflows:

```js
// Non-awaiting / polling pattern:
const outPath = shell.run('some-interactive-cmd', []).path;  // .path available immediately
// ... do other work ...
let out;
do {
  out = fs.readFile(outPath);
} while (out.includes('state: running'));
console.log('done:', out);
```

#### `shell.stdin(pid, sendkeys)`

Sends a string to the stdin of a running child process. Useful for answering interactive prompts:

```js
const outPath = shell.run('some-tool', ['--interactive']).path;
shell.stdin(pid, 'Y\n');   // answer a y/n prompt
// poll outPath until state: ended
```

- `pid` — the integer PID from the `.out` file's `pid:` field
- `sendkeys` — string to write to the process's stdin (raw bytes; use `\n` for Enter)
- Returns `undefined`; throws if PID is not a child of the current agent session or is already ended.

> **Session-scoped:** `shell.stdin` (and `shell.kill` below) only accept PIDs that were created by `shell.run` in the current session. Arbitrary host PIDs are rejected with a JS error.

#### `shell.kill(pid, [signal])`

Sends a signal to a running child process, terminating or interrupting it:

```js
const outPath = shell.run('some-long-running-cmd', []).path;
// ... decide it's no longer needed
shell.kill(pid);          // default: SIGTERM
shell.kill(pid, 'SIGKILL');  // forceful kill
shell.kill(pid, 'SIGINT');   // Ctrl-C equivalent
```

- `pid` — the integer PID from the `.out` file's `pid:` field
- `signal` — optional signal name string; one of `SIGTERM` (default), `SIGKILL`, `SIGINT`, `SIGHUP`
- Returns `undefined` on success; throws if PID is not a session child, is already ended, or signal name is unrecognised.
- After a successful kill the `.out` file is updated with `state: killed` and the actual exit code.

### 5.2 Allow-list check (host-side)

When the guest calls the `shell_run` host function, the host:

1. Reconstructs the full command string: `format!("{cmd} {joined_args}")` (space-joined).
2. Iterates the `metadata.shell.allow` regexp list from the loaded template in order.
3. If **no** pattern matches → rejects with a JS error containing the policy message; does **not** execute anything.
4. If a pattern matches → proceeds to execution.

Matching uses Rust's `regex` crate, case-sensitive, against the full `"cmd arg1 arg2 ..."` string.

### 5.3 Host execution

On allow-list pass:

1. Generates the output filename **before spawning**: `/tmp/{unix_time_ms}_{6_char_sha1}.out`
   - `unix_time_ms` — `SystemTime::now()` milliseconds since epoch
   - `6_char_sha1` — first 6 hex chars of `sha1("{cmd} {args} {unix_time_ms}")`
2. Writes an **initial** YAML snapshot to `pending_writes` immediately (state `running`):

```yaml
pid: 12345
state: running
cmd: some-interactive-cmd
args: []
started_ms: 1709123456789
stdout: ""
stderr: ""
exit_code: ~
elapsed_ms: ~
```

3. Spawns the process via `std::process::Command` with `stdout(Stdio::piped())` and `stderr(Stdio::piped())`.
4. Stores the `Child` handle in `HostState::running_processes: HashMap<u32, Child>` keyed by PID.
5. Waits for the process to exit (blocking, up to `SHELL_TIMEOUT_SECS`), reading stdout/stderr as it completes.
6. Updates the `.out` entry in `pending_writes` with the final YAML:

```yaml
pid: 12345
state: ended
cmd: git
args:
  - status
  - --short
started_ms: 1709123456789
exit_code: 0
stdout: |
  M src/main.rs
stderr: ""
elapsed_ms: 124
```

7. Removes the PID from `running_processes`.
8. Returns the `.out` path string to the guest.

---

## 6. Architecture Diagram

```
  Boa JS context (inside Wasm guest)
  ├── console.log               (captured stdout)
  ├── fs.readFile(path)         (reads from .tcow)
  ├── fs.writeFile(p, d)        (writes to .tcow)
  ├── fs.readdir(dir)           (lists .tcow)
  ├── require(path)             (evals .tcow JS module)
  │     ├── 'child_process'  → policy error mock (no execution)
  │     └── 'shell'          → returns shell object (same as global)
  ├── shell.run(cmd, args)   ──────────────────────────────────────────┐
  │                                                       host fn:      │
  │                                                       shell_run     ▼
  │                                                   ┌────────────────────────────────┐
  │                                                   │ 1. allow-list regexp check     │
  │                                                   │    (metadata.shell.allow)      │
  │                                                   │ 2. write initial .out (running)│
  │                                                   │ 3. spawn child process         │
  │                                                   │ 4. store Child in HashMap<pid> │
  │                                                   │ 5. wait + capture output       │
  │                                                   │ 6. update .out (ended)         │
  │                                                   │ 7. return .out path            │
  │                                                   └────────────────────────────────┘
  │                                                               │
  ├── shell.stdin(pid, keys)  ────────────────────────────────────┼──────┐
  │                                                   host fn:     │      │
  │                                                   shell_stdin  ▼      ▼
  │                                                ┌────────────────────────────────┐
  │                                                │ validate pid ∈ running_processes│
  │                                                │ write keys to child stdin       │
  │                                                └────────────────────────────────┘
  │
  └── shell.kill(pid, signal)  ───────────────────────────────────────────┐
                                                              host fn:     │
                                                              shell_kill   ▼
                                                   ┌────────────────────────────────┐
                                                   │ validate pid ∈ running_processes│
                                                   │ send signal to child process    │
                                                   │ update .out  state: killed      │
                                                   └────────────────────────────────┘
                                                               │
                                                      agent.tcow  (/tmp/*.out)
```

---

## 7. Host Changes

### 7.1 `HostState` additions

```rust
struct HostState {
    // ... existing fields ...

    /// Compiled allow-list from template metadata.shell.allow.
    /// Empty vec = all shell commands denied.
    shell_allow: Vec<regex::Regex>,

    /// Wall-clock timeout for shell commands in seconds.
    shell_timeout_secs: u64,  // default: 30

    /// Live child processes spawned this session, keyed by PID.
    /// Cleared when process exits; consulted by shell_stdin.
    running_processes: HashMap<u32, std::process::Child>,
}
```

### 7.2 New host functions

#### `shell_run`

Registered as `"host"` / `"shell_run"`:

```rust
linker.func_wrap("host", "shell_run",
    |mut caller: Caller<'_, HostState>,
     cmd_ptr: i32, cmd_len: i32,      // UTF-8 command name
     args_ptr: i32, args_len: i32,    // JSON array of string args
     out_ptr: i32, out_cap: i32|      // output buffer receives .out path
     -> i32 { ... })?;
```

Args are passed as a JSON array string (`["status","--short"]`) to avoid a variadic ABI.

Return value: byte count of path written, or negative error sentinel.

#### `shell_stdin`

Registered as `"host"` / `"shell_stdin"`:

```rust
linker.func_wrap("host", "shell_stdin",
    |mut caller: Caller<'_, HostState>,
     pid: i32,                         // PID of running child
     keys_ptr: i32, keys_len: i32|     // raw bytes to write to child stdin
     -> i32 { ... })?;  // 0 on success, negative on error
```

Error codes: `-1` PID not found / already ended, `-2` write failed, `-3` not a child of this session.

#### `shell_kill`

Registered as `"host"` / `"shell_kill"`:

```rust
linker.func_wrap("host", "shell_kill",
    |mut caller: Caller<'_, HostState>,
     pid:        i32,
     sig_ptr:    i32, sig_len: i32|   // signal name UTF-8 string; empty = "SIGTERM"
     -> i32 { ... })?;  // 0 on success, negative on error
```

Accepted signal names: `SIGTERM`, `SIGKILL`, `SIGINT`, `SIGHUP`. Any other value returns `-4` (invalid signal). Uses `libc::kill` on Unix.

Error codes: `-1` PID not found / already ended, `-2` kill syscall failed, `-3` not a child of this session, `-4` invalid signal name.

After a successful kill, the `.out` entry in `pending_writes` is updated: `state: killed`, `exit_code` set to the actual exit code collected by a non-blocking `waitpid`.

### 7.3 New Cargo dependencies

```toml
regex      = "1"
serde_yaml = "0.9"
sha1       = "0.10"
```

---

## 8. Guest Changes

### 8.1 New host imports

```rust
#[link(wasm_import_module = "host")]
unsafe extern "C" {
    // ... existing imports ...

    fn shell_run(
        cmd_ptr:  i32, cmd_len:  i32,   // command name
        args_ptr: i32, args_len: i32,   // JSON string array
        out_ptr:  i32, out_cap:  i32,   // receives .out path
    ) -> i32;  // bytes written, or negative error

    fn shell_stdin(
        pid:       i32,
        keys_ptr:  i32, keys_len: i32,  // raw bytes to send
    ) -> i32;  // 0 on success, negative error

    fn shell_kill(
        pid:     i32,
        sig_ptr: i32, sig_len: i32,     // signal name (empty = SIGTERM)
    ) -> i32;  // 0 on success, negative error
}
```

### 8.2 Boa injections in `run_js_in_boa()`

```
child_process  (mock object — always throws)
  .exec / .execSync / .spawn / .spawnSync / .execFile / .fork
  → NativeFunction throws JS Error with AI policy message

shell  (real object)
  .run(cmd, args)
      → calls shell_run host fn
      → immediately returns a Promise-shim:
           { path: "/tmp/...", then: fn(cb) { cb(path) } }
         (the host has already waited for process exit before returning,
          so .then is synchronous; await works naturally in Boa)
  .stdin(pid, sendkeys)
      → calls shell_stdin host fn
      → validates pid ∈ session running_processes; throws otherwise
      → returns undefined
  .kill(pid, signal?)
      → calls shell_kill host fn
      → validates pid ∈ session running_processes; throws otherwise
      → updates .out state to 'killed'
      → returns undefined

require('child_process')  → same policy error as mock methods above
require('shell')          → returns the shell object (same reference as global)
```

---

## 9. Security Properties

| Property | Mechanism |
|---|---|
| LLM cannot escape the allow-list | `metadata.shell.allow` is compiled into host `HostState`; guest has no access to it |
| LLM cannot bypass via `child_process` | Mock throws before any syscall; no real `child_process` binding exists in Boa |
| LLM cannot send stdin to arbitrary PIDs | `shell_stdin` validates PID against `running_processes` — only PIDs created by `shell.run` in this session |
| LLM cannot kill arbitrary host processes | `shell_kill` validates PID against the same session-scoped `running_processes` map before issuing any signal |
| LLM cannot read arbitrary host files | `fs.*` only reads from `.tcow` virtual FS |
| Command output is auditable | Every `/tmp/*.out` file is flushed to `agent.tcow`; full history is permanently queryable via `tcow cat` |
| Timeout limits runaway commands | `shell_timeout_secs` hard-kills the child and sets `state: timed_out` in the `.out` file |
| No ambient WASI authority | Unchanged — guest still has no direct WASI fs/net access |

---

## 10. Testing Plan

| Scenario | Expected result |
|---|---|
| Agent uses `child_process.exec()` | Policy error thrown; LLM receives error and re-plans |
| Agent uses `require('child_process')` | Same policy error via `require` intercept |
| `shell.run('echo', ['hello'])` with `^echo\b` in allow-list | Returns Promise; `.out` has `state: ended`, `exit_code: 0`, `stdout: "hello\n"` |
| `shell.run('rm', ['-rf', '/'])` with no matching allow entry | Host rejects before spawn; JS error surfaced to LLM |
| `-t` given a basename; file exists in `.agent/templates/` | Resolved and loaded; INCLUDE_PATH search used |
| `-t` given an absolute path | File loaded directly, no search |
| `-t` basename not found in any INCLUDE_PATH dir | Process exits with clear error |
| Command times out (> `shell_timeout_secs`) | Child killed; `.out` has `state: timed_out`, `exit_code: -1` |
| Agent polls `.out` while process is running | `state: running` returned; updates to `state: ended` after process exits |
| `shell.stdin(pid, 'Y\n')` to a running process | Bytes written to child stdin; next `fs.readFile(outPath)` reflects new stdout |
| `shell.stdin(pid, ...)` with dead or foreign PID | Returns `-1`; JS throws |
| `shell.kill(pid)` (SIGTERM) on a running process | Child receives SIGTERM; `.out` updated to `state: killed` |
| `shell.kill(pid, 'SIGKILL')` on a running process | Child forcefully terminated; `.out` updated to `state: killed` |
| `shell.kill(pid, 'SIGINT')` on a running process | Child receives SIGINT (Ctrl-C); `.out` updated to `state: killed` |
| `shell.kill(pid, ...)` with dead or foreign PID | Returns `-1`; JS throws |
| `shell.kill(pid, 'SIGXXX')` with invalid signal | Returns `-4`; JS throws |
| Agent reads output path with `fs.readFile()` | Returns YAML string from `.tcow /tmp/*.out` entry |
| `tcow ls agent.tcow` after shell command | `/tmp/*.out` file(s) visible in union view |
| Across agent restarts, old `/tmp/*.out` still readable | `.tcow` persistence via delta layers |
