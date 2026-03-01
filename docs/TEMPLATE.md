# Template Schema (`daemon/v1`)

This document defines the `wasm1` agent template format. The schema is intentionally kept
compatible with `subd`'s `daemon/v1` format (minus heartbeat).

## Minimal template

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: General purpose helper
  model: xai:grok-4-fast-reasoning
spec:
  system_prompt: |
    You are a helpful assistant.
```

## Top-level fields

| Field | Type | Required | Notes |
|---|---|---:|---|
| `apiVersion` | string | yes | Must be `daemon/v1`. |
| `kind` | string | yes | Must be `Agent`. |
| `metadata` | object | yes | Runtime + behavior config. |
| `spec` | object | yes | Prompt and execution-facing data. |

## `metadata` fields

| Field | Type | Required | Notes |
|---|---|---:|---|
| `description` | string | no | Human-friendly description. |
| `model` | string | no | Provider-prefixed model string, e.g. `xai:grok-4-fast-reasoning`. |
| `context_window` | number | no | Context window size in tokens. Known xAI models are resolved automatically from a built-in table; set this only for unlisted or custom models. Used to display `[CTX]` usage percentages. |
| `tools` | array | no | Tool allowlist for the session. Absent = all tools. |
| `shell` | object | no | Shell execution policy (wasm1 extension). |

## `metadata.shell` fields

| Field | Type | Required | Notes |
|---|---|---:|---|
| `allow` | array | no | Ordered list of regexp patterns matched against the full command string. First match wins. Absent (or empty) = all shell commands denied. |
| `timeout_secs` | number | no | Kill child processes after N seconds. Default: wait indefinitely. |

## `spec` fields

| Field | Type | Required | Notes |
|---|---|---:|---|
| `system_prompt` | string | no | Extra context prepended to the agent's base system instructions, separated by `---`. |
| `max_steps` | number | no | Maximum tool-call iterations. Absent = unlimited. |

---

## Available tools

| Tool name | Description |
|---|---|
| `js_exec` | Execute JavaScript in the sandboxed Boa ES2020 interpreter. Globals: `console.log`, `fs.readFile(path)`, `fs.writeFile(path, content)`, `fs.readdir(dir)`, `require(path)`. Real host filesystem is NOT accessible. |
| `fs__file__view` | Read and return the full contents of a `.tcow` virtual-FS file. |
| `fs__file__create` | Create or overwrite a `.tcow` virtual-FS file. |
| `fs__file__edit` | Replace the first occurrence of `oldString` with `newString` in a `.tcow` file. |
| `fs__directory__list` | List entries under a directory in the `.tcow` virtual FS. |

When `metadata.tools` is absent, all tools above are available to the model.

---

## LLM tool-call JSON formats

The model returns one of:

```jsonc
// Tool call — named tool with structured args
{"type":"tool_call","tool":"fs__file__view","args":{"filePath":"notes.md"},"thought":"..."}
{"type":"tool_call","tool":"fs__file__create","args":{"filePath":"out.txt","content":"hello"},"thought":"..."}
{"type":"tool_call","tool":"fs__file__edit","args":{"filePath":"out.txt","oldString":"hello","newString":"world"},"thought":"..."}
{"type":"tool_call","tool":"fs__directory__list","args":{"path":""},"thought":"..."}

// js_exec — args.code or top-level code field both accepted
{"type":"tool_call","tool":"js_exec","args":{"code":"console.log(1+1)"},"thought":"..."}

// Final answer
{"type":"final","answer":"42","thought":"..."}
```

---

## Example templates

### Minimal (all tools, no shell)

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: General purpose helper
  model: xai:grok-4-fast-reasoning
```

### FS-only agent

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: File editor
  model: xai:grok-4-fast-reasoning
  tools:
    - fs__file__view
    - fs__file__create
    - fs__file__edit
    - fs__directory__list
spec:
  max_steps: 20
```

### Shell-enabled automation agent

```yaml
apiVersion: daemon/v1
kind: Agent
metadata:
  description: Shell agent
  model: xai:grok-4-fast-reasoning
  # context_window is optional — omit for known xAI models (auto-detected).
  # Set only for custom or unlisted models:
  # context_window: 131072
  tools:
    - js_exec
    - fs__file__view
    - fs__file__create
    - fs__file__edit
    - fs__directory__list
  shell:
    allow:
      - '^bash\b'
      - '^python3\b'
      - '^git\s+(status|log|diff)\b'
    timeout_secs: 60
spec:
  max_steps: 50
  system_prompt: |
    You are a shell automation agent.
```
