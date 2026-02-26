use anyhow::{anyhow, Context, Result};
use dotenvy::from_filename;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::path::Path;
use std::process::Command;
use wasmtime::{Caller, Config, Engine, Linker, Module, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, add_to_linker as wasi_add_to_linker};

const DEFAULT_MODEL: &str = "grok-4-1-fast-reasoning";
const GUEST_WASM_PATH: &str = "guest/target/wasm32-wasip1/debug/guest.wasm";
const FUEL_LIMIT: u64 = 2_000_000_000;

struct HostState {
    prompt: String,
    final_answer: Option<String>,
    api_key: String,
    model: String,
    client: Client,
    wasi: WasiCtx,
}

#[derive(Debug, Deserialize)]
struct GuestRequest {
    prompt: String,
    tool_result: Option<String>,
    step: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum LlmDecision {
    #[serde(rename = "tool_call")]
    ToolCall {
        tool: String,
        code: String,
        thought: Option<String>,
    },
    #[serde(rename = "final")]
    Final {
        answer: String,
        thought: Option<String>,
    },
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    temperature: f32,
    messages: Vec<ChatMessage<'a>>,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    content: String,
}

fn main() -> Result<()> {
    let _ = from_filename(".env");

    let prompt = env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: cargo run -- \"<prompt>\""))?;

    println!("[HOST] Starting agent with prompt: {:?}", prompt);

    let api_key = env::var("XAI_API_KEY")
        .context("XAI_API_KEY is required (set it in environment or .env)")?;
    let model = env::var("XAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    println!("[HOST] Model: {model} | API key: loaded");

    ensure_guest_wasm()?;

    println!("[HOST] Instantiating guest Wasm module (fuel limit: {FUEL_LIMIT})...");

    let mut config = Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config)?;

    let module = Module::from_file(&engine, GUEST_WASM_PATH)
        .with_context(|| format!("failed to load {GUEST_WASM_PATH}"))?;

    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasi_add_to_linker(&mut linker, |s: &mut HostState| &mut s.wasi)?;

    linker.func_wrap(
        "host",
        "get_prompt",
        |mut caller: Caller<'_, HostState>, out_ptr: i32, out_cap: i32| -> i32 {
            let prompt = caller.data().prompt.clone();
            write_memory(&mut caller, out_ptr, out_cap, &prompt)
        },
    )?;

    linker.func_wrap(
        "host",
        "host_log",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
            if let Ok(line) = read_memory(&mut caller, ptr, len) {
                println!("[GUEST] {line}");
            }
        },
    )?;

    linker.func_wrap(
        "host",
        "emit_final",
        |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
            if let Ok(answer) = read_memory(&mut caller, ptr, len) {
                println!("[HOST] Agent loop complete. Final answer: {answer}");
                caller.data_mut().final_answer = Some(answer);
            }
        },
    )?;

    linker.func_wrap(
        "host",
        "grok_chat",
        |mut caller: Caller<'_, HostState>,
         req_ptr: i32,
         req_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            let req_json = match read_memory(&mut caller, req_ptr, req_len) {
                Ok(v) => v,
                Err(e) => {
                    let fallback = serde_json::to_string(&LlmDecision::Error {
                        message: format!("invalid request memory: {e}"),
                    })
                    .unwrap_or_else(|_| {
                        "{\"type\":\"error\",\"message\":\"internal\"}".to_string()
                    });
                    return write_memory(&mut caller, out_ptr, out_cap, &fallback);
                }
            };

            let req: GuestRequest = match serde_json::from_str(&req_json) {
                Ok(v) => v,
                Err(e) => {
                    let fallback = serde_json::to_string(&LlmDecision::Error {
                        message: format!("bad guest request JSON: {e}"),
                    })
                    .unwrap_or_else(|_| {
                        "{\"type\":\"error\",\"message\":\"internal\"}".to_string()
                    });
                    return write_memory(&mut caller, out_ptr, out_cap, &fallback);
                }
            };

            println!("[GUEST → LLM] step={} sending request", req.step);
            let decision = match llm_decide(caller.data(), &req) {
                Ok(v) => v,
                Err(err) => LlmDecision::Error {
                    message: format!("llm call failed: {err:#}"),
                },
            };

            if let LlmDecision::ToolCall { tool, code, .. } = &decision {
                println!("[LLM → GUEST] Tool call: {tool} {{ code: {code:?} }}");
            }
            if let LlmDecision::Final { answer, .. } = &decision {
                println!("[LLM → GUEST] Final answer: {answer}");
            }

            let response = serde_json::to_string(&decision).unwrap_or_else(|_| {
                "{\"type\":\"error\",\"message\":\"serialization failed\"}".to_string()
            });
            write_memory(&mut caller, out_ptr, out_cap, &response)
        },
    )?;

    let wasi = WasiCtxBuilder::new()
        .inherit_stdio()
        .build();
    let state = HostState {
        prompt,
        final_answer: None,
        api_key,
        model,
        client: Client::new(),
        wasi,
    };

    let mut store = Store::new(&engine, state);
    store
        .add_fuel(FUEL_LIMIT)
        .context("failed to set fuel limit")?;

    let instance = linker.instantiate(&mut store, &module)?;
    let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;
    run.call(&mut store, ())?;

    let consumed = store.fuel_consumed().unwrap_or(0);
    let remaining = FUEL_LIMIT.saturating_sub(consumed);
    println!("[HOST] Fuel consumed: {consumed} / {FUEL_LIMIT} (remaining: {remaining})");

    if store.data().final_answer.is_none() {
        println!("[HOST] Agent completed without final answer export.");
    }

    Ok(())
}

