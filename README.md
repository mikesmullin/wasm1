# wasm1 — Wasmtime AI Agent Sandbox

A Rust proof-of-concept for running an **xAI Grok-powered AI agent** inside a **Wasmtime WebAssembly sandbox**, with tool calling, an in-guest JavaScript runtime, and a copy-on-write virtual filesystem.

The agent loop runs entirely inside a `.wasm` guest module. All privileged operations — LLM inference, code execution, filesystem access — are mediated through **explicit host functions**. The API token never enters guest memory.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Host Process  (Rust)                                       │
│                                                             │
│  HostState                                                  │
│  ├── XAI API key          (never crosses into guest)        │
│  ├── reqwest HTTP client  (for Grok API calls)              │
│  ├── tcow_path            path to agent.tcow on disk        │
│  └── pending_writes       buffered VFS writes (flushed on exit)│
│                                                             │
│  Linker — host functions exposed to guest:                  │
│  ├── host::get_prompt     read the initial prompt           │
│  ├── host::grok_chat      call xAI Grok for inference       │
│  ├── host::host_log       write a log line to host stdout   │
│  ├── host::emit_final     emit the agent's final answer     │
│  ├── host::fs_read        read a file from agent.tcow       │
│  ├── host::fs_write       buffer a write to agent.tcow      │
│  └── host::fs_list        list a directory in agent.tcow    │
└──────────────────────┬──────────────────────────────────────┘
                       │  Wasmtime
┌──────────────────────▼──────────────────────────────────────┐
│  Guest Module  (guest/src/lib.rs → wasm32-wasip1)           │
│                                                             │
│  run()                                                      │
│  └── agent loop (up to MAX_STEPS):                          │
│       1. serialize prompt + tool result → grok_chat()       │
│       2. parse LLM response (ToolCall | Final | Error)      │
│       3. dispatch tool:                                      │
│            js_exec → run_js_in_boa() [Boa, in-guest]        │
│              ├── console.log shim (captured stdout)          │
│              ├── fs.readFile(path)  → host::fs_read          │
│              ├── fs.writeFile(p,d)  → host::fs_write         │
│              ├── fs.readdir(dir)    → host::fs_list          │
│              └── require(path)      → fs.readFile + eval     │
│       4. feed result back → repeat                          │
│       5. emit_final() on done                               │
└─────────────────────────────────────────────────────────────┘
         │                              │
     agent.tcow               host stdout trace
  (CoW virtual FS)          [HOST] / [GUEST] / [LLM]
```

---

## Host Functions

All host functions use a simple `i32` ABI (pointer + length pairs, return value = byte count or negative error code). No WIT / Component Model — raw core Wasm imports for simplicity.

| Function | Description |
|---|---|
| `get_prompt` | Returns the CLI prompt string to the guest |
| `grok_chat(req_json)` | Sends a chat request to xAI Grok, returns LLM decision JSON |
| `host_log(msg)` | Emits a prefixed log line to host stdout |
| `emit_final(answer)` | Signals the agent's final answer to the host |
| `fs_read(path)` | Reads a file from the virtual `.tcow` filesystem (union view across all layers) |
| `fs_write(path, data)` | Buffers a write; flushed as a new delta layer to `agent.tcow` on exit |
| `fs_list(dir)` | Returns newline-delimited visible entry names under a directory |

### In-guest tool: `js_exec`

`js_exec` is **not** a host function. JavaScript execution happens entirely inside the guest module using **[Boa](https://github.com/boa-dev/boa)**, a pure-Rust ES2020 interpreter compiled into the `.wasm` binary itself. The guest calls `run_js_in_boa(code)` directly — no round-trip to the host for JS evaluation. `console.log` output is captured via an injected shim and returned alongside the final expression value as JSON.

Virtual filesystem access is exposed to JS code via Boa native objects that call the `fs_*` host functions under the hood:

| JS API | Backed by | Description |
|---|---|---|
| `fs.readFile(path)` | `host::fs_read` | Read a file from the virtual `.tcow` filesystem |
| `fs.writeFile(path, data)` | `host::fs_write` | Write a file into the virtual `.tcow` filesystem |
| `fs.readdir(dir)` | `host::fs_list` | List visible entries under a directory |
| `require(path)` | `host::fs_read` + eval | Load and evaluate a JS module from the `.tcow` filesystem |

---

## Security Model

- The xAI API key lives **only in the host process** (env var / `.env` file). It is never written into guest linear memory.
- The guest has **no ambient WASI authority** — no direct filesystem or network access. Everything goes through named host functions that the host explicitly registers.
- JavaScript execution runs inside **[Boa](https://github.com/boa-dev/boa)**, a pure-Rust ES2020 interpreter compiled directly into the guest `.wasm` binary. It has no access to the host filesystem, network, or any WASI capability — only what the Boa `Context` explicitly provides.
- Virtual filesystem access is scoped to the `.tcow` virtual FS. The `fs_*` host functions are registered in the Wasmtime linker, but JS code reaches them only through Boa native object wrappers (`fs.readFile`, `fs.writeFile`, `fs.readdir`, `require`). The real host filesystem is not reachable from JS. All writes are buffered in-process and flushed as a new delta layer to `agent.tcow` on clean exit, providing a persistent, auditable record across runs.
- Shell execution (`shell.run(cmd, [params])`) is **host-mediated and policy-gated**: the host receives a pre-parsed executable + argument list (instead of parsing arbitrary bash script text), then matches the reconstructed command against `metadata.shell.allow` regexes (default deny if no match). Optional `metadata.shell.timeout_secs` bounds runtime, and only session-owned child PIDs can be controlled via `shell.stdin` / `shell.kill`.

---

## Building

**Prerequisites:** Rust stable, `wasm32-wasip1` target, an `XAI_API_KEY` env var.

```bash
# Add the Wasm target (once)
rustup target add wasm32-wasip1

# Build the guest module
cargo build --manifest-path guest/Cargo.toml --target wasm32-wasip1

# Build and run the host
XAI_API_KEY=your_key cargo run -- "What is the capital of Japan?"
```

The host automatically rebuilds the guest if `guest/target/wasm32-wasip1/debug/guest.wasm` is stale. The virtual filesystem is stored in `agent.tcow` (path overridable via `TCOW_PATH` env var); it is created on the first write and extended with a new delta layer on each subsequent run.

---

## Output Trace

```
[HOST] Starting agent with prompt: "What is 17 × 23?"
[HOST] Model: grok-4-1-fast-reasoning | API key: loaded
[HOST] Instantiating guest Wasm module (fuel limit: 2000000000)...
[GUEST] Starting guest agent loop
[GUEST → LLM] Sending step 0
[LLM → GUEST] Tool call: js_exec
[GUEST] Model thought: I should calculate this precisely.
[GUEST] Tool call requested: js_exec
[HOST] Executing js_exec → 391
[GUEST] Tool result: 391
[GUEST → LLM] Sending step 1
[LLM → GUEST] Final answer: 17 × 23 = 391.
[HOST] Agent loop complete.
```
