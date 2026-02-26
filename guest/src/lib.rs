use boa_engine::{Context as BoaContext, Source};
use serde::{Deserialize, Serialize};

const BUF_SIZE: usize = 128 * 1024;
const MAX_STEPS: u32 = 6;

#[derive(Debug, Serialize)]
struct GuestRequest<'a> {
    prompt: &'a str,
    tool_result: Option<&'a str>,
    step: u32,
}

#[derive(Debug, Deserialize)]
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

#[link(wasm_import_module = "host")]
unsafe extern "C" {
    fn get_prompt(out_ptr: i32, out_cap: i32) -> i32;
    fn host_log(ptr: i32, len: i32);
    fn emit_final(ptr: i32, len: i32);
    fn grok_chat(req_ptr: i32, req_len: i32, out_ptr: i32, out_cap: i32) -> i32;
}

#[no_mangle]
pub extern "C" fn run() {
    match run_inner() {
        Ok(()) => {}
        Err(err) => {
            let msg = format!("Guest error: {err}");
            log_line(&msg);
            emit_final_line(&msg);
        }
    }
}

fn run_inner() -> Result<(), String> {
    log_line("Starting guest agent loop");
    let prompt = read_prompt()?;
    log_line(&format!("Received prompt: {prompt}"));

    let mut tool_result: Option<String> = None;

    for step in 0..MAX_STEPS {
        let req = GuestRequest {
            prompt: &prompt,
            tool_result: tool_result.as_deref(),
            step,
        };
        let req_json = serde_json::to_string(&req).map_err(|e| e.to_string())?;
        let llm_raw = call_host_text_grok(&req_json)?;
        log_line(&format!("LLM raw response: {llm_raw}"));

        let decision: LlmDecision = serde_json::from_str(&llm_raw)
            .map_err(|e| format!("failed parsing llm decision: {e}; raw={llm_raw}"))?;

        match decision {
            LlmDecision::ToolCall {
                tool,
                code,
                thought,
            } => {
                log_line(&format!("Tool call requested: {tool}"));
                if let Some(t) = thought {
                    log_line(&format!("Model thought: {t}"));
                }
                if tool != "js_exec" {
                    return Err(format!("unsupported tool: {tool}"));
                }
                let result = run_js_in_boa(&code);
                log_line(&format!("Tool result: {result}"));
                tool_result = Some(result);
            }
            LlmDecision::Final { answer, thought } => {
                if let Some(t) = thought {
                    log_line(&format!("Model thought: {t}"));
                }
                emit_final_line(&answer);
                return Ok(());
            }
            LlmDecision::Error { message } => {
                return Err(format!("llm error: {message}"));
            }
        }
    }

    emit_final_line("Stopped after max steps without final answer.");
    Ok(())
}

fn read_prompt() -> Result<String, String> {
    let mut out = vec![0u8; BUF_SIZE];
    let written = unsafe { get_prompt(out.as_mut_ptr() as i32, out.len() as i32) };
    if written < 0 {
        return Err(format!("get_prompt failed with {written}"));
    }
    String::from_utf8(out[..written as usize].to_vec()).map_err(|e| e.to_string())
}

fn call_host_text_grok(input: &str) -> Result<String, String> {
    let mut out = vec![0u8; BUF_SIZE];
    let written = unsafe {
        grok_chat(
            input.as_ptr() as i32,
            input.len() as i32,
            out.as_mut_ptr() as i32,
            out.len() as i32,
        )
    };
    if written < 0 {
        return Err(format!("grok_chat failed with {written}"));
    }
    String::from_utf8(out[..written as usize].to_vec()).map_err(|e| e.to_string())
}

fn log_line(line: &str) {
    unsafe { host_log(line.as_ptr() as i32, line.len() as i32) }
}

fn emit_final_line(line: &str) {
    unsafe { emit_final(line.as_ptr() as i32, line.len() as i32) }
}

#[derive(Serialize)]
struct JsResult {
    stdout: String,
    result: String,
    error: Option<String>,
}

fn run_js_in_boa(code: &str) -> String {
    let mut ctx = BoaContext::default();

    let setup = r#"
        var __logs = [];
        var console = { log: function() {
            var parts = [];
            for (var i = 0; i < arguments.length; i++) parts.push(String(arguments[i]));
            __logs.push(parts.join(' '));
        }};
    "#;

    if let Err(e) = ctx.eval(Source::from_bytes(setup)) {
        let r = JsResult {
            stdout: String::new(),
            result: String::new(),
            error: Some(format!("console setup failed: {e:?}")),
        };
        return serde_json::to_string(&r).unwrap_or_default();
    }

    let user_result = ctx.eval(Source::from_bytes(code));

    let stdout = match ctx.eval(Source::from_bytes("__logs.join('\\n')")) {
        Ok(v) => v.to_string(&mut ctx)
            .map(|s| s.to_std_string_escaped())
            .unwrap_or_default(),
        Err(_) => String::new(),
    };

    let r = match user_result {
        Ok(val) => {
            let result = val.to_string(&mut ctx)
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_else(|_| "undefined".to_string());
            JsResult { stdout, result, error: None }
        }
        Err(e) => JsResult {
            stdout,
            result: String::new(),
            error: Some(format!("{e:?}")),
        },
    };

    serde_json::to_string(&r).unwrap_or_default()
}