fn llm_decide(state: &HostState, req: &GuestRequest) -> Result<LlmDecision> {
    let tools_json = r#"[
  {
    "type": "function",
    "function": {
      "name": "js_exec",
      "description": "Execute arbitrary JavaScript code in a sandboxed Boa JS interpreter running inside a Wasmtime WebAssembly guest. Supports ES2020. console.log output is captured. No filesystem, fetch, or Node APIs available.",
      "parameters": {
        "type": "object",
        "properties": {
          "code": { "type": "string", "description": "The JavaScript code to execute" }
        },
        "required": ["code"]
      }
    }
  }
]"#;

    let system = "You are an agent planner. Return ONLY valid JSON with schema:\n\
{\"type\":\"tool_call\",\"tool\":\"js_exec\",\"code\":\"...\",\"thought\":\"...\"}\n\
or\n\
{\"type\":\"final\",\"answer\":\"...\",\"thought\":\"...\"}.\n\
Use js_exec when computation or program execution is useful. No markdown, no code fences.";

    let user = match &req.tool_result {
        Some(result) => format!(
            "Original prompt:\n{}\n\nTool available:\n{}\n\nLatest tool result from js_exec:\n{}\n\nIf this is sufficient, return final answer.",
            req.prompt, tools_json, result
        ),
        None => format!(
            "User prompt:\n{}\n\nTool available:\n{}\n\nDecide next step.",
            req.prompt, tools_json
        ),
    };

    let body = ChatRequest {
        model: &state.model,
        temperature: 0.1,
        messages: vec![
            ChatMessage {
                role: "system",
                content: system,
            },
            ChatMessage {
                role: "user",
                content: &user,
            },
        ],
    };

    let resp = state
        .client
        .post("https://api.x.ai/v1/chat/completions")
        .bearer_auth(&state.api_key)
        .json(&body)
        .send()
        .context("request to xAI failed")?
        .error_for_status()
        .context("xAI returned non-success status")?;

    let payload: ChatResponse = resp.json().context("failed to parse xAI response JSON")?;
    let content = payload
        .choices
        .first()
        .ok_or_else(|| anyhow!("xAI returned zero choices"))?
        .message
        .content
        .trim()
        .to_string();

    parse_llm_decision(&content)
}

fn parse_llm_decision(text: &str) -> Result<LlmDecision> {
    if let Ok(v) = serde_json::from_str::<LlmDecision>(text) {
        return Ok(v);
    }
    let cleaned = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str::<LlmDecision>(cleaned)
        .with_context(|| format!("model output not valid decision JSON: {text}"))
}

fn ensure_guest_wasm() -> Result<()> {
    if Path::new(GUEST_WASM_PATH).exists() {
        return Ok(());
    }

    let target_check = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    if let Ok(output) = target_check {
        let installed = String::from_utf8_lossy(&output.stdout);
        if !installed.contains("wasm32-wasip1") {
            println!("[HOST] Installing target wasm32-wasip1...");
            let status = Command::new("rustup")
                .args(["target", "add", "wasm32-wasip1"])
                .status()
                .context("failed to launch rustup target add")?;
            if !status.success() {
                return Err(anyhow!("rustup target add wasm32-wasip1 failed"));
            }
        }
    }

    println!("[HOST] Building guest Wasm module...");
    let status = Command::new("cargo")
        .args([
            "build",
            "--manifest-path",
            "guest/Cargo.toml",
            "--target",
            "wasm32-wasip1",
        ])
        .status()
        .context("failed to launch cargo build for guest")?;

    if !status.success() {
        return Err(anyhow!("guest build failed"));
    }

    if !Path::new(GUEST_WASM_PATH).exists() {
        return Err(anyhow!("guest wasm not found at {GUEST_WASM_PATH}"));
    }
    Ok(())
}

fn read_memory(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Result<String> {
    if ptr < 0 || len < 0 {
        return Err(anyhow!("negative memory range"));
    }
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| anyhow!("guest memory export missing"))?;
    let mut bytes = vec![0u8; len as usize];
    memory
        .read(caller, ptr as usize, &mut bytes)
        .context("failed reading guest memory")?;
    String::from_utf8(bytes).context("guest memory is not valid utf-8")
}

fn write_memory(
    caller: &mut Caller<'_, HostState>,
    out_ptr: i32,
    out_cap: i32,
    data: &str,
) -> i32 {
    if out_ptr < 0 || out_cap <= 0 {
        return -1;
    }
    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return -2,
    };
    let data_bytes = data.as_bytes();
    let write_len = data_bytes.len().min((out_cap as usize).saturating_sub(1));
    if memory
        .write(&mut *caller, out_ptr as usize, &data_bytes[..write_len])
        .is_err()
    {
        return -3;
    }
    if memory
        .write(&mut *caller, out_ptr as usize + write_len, &[0])
        .is_err()
    {
        return -4;
    }
    write_len as i32
}
