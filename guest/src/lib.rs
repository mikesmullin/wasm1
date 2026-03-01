use boa_engine::{
    js_string,
    object::ObjectInitializer,
    property::Attribute,
    Context as BoaContext,
    JsNativeError,
    JsResult,
    JsString,
    JsValue,
    NativeFunction,
    Source,
};
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
    // Virtual .tcow filesystem — exposed only to Boa, not to the agent loop directly
    fn fs_read(path_ptr: i32, path_len: i32, out_ptr: i32, out_cap: i32) -> i32;
    fn fs_write(path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32) -> i32;
    fn fs_list(dir_ptr: i32, dir_len: i32, out_ptr: i32, out_cap: i32) -> i32;
}

// ── Safe VFS wrappers called from Boa native functions ────────────────────────

fn vfs_read(path: &str) -> Result<Vec<u8>, i32> {
    let mut buf = vec![0u8; 256 * 1024];
    let rc = unsafe {
        fs_read(
            path.as_ptr() as i32,
            path.len() as i32,
            buf.as_mut_ptr() as i32,
            buf.len() as i32,
        )
    };
    if rc >= 0 {
        buf.truncate(rc as usize);
        return Ok(buf);
    }
    if rc == -2 {
        // Buffer too small — retry at 8 MiB
        let mut big = vec![0u8; 8 * 1024 * 1024];
        let rc2 = unsafe {
            fs_read(
                path.as_ptr() as i32,
                path.len() as i32,
                big.as_mut_ptr() as i32,
                big.len() as i32,
            )
        };
        if rc2 >= 0 {
            big.truncate(rc2 as usize);
            return Ok(big);
        }
        return Err(rc2);
    }
    Err(rc)
}

fn vfs_write(path: &str, content: &[u8]) -> Result<(), i32> {
    let rc = unsafe {
        fs_write(
            path.as_ptr() as i32,
            path.len() as i32,
            content.as_ptr() as i32,
            content.len() as i32,
        )
    };
    if rc == 0 { Ok(()) } else { Err(rc) }
}

fn vfs_list(dir: &str) -> Result<String, i32> {
    let mut buf = vec![0u8; 64 * 1024];
    let rc = unsafe {
        fs_list(
            dir.as_ptr() as i32,
            dir.len() as i32,
            buf.as_mut_ptr() as i32,
            buf.len() as i32,
        )
    };
    if rc >= 0 {
        buf.truncate(rc as usize);
        Ok(String::from_utf8_lossy(&buf).into_owned())
    } else {
        Err(rc)
    }
}

// ── Boa NativeFunction implementations ───────────────────────────────────────

/// js: fs.readFile(path) → string  (throws on error)
fn js_fs_read_file(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let path = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_string(ctx)?
        .to_std_string_escaped();
    match vfs_read(&path) {
        Ok(bytes) => {
            let s = String::from_utf8_lossy(&bytes).into_owned();
            Ok(JsValue::from(JsString::from(s.as_str())))
        }
        Err(-1) => Err(JsNativeError::error()
            .with_message(format!("fs.readFile: not found: {path}"))
            .into()),
        Err(code) => Err(JsNativeError::error()
            .with_message(format!("fs.readFile: error {code} reading {path}"))
            .into()),
    }
}

/// js: fs.writeFile(path, content)
fn js_fs_write_file(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let path = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_string(ctx)?
        .to_std_string_escaped();
    let content = args
        .get(1)
        .unwrap_or(&JsValue::undefined())
        .to_string(ctx)?
        .to_std_string_escaped();
    match vfs_write(&path, content.as_bytes()) {
        Ok(()) => Ok(JsValue::undefined()),
        Err(code) => Err(JsNativeError::error()
            .with_message(format!("fs.writeFile: error {code} writing {path}"))
            .into()),
    }
}

/// js: fs.readdir(dir) → newline-delimited string of entry names
fn js_fs_readdir(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let dir = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_string(ctx)?
        .to_std_string_escaped();
    match vfs_list(&dir) {
        Ok(listing) => Ok(JsValue::from(JsString::from(listing.as_str()))),
        Err(-1) => Err(JsNativeError::error()
            .with_message(format!("fs.readdir: not found: {dir}"))
            .into()),
        Err(code) => Err(JsNativeError::error()
            .with_message(format!("fs.readdir: error {code}"))
            .into()),
    }
}

/// js: require(path) → evaluates the .tcow file at path as JS, returns its result
fn js_require(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let path = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_string(ctx)?
        .to_std_string_escaped();
    match vfs_read(&path) {
        Ok(bytes) => ctx.eval(Source::from_bytes(&bytes)),
        Err(-1) => Err(JsNativeError::error()
            .with_message(format!("require: module not found: {path}"))
            .into()),
        Err(code) => Err(JsNativeError::error()
            .with_message(format!("require: error {code} loading {path}"))
            .into()),
    }
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
struct JsExecResult {
    stdout: String,
    result: String,
    error: Option<String>,
}

fn run_js_in_boa(code: &str) -> String {
    let mut ctx = BoaContext::default();

    // ── console shim ──────────────────────────────────────────────────────────
    let setup = r#"
        var __logs = [];
        var console = { log: function() {
            var parts = [];
            for (var i = 0; i < arguments.length; i++) parts.push(String(arguments[i]));
            __logs.push(parts.join(' '));
        }};
    "#;

    if let Err(e) = ctx.eval(Source::from_bytes(setup)) {
        let r = JsExecResult {
            stdout: String::new(),
            result: String::new(),
            error: Some(format!("console setup failed: {e:?}")),
        };
        return serde_json::to_string(&r).unwrap_or_default();
    }

    // ── fs object  (backed by TCOW virtual filesystem host functions) ─────────
    let fs_obj = ObjectInitializer::new(&mut ctx)
        .function(
            NativeFunction::from_fn_ptr(js_fs_read_file),
            js_string!("readFile"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(js_fs_write_file),
            js_string!("writeFile"),
            2,
        )
        .function(
            NativeFunction::from_fn_ptr(js_fs_readdir),
            js_string!("readdir"),
            1,
        )
        .build();
    if let Err(e) = ctx.register_global_property(js_string!("fs"), fs_obj, Attribute::all()) {
        let r = JsExecResult {
            stdout: String::new(),
            result: String::new(),
            error: Some(format!("fs setup failed: {e:?}")),
        };
        return serde_json::to_string(&r).unwrap_or_default();
    }

    // ── require(path)  (load + eval a JS file from the virtual FS) ───────────
    if let Err(e) = ctx.register_global_callable(
        js_string!("require"),
        1,
        NativeFunction::from_fn_ptr(js_require),
    ) {
        let r = JsExecResult {
            stdout: String::new(),
            result: String::new(),
            error: Some(format!("require setup failed: {e:?}")),
        };
        return serde_json::to_string(&r).unwrap_or_default();
    }

    // ── run user code ─────────────────────────────────────────────────────────
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
            JsExecResult { stdout, result, error: None }
        }
        Err(e) => JsExecResult {
            stdout,
            result: String::new(),
            error: Some(format!("{e:?}")),
        },
    };

    serde_json::to_string(&r).unwrap_or_default()
}