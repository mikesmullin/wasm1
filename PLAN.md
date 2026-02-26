# PRD: Minimal Wasmtime + Rust + QuickJS Sandbox for xAI Grok Agent with Tool Calling

**Version** — 1.0  
**Date** — February 2026  
**Goal** — Build a tiny, secure, observable proof-of-concept that demonstrates an xAI Grok-powered agent running inside a Wasmtime WebAssembly sandbox. The agent can only use one tool: executing arbitrary JavaScript via a QuickJS interpreter exposed as a host function. All LLM inference happens via host functions (token/API key never enters Wasm guest). The system runs from CLI, prints verbose trace of the agent loop to stdout, and passes basic smoke tests.

**Non-goals for v0.1**  
- Multiple tools beyond `js_exec`  
- Stateful sessions / persistent memory across runs  
- WIT / Component Model (stick to core Wasm modules for simplicity)  
- Fuel / epoch limits (add later for safety)  
- Error recovery / retry loops in agent  
- WASI filesystem/network access for guest  
- Production hardening (secrets management, timeouts, etc.)

### 1. High-Level Architecture

- **Host binary**: Rust application using Wasmtime  
  - Reads CLI argument: prompt string  
  - Implements two host functions (imported by guest):  
    - `grok_chat`: calls xAI Grok API (using official Rust SDK) for inference  
    - `js_exec`: evaluates JavaScript code string in embedded QuickJS and returns output / error  
  - Loads a pre-compiled `.wasm` guest module (agent logic)  
  - Runs agent loop: prompt → inference → tool calls → tool results → repeat until final answer  
  - Prints every step (prompt, tool call, tool result, reasoning delta, final answer) to stdout

- **Guest Wasm module**: Small Rust program compiled to `wasm32-wasip1` (or wasip2)  
  - Contains the agent loop logic  
  - Imports and calls the two host functions above  
  - Uses simple string-based protocol for tool calls / results

- **LLM model**: Use `grok-4-1-fast-reasoning` or latest fast reasoning variant (tool-calling capable)

- **Security invariants**  
  - API token never enters guest memory  
  - Guest has no ambient authority (no WASI FS/network unless explicitly granted later)  
  - All side effects (API calls, JS execution) go through explicit host functions

### 2. Functional Requirements

**CLI interface**  
```
cargo run -- "You are a helpful assistant. Answer in one sentence: What is the capital of Japan?"
```
or multi-line prompt via file/quoted string.

**Output format (verbose stdout trace)**  
Every major event is printed clearly, roughly like:
```
[HOST] Starting agent with prompt: "..."
[HOST] Instantiating guest Wasm module...
[GUEST → LLM] Sending initial prompt + tools description
[LLM → GUEST] Received chunk: "Thought: I need to ..."
[LLM → GUEST] Tool call: js_exec { code: "console.log('test'); 42" }
[HOST] Executing js_exec → result: "test\n" + return value "42" (or error)
[GUEST → LLM] Appending tool result: "..."
[LLM → GUEST] Final answer: "Tokyo is the capital of Japan."
[HOST] Agent loop complete. Final answer: ...
```

**Tool definition (exposed to LLM)**  
Only one tool for now:

```json
{
  "type": "function",
  "function": {
    "name": "js_exec",
    "description": "Execute arbitrary JavaScript code in a sandboxed QuickJS environment. Use for calculations, string manipulation, simulations, or small programs. Code runs synchronously. console.log output is captured and returned. Return value is the last expression or explicit return. No Node.js APIs, no fetch, no filesystem.",
    "parameters": {
      "type": "object",
      "properties": {
        "code": { "type": "string", "description": "The JavaScript code to execute" }
      },
      "required": ["code"]
    }
  }
}
```

**Smoke test scenarios** (run via CLI and verify output visually)

1. **Simple direct answer**  
   Prompt: "What is 17 × 23?"  
   Expected: LLM answers directly (no tool) or uses js_exec for calculation.

2. **Magic 8-ball JavaScript program**  
   Prompt:  
   ```
   You are a magic 8-ball simulator. Write and execute a complete JavaScript program that:
   - Uses readline-like input (simulate with a hardcoded question for test)
   - Prints a random 8-ball style answer to stdout via console.log
   - Exits cleanly
   Use only console.log and basic JS (Math.random, arrays). Do not use external APIs.
   ```

   Expected behavior:  
   - LLM generates JS code  
   - Calls `js_exec` with that code  
   - You see console.log output in tool response  
   - Final answer summarizes or quotes the 8-ball reply

3. **Basic computation chain**  
   Prompt: "Compute (3^5 + 7) × 4 and tell me the result in words."  
   Expected: LLM uses js_exec at least once for math.

### 3. Technical Stack & Constraints

- Rust 1.80+ (stable)
- Wasmtime ^14.0 (or latest 2026 version)
- xAI Rust SDK: Use `xai-sdk` or `grok-rust-sdk` crate from crates.io (supports chat completions + tool calling)
- QuickJS in Wasm:  
  - Use an existing QuickJS → Wasm build (e.g. from javy, quickjs-wasm, or wasmedge-quickjs)  
  - Or embed QuickJS C → Wasm via wasi-sdk + link as host function  
  - `js_exec(code: &str) → Result<(stdout: String, result: String, error: Option<String>)>`
- Target: `wasm32-wasip1` (simpler WASI preview 1 for host functions + basic stdout)
- No external crates beyond necessities (wasmtime, xai-sdk, anyhow, etc.)
- Guest .wasm is built separately and embedded or loaded from disk

### 4. Acceptance Criteria

- Binary compiles and runs without errors
- CLI accepts prompt → produces readable stdout trace of full agent loop
- All LLM calls go through host function (no token in guest)
- `js_exec` tool works: can run `console.log("hello")` → sees output
- Smoke test #2 (magic 8-ball) succeeds: LLM writes JS, executes it, incorporates output
- No guest crash / trap on valid inputs
- Clear separation: token only in host binary (env var or hardcoded for test)

### 5. Nice-to-have Stretch (if time allows)

- Add fuel consumption to prevent infinite loops
- Capture QuickJS console output more richly
- Pretty-print JSON tool calls / results
- Support multi-turn in one run (continue after final answer)