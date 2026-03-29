# SHELL.md: `shell_execute` Runtime Contract

Version: 2.0
Status: Current implementation
Scope: Host shell emulator + template policy + session exec mode

---

## 1. Overview

`wasm1` no longer executes JavaScript shell APIs (`shell.run`, `child_process`, or Boa-backed shell wrappers).

Shell execution is now handled by a host-side tool named `shell_execute` that accepts one command string:

- Tool name: `shell_execute`
- Input schema: `{ "command": "..." }`
- Execution model: parse -> policy check -> execute in constrained emulator

The same emulator can also be invoked directly from CLI without adding chat history:

- `wasm1 -s <session_id> exec "<command>"`

---

## 2. Command Surface

### 2.1 Tool call shape

The assistant must call:

```json
{"command":"skills"}
```

through tool `shell_execute`.

### 2.2 CLI exec shape

```bash
wasm1 -s 1774737544123-248543-3ad5 exec "skills"
wasm1 -s 1774737544123-248543-3ad5 exec "echo hello > out.txt"
```

`exec` uses the session `.tcow` filesystem and current shell metadata, then updates `metadata.cwd` in the session YAML.

---

## 3. Policy and Template Configuration

Shell policy is configured in templates and compiled by the host at load time.

### 3.1 Location and merge behavior

Allow-list regexes come from:

1. Inline tool policy under `metadata.tools` for `shell_execute` (preferred)
2. `metadata.shell.allow` (legacy; still supported)

Both sources are merged into one allow-list (`OR` behavior). Prefer inline tool policy for new templates.

### 3.2 Current preferred template style

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  tools:
    - shell_execute:
        allow:
          - '^skills'
          - '^echo'
          - '^cat'
          - '^ls'
          - '^pwd$'
          - '^cd($|\s)'
        auto_approve:
          - '^skills($|\s)'
          - '^echo($|\s)'
          - '^cat($|\s)'
          - '^ls($|\s)'
          - '^pwd$'
          - '^cd($|\s)'
spec:
  system_prompt: |
    Use shell_execute with {"command":"..."}.
```

Notes:

- `tools` entries may be simple strings (enable only) or single-key mappings with config.
- Inline `allow` must be a list of regex strings.
- Inline `auto_approve` must be a list of regex strings.
- If no allow regex matches, execution is rejected before running.

---

## 4. Shell Emulator Behavior

## 4.1 Parsing model

Input is parsed as one simple command with optional quoting and optional single `>` redirection.

Supported:

- Whitespace tokenization
- Single quotes and double quotes
- Backslash escaping (outside single quotes)
- One trailing `>` redirection target

Semicolons are sanitized into spaces before parsing.

## 4.2 Unsupported syntax (returns policy guidance)

The emulator rejects advanced shell syntax with explicit messages:

- Subshell: `$()`
- Logical chaining: `&&`
- Pipes: `|`
- Heredoc: `<<`
- Append redirect: `>>`
- Backticks: `` `...` ``
- Variable export: `export VAR=...`
- Variable substitution: `$VAR`
- Background execution: `&`

Typical response prefix:

- `Policy Error: ...` for parser-level policy guidance
- `AI Policy Error: shell_execute command not in allow-list: "<program>". Check template metadata.shell.allow.` for allow-list denial

## 4.3 Built-in commands

The emulator implements these built-ins directly:

- `pwd`
- `cd [path]`
- `echo ...`
- `cat <file-or-glob>`
- `ls [dir-or-glob]`

Behavior details:

- `pwd` prints current shell directory plus newline.
- `cd` accepts zero or one arg; zero means `/`.
- `cat` requires exactly one file/glob argument.
- `ls` accepts zero or one argument.
- `cat`/`ls` support `*` and `?` glob matching.

## 4.4 External commands

If command is not one of the built-ins, host spawns it via `std::process::Command`.

- stdout and stderr are combined into one returned string
- optional timeout enforced by template (`metadata.shell.timeout_secs`)
- timeout returns text: `shell emulator timeout: command exceeded <N>s`

## 4.5 Redirection

Single output redirection is supported:

```bash
echo hello > out.txt
```

The redirected content is written into session pending writes and then flushed into session `.tcow`.

Restrictions:

- only one `>`
- redirection target must be present
- no extra tokens after target

---

## 5. Working Directory Semantics

Each session tracks shell directory in metadata:

- `metadata.cwd`: current shell directory (persisted)
- `metadata.workdir`: optional initial directory preference

Resolution order at run/exec start:

1. `metadata.cwd` if present/non-empty
2. else `metadata.workdir` if present/non-empty
3. else `/`

Path behavior:

- relative paths resolve from current cwd
- absolute paths start at `/`
- `.` and `..` are normalized
- directory existence for `cd` is determined from union view of existing `.tcow` data and pending writes

After each `exec`, resolved cwd is written back to `metadata.cwd`.

---

## 6. Session and Filesystem Effects

## 6.1 Tool path (`shell_execute`)

- Runs inside normal agent loop.
- Tool result is returned to model and can be persisted in session message history.
- Any writes (including redirection output) are flushed into:
  - `.agent/fs/<session_id>.tcow`

## 6.2 CLI path (`-s <id> exec`)

- Runs one shell command in same emulator.
- Uses same `.tcow` and cwd/workdir metadata.
- Does not append user/assistant/tool messages to `spec.messages`.
- Persists `metadata.cwd` and any file writes.

Output newline behavior:

- if output already ends with newline, print as-is
- otherwise append newline for clean terminal rendering

---

## 7. Verbosity Interaction

Global CLI verbosity controls display style, not shell semantics:

- default: final assistant response only
- `-v`: user/assistant/tool event summary
- `-vv`: full host/guest diagnostics
- `-vvv`: hook execution trace (considered, executed, inputs/outputs per step)
- `-q`: suppress stdout (errors still on stderr)

These flags apply to normal runs. `exec` respects quiet mode by suppressing printed output when `-q` is set.

---

## 8. Help Text Contract

`wasm1` with no args and `wasm1 --help` both print the same expanded help text.

Shell-relevant command listed there:

- `-s <session_id> exec <command>`

with explicit note that it uses session shell context and metadata cwd/workdir.

---

## 9. Error Message Expectations

The runtime emits concise, user-guiding shell errors.

Common forms:

- `shell emulator error: ...` for malformed/simple usage errors
- `Policy Error: ...` for unsupported shell constructs
- `AI Policy Error: shell_execute command not in allow-list: ...` for template denial

This split is intentional:

- parser/usability issues are recoverable command edits
- policy issues communicate capability boundaries
- allow-list denial points to template configuration

---

## 10. Migration Notes from Old Design

Removed from runtime:

- Boa-based shell APIs
- `shell.run`, `shell.stdin`, `shell.kill`
- `child_process` policy shim model
- `.out.json` process-state files as primary shell contract

Current architecture is tool-native:

- one command string through `shell_execute`
- constrained parser + built-ins + optional host external spawn
- policy gating via template regex allow-list
- session metadata-backed cwd continuity
