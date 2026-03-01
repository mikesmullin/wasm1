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
│  └── TcowFs               (virtual CoW filesystem)  [planned]│
│                                                             │
│  Linker — host functions exposed to guest:                  │
│  ├── host::get_prompt     read the initial prompt           │
│  ├── host::grok_chat      call xAI Grok for inference       │
│  ├── host::host_log       write a log line to host stdout   │
│  └── host::emit_final     emit the agent's final answer     │
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
│              ├── require()      [planned — TCOW-backed]      │
│              └── fs.readFile()  [planned — TCOW-backed]      │
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

### In-guest tool: `js_exec`

`js_exec` is **not** a host function. JavaScript execution happens entirely inside the guest module using **[Boa](https://github.com/boa-dev/boa)**, a pure-Rust ES2020 interpreter compiled into the `.wasm` binary itself. The guest calls `run_js_in_boa(code)` directly — no round-trip to the host. `console.log` output is captured via an injected shim and returned alongside the final expression value as JSON.

Virtual filesystem access is exposed **only to Boa**, not as Wasmtime host functions. Planned Boa-native APIs (backed by the TCOW library compiled into the guest):

| JS API | Description |
|---|---|
| `require(path)` | Load and execute a JS module from the virtual `.tcow` filesystem |
| `fs.readFile(path)` | Read a file from the virtual `.tcow` filesystem, returns a string |

See [docs/ADD_FS.md](docs/ADD_FS.md) for the full TCOW integration plan.

---

## Security Model

- The xAI API key lives **only in the host process** (env var / `.env` file). It is never written into guest linear memory.
- The guest has **no ambient WASI authority** — no direct filesystem or network access. Everything goes through named host functions that the host explicitly registers.
- JavaScript execution runs inside **[Boa](https://github.com/boa-dev/boa)**, a pure-Rust ES2020 interpreter compiled directly into the guest `.wasm` binary. It has no access to the host filesystem, network, or any WASI capability — only what the Boa `Context` explicitly provides.
- Virtual filesystem access is exposed **only to Boa**, not to the Wasm module at large. Planned `require()` and `fs.readFile()` shims are implemented as Boa native objects backed by the TCOW library compiled into the guest. No `fs_*` host functions are needed — the guest never makes individual file-op calls back to the host.

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

The host automatically rebuilds the guest if `guest/target/wasm32-wasip1/debug/guest.wasm` is stale.

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

---

## Related

- **[tcow CLI](https://github.com/mikesmullin/tcow)** — standalone tool to inspect and manipulate `.tcow` virtual filesystem files. (a copy of this src is checked out to `./tmp/tcow/` (read its `**/*.md` to understand the project in detail. also read `./docs/ADD_FS.md` to understand how it is planned to integrate with `wasm1` (integration is only planned; hasn't happened, yet))
- **[docs/PLAN.md](docs/PLAN.md)** — full PRD including acceptance criteria and stretch goals. (although the implementation has changed since this PLAN was first written)
