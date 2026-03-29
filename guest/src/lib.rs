use serde::{Deserialize, Serialize};

const BUF_SIZE: usize = 128 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryEntry {
    tool_call_id: String,
    tool_name: String,
    assistant_msg_json: String,
    result_json: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ToolInvocation {
    tool: String,
    tool_call_id: String,
    #[serde(default)]
    args: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct GuestSeed {
    prompt: String,
    #[serde(default)]
    history: Vec<HistoryEntry>,
    #[serde(default)]
    validation_feedback: Vec<String>,
    #[serde(default)]
    step: u32,
    #[serde(default)]
    pending_tool_calls: Vec<PendingToolCall>,
    #[serde(default)]
    blocked_on_approval: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct PendingToolCall {
    tool: String,
    tool_call_id: String,
    #[serde(default)]
    args: serde_json::Value,
    assistant_msg_json: String,
}

#[derive(Debug, Serialize)]
struct GuestRequest<'a> {
    prompt: &'a str,
    history: &'a [HistoryEntry],
    validation_feedback: &'a [String],
    step: u32,
    unsent_tool_result_ids: &'a [String],
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum LlmDecision {
    #[serde(rename = "tool_call")]
    ToolCall {
        tool_calls: Vec<ToolInvocation>,
        assistant_msg_json: String,
    },
    #[serde(rename = "final")]
    Final {
        answer: String,
        thought: Option<String>,
    },
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Serialize)]
struct ToolExecResult {
    stdout: String,
    result: String,
    error: Option<String>,
}

#[link(wasm_import_module = "host")]
unsafe extern "C" {
    fn get_prompt(out_ptr: i32, out_cap: i32) -> i32;
    fn get_session_seed(out_ptr: i32, out_cap: i32) -> i32;
    fn get_max_steps() -> i32;
    fn save_session_checkpoint(
        req_ptr: i32,
        req_len: i32,
        state_ptr: i32,
        state_len: i32,
        action_ptr: i32,
        action_len: i32,
    ) -> i32;
    fn host_log(ptr: i32, len: i32);
    fn emit_final(ptr: i32, len: i32);
    fn grok_chat(req_ptr: i32, req_len: i32, out_ptr: i32, out_cap: i32) -> i32;
    fn tool_dispatch(
        name_ptr: i32,
        name_len: i32,
        args_ptr: i32,
        args_len: i32,
        out_ptr: i32,
        out_cap: i32,
    ) -> i32;
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
    let seed = read_session_seed()?;
    let prompt = if seed.prompt.is_empty() {
        read_prompt()?
    } else {
        seed.prompt
    };
    log_line(&format!("Received prompt: {prompt}"));

    let mut history: Vec<HistoryEntry> = seed.history;
    let validation_feedback: Vec<String> = seed.validation_feedback;
    let max_steps: i32 = unsafe { get_max_steps() };
    let step: u32 = seed.step;

    if seed.blocked_on_approval && seed.pending_tool_calls.is_empty() {
        log_line("Waiting for manual approval; no executable calls this turn");
        return Ok(());
    }

    if !seed.pending_tool_calls.is_empty() {
        // Execute exactly one pending tool call per run (strict STEPWISE).
        // Remaining calls stay pending and will be executed on subsequent resumes.
        let call = seed.pending_tool_calls.into_iter().next().unwrap();
        let tool = call.tool;
        let tool_call_id = call.tool_call_id;
        let args_str = call.args.to_string();
        let result = call_tool_dispatch(&tool, &args_str);
        log_line(&format!("Tool result ({tool_call_id}): {result}"));
        history.push(HistoryEntry {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool,
            assistant_msg_json: call.assistant_msg_json,
            result_json: result,
        });
        let unsent_ids = vec![tool_call_id];
        checkpoint_state(
            &prompt,
            &history,
            &validation_feedback,
            step + 1,
            "IDLE",
            "tool_result",
            &unsent_ids,
        );
        return Ok(());
    }

    if max_steps >= 0 && step >= max_steps as u32 {
        checkpoint_state(
            &prompt,
            &history,
            &validation_feedback,
            step,
            "IDLE",
            "step_complete",
            &[],
        );
        return Ok(());
    }

    let req = GuestRequest {
        prompt: &prompt,
        history: &history,
        validation_feedback: &validation_feedback,
        step,
        unsent_tool_result_ids: &[],
    };
    let req_json = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    let llm_raw = call_host_text_grok(&req_json)?;
    log_line(&format!("LLM raw response: {llm_raw}"));

    let decision: LlmDecision = serde_json::from_str(&llm_raw)
        .map_err(|e| format!("failed parsing llm decision: {e}; raw={llm_raw}"))?;

    match decision {
        LlmDecision::ToolCall { tool_calls, assistant_msg_json } => {
            let _ = assistant_msg_json;
            log_line(&format!("Tool calls requested: {}", tool_calls.len()));
            Ok(())
        }
        LlmDecision::Final { answer, thought } => {
            if let Some(t) = thought {
                log_line(&format!("Model thought: {t}"));
            }
            emit_final_line(&answer);
            Ok(())
        }
        LlmDecision::Error { message } => Err(format!("llm error: {message}")),
    }
}

fn read_session_seed() -> Result<GuestSeed, String> {
    let mut out = vec![0u8; BUF_SIZE];
    let written = unsafe { get_session_seed(out.as_mut_ptr() as i32, out.len() as i32) };
    if written < 0 {
        return Err(format!("get_session_seed failed with {written}"));
    }
    let raw = String::from_utf8(out[..written as usize].to_vec()).map_err(|e| e.to_string())?;
    if raw.trim().is_empty() || raw.trim() == "{}" {
        return Ok(GuestSeed {
            prompt: "".to_string(),
            history: Vec::new(),
            validation_feedback: Vec::new(),
            step: 0,
            pending_tool_calls: Vec::new(),
            blocked_on_approval: false,
        });
    }
    serde_json::from_str(&raw).map_err(|e| e.to_string())
}

fn checkpoint_state(
    prompt: &str,
    history: &[HistoryEntry],
    validation_feedback: &[String],
    step: u32,
    state: &str,
    action: &str,
    unsent_tool_result_ids: &[String],
) {
    let req = GuestRequest {
        prompt,
        history,
        validation_feedback,
        step,
        unsent_tool_result_ids,
    };
    if let Ok(raw) = serde_json::to_string(&req) {
        let rc = unsafe {
            save_session_checkpoint(
                raw.as_ptr() as i32,
                raw.len() as i32,
                state.as_ptr() as i32,
                state.len() as i32,
                action.as_ptr() as i32,
                action.len() as i32,
            )
        };
        if rc != 0 {
            log_line(&format!("save_session_checkpoint failed: {rc}"));
        }
    }
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

fn call_tool_dispatch(name: &str, args_json: &str) -> String {
    let mut out = vec![0u8; BUF_SIZE];
    let written = unsafe {
        tool_dispatch(
            name.as_ptr() as i32,
            name.len() as i32,
            args_json.as_ptr() as i32,
            args_json.len() as i32,
            out.as_mut_ptr() as i32,
            out.len() as i32,
        )
    };
    if written < 0 {
        let err_msg = format!("tool_dispatch returned {written}");
        return serde_json::to_string(&ToolExecResult {
            stdout: String::new(),
            result: String::new(),
            error: Some(err_msg),
        })
        .unwrap_or_else(|_| r#"{"stdout":"","result":"","error":"dispatch failed"}"#.to_string());
    }

    let raw = String::from_utf8_lossy(&out[..written as usize]).into_owned();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    if let Some(r) = parsed["result"].as_str() {
        serde_json::to_string(&ToolExecResult {
            stdout: r.to_string(),
            result: "undefined".to_string(),
            error: None,
        })
        .unwrap_or_else(|_| r#"{"stdout":"","result":"undefined","error":null}"#.to_string())
    } else if let Some(e) = parsed["error"].as_str() {
        serde_json::to_string(&ToolExecResult {
            stdout: String::new(),
            result: String::new(),
            error: Some(e.to_string()),
        })
        .unwrap_or_else(|_| r#"{"stdout":"","result":"","error":"dispatch error"}"#.to_string())
    } else {
        serde_json::to_string(&ToolExecResult {
            stdout: raw,
            result: "undefined".to_string(),
            error: None,
        })
        .unwrap_or_else(|_| r#"{"stdout":"","result":"undefined","error":null}"#.to_string())
    }
}

fn log_line(line: &str) {
    unsafe { host_log(line.as_ptr() as i32, line.len() as i32) }
}

fn emit_final_line(line: &str) {
    unsafe { emit_final(line.as_ptr() as i32, line.len() as i32) }
}
