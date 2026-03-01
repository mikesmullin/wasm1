use boa_engine::{
    js_string,
    object::{builtins::JsArray, ObjectInitializer},
    property::Attribute,
    Context as BoaContext,
    JsNativeError,
    JsObject,
    JsResult,
    JsString,
    JsValue,
    NativeFunction,
    Source,
};
use serde::{Deserialize, Serialize};

const BUF_SIZE: usize = 128 * 1024;

#[derive(Debug, Serialize)]
struct HistoryEntry {
    tool_call_id: String,
    tool_name: String,
    assistant_msg_json: String,
    result_json: String,
}


#[derive(Debug, Serialize)]
struct GuestRequest<'a> {
    prompt: &'a str,
    history: &'a [HistoryEntry],
    validation_feedback: &'a [String],
    step: u32,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum LlmDecision {
    #[serde(rename = "tool_call")]
    ToolCall {
        tool: String,
        tool_call_id: String,
        assistant_msg_json: String,
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        args: Option<serde_json::Value>,
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
    fn get_max_steps() -> i32;
    fn get_validate(out_ptr: i32, out_cap: i32) -> i32;
    fn get_max_validation_fails() -> i32;
    fn host_log(ptr: i32, len: i32);
    fn emit_final(ptr: i32, len: i32);
    fn grok_chat(req_ptr: i32, req_len: i32, out_ptr: i32, out_cap: i32) -> i32;
    // Virtual .tcow filesystem — exposed only to Boa, not to the agent loop directly
    fn fs_read(path_ptr: i32, path_len: i32, out_ptr: i32, out_cap: i32) -> i32;
    fn fs_write(path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32) -> i32;
    fn fs_list(dir_ptr: i32, dir_len: i32, out_ptr: i32, out_cap: i32) -> i32;
    // Shell execution — validated against template allow-list on the host
    fn shell_run(
        cmd_ptr: i32, cmd_len: i32,
        args_ptr: i32, args_len: i32,
        out_ptr: i32, out_cap: i32,
    ) -> i32;
    fn shell_stdin(pid: i32, keys_ptr: i32, keys_len: i32) -> i32;
    fn shell_kill(pid: i32, sig_ptr: i32, sig_len: i32) -> i32;
    // Named tool dispatch — returns JSON {"result":"..."} or {"error":"..."}
    fn tool_dispatch(
        name_ptr: i32, name_len: i32,
        args_ptr: i32, args_len: i32,
        out_ptr: i32, out_cap: i32,
    ) -> i32;
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

/// AI policy error message shown when the LLM attempts to use child_process.
const CHILD_PROCESS_POLICY_MSG: &str =
    "AI Policy Error: Use of child_process is prohibited for security reasons. \
    Please use require('shell').run(cmd, args) which returns an object { pid, path }, \
    where path points to a JSON file in the virtual filesystem containing { exit_code, stdout, stderr }.";

// ── child_process policy mock ─────────────────────────────────────────────────

/// All child_process methods (exec, spawn, etc.) call this and throw.
fn js_child_process_policy(
    _this: &JsValue,
    _args: &[JsValue],
    _ctx: &mut BoaContext,
) -> JsResult<JsValue> {
    Err(JsNativeError::error()
        .with_message(CHILD_PROCESS_POLICY_MSG)
        .into())
}

// ── shell.run / shell.stdin / shell.kill ──────────────────────────────────────

/// js: shell.run(cmd, args?) → { pid, path }  (plain object, safe to `await`)
/// Also sets shell.lastPid and shell.lastFile on the shell object.
fn js_shell_run(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let cmd = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_string(ctx)?
        .to_std_string_escaped();

    // Collect the optional args array by iterating the JsObject directly
    let mut cmd_args: Vec<String> = Vec::new();
    if let Some(arg1) = args.get(1) {
        if let Some(obj) = arg1.as_object() {
            let len = obj
                .get(js_string!("length"), ctx)
                .ok()
                .and_then(|v| v.to_u32(ctx).ok())
                .unwrap_or(0);
            for i in 0..len {
                let item = obj.get(i, ctx).unwrap_or(JsValue::undefined());
                cmd_args.push(
                    item.to_string(ctx)
                        .map(|s| s.to_std_string_escaped())
                        .unwrap_or_default(),
                );
            }
        }
    }
    let args_json = serde_json::to_string(&cmd_args).unwrap_or_else(|_| "[]".into());

    let mut out_buf = vec![0u8; 1024];
    let rc = unsafe {
        shell_run(
            cmd.as_ptr() as i32,
            cmd.len() as i32,
            args_json.as_ptr() as i32,
            args_json.len() as i32,
            out_buf.as_mut_ptr() as i32,
            out_buf.len() as i32,
        )
    };
    if rc < 0 {
        let msg = match rc {
            -1 => format!(
                "AI Policy Error: shell.run command not in allow-list: {cmd:?}. \
                 Check the template metadata.shell.allow list."
            ),
            -2 => format!("shell.run: failed to spawn process: {cmd:?}"),
            _ => format!("shell.run: host error {rc} for command: {cmd:?}"),
        };
        return Err(JsNativeError::error().with_message(msg).into());
    }

    // Host returns JSON: {"pid": N, "path": "/tmp/..."}
    let response = String::from_utf8_lossy(&out_buf[..rc as usize]).into_owned();
    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap_or_default();
    let pid = parsed["pid"].as_u64().unwrap_or(0);
    let path = parsed["path"].as_str().unwrap_or("").to_string();

    // Set lastPid and lastFile on the shell object (_this when called as shell.run(...))
    if let Some(obj) = _this.as_object() {
        let _ = obj.set(js_string!("lastPid"), JsValue::from(pid as f64), false, ctx);
        let _ = obj.set(
            js_string!("lastFile"),
            JsValue::from(JsString::from(path.as_str())),
            false,
            ctx,
        );
    }

    // Return a plain (non-thenable) object so that:
    //   const r = shell.run(...)        → works directly
    //   const r = await shell.run(...)  → await wraps in already-resolved Promise,
    //                                     one run_jobs() pass resumes with {pid,path}
    // If .then existed (thenable), await would create a PromiseResolveThenableJob
    // that resolves to just the callback arg (the path string), losing the object.
    let path_json = serde_json::to_string(&path).unwrap_or_else(|_| "\"\"".into());
    let code = format!("({{pid:{pid},path:{path_json}}})");
    ctx.eval(Source::from_bytes(code.as_bytes()))
}

/// js: shell.stdin(pid, sendkeys) → undefined
fn js_shell_stdin(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let pid = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_number(ctx)? as i32;
    let keys = args
        .get(1)
        .unwrap_or(&JsValue::undefined())
        .to_string(ctx)?
        .to_std_string_escaped();
    let rc = unsafe { shell_stdin(pid, keys.as_ptr() as i32, keys.len() as i32) };
    match rc {
        0 => Ok(JsValue::undefined()),
        -1 => Err(JsNativeError::error()
            .with_message("shell.stdin: PID not found or already ended")
            .into()),
        -2 => Err(JsNativeError::error()
            .with_message("shell.stdin: write to child stdin failed")
            .into()),
        -3 => Err(JsNativeError::error()
            .with_message("shell.stdin: PID is not a child of this session")
            .into()),
        _ => Err(JsNativeError::error()
            .with_message(format!("shell.stdin: error {rc}"))
            .into()),
    }
}

/// js: shell.kill(pid, signal?) → undefined
fn js_shell_kill(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let pid = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_number(ctx)? as i32;
    let signal = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|| "SIGTERM".into());
    let rc = unsafe { shell_kill(pid, signal.as_ptr() as i32, signal.len() as i32) };
    match rc {
        0 => Ok(JsValue::undefined()),
        -1 => Err(JsNativeError::error()
            .with_message("shell.kill: PID not found or already ended")
            .into()),
        -2 => Err(JsNativeError::error()
            .with_message("shell.kill: kill syscall failed")
            .into()),
        -3 => Err(JsNativeError::error()
            .with_message("shell.kill: PID not a child of this session")
            .into()),
        -4 => Err(JsNativeError::error()
            .with_message(format!("shell.kill: invalid signal '{signal}'"))
            .into()),
        _ => Err(JsNativeError::error()
            .with_message(format!("shell.kill: error {rc}"))
            .into()),
    }
}
/// If the last argument is a callable function, return it.
fn last_callable(args: &[JsValue]) -> Option<JsObject> {
    args.last()
        .and_then(|v| v.as_object())
        .filter(|o| o.is_callable())
}

// ── fs shims ──────────────────────────────────────────────────────────────────
// Each async variant (readFile, writeFile, readdir) matches Node.js v18 signature:
//   readFile(path[, options][, callback])
// If a callback is supplied it is called synchronously as cb(err, data).
// If no callback is supplied the result is returned directly (sync convenience).
// The Sync variants always return the value / throw — no callback.

/// js: fs.readFileSync(path[, options]) → string  (throws on error)
fn js_fs_read_file_sync(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let path = args.first().unwrap_or(&JsValue::undefined()).to_string(ctx)?.to_std_string_escaped();
    match vfs_read(&path) {
        Ok(bytes) => Ok(JsValue::from(JsString::from(
            String::from_utf8_lossy(&bytes).as_ref(),
        ))),
        Err(-1) => Err(JsNativeError::error()
            .with_message(format!("ENOENT: no such file or directory, open '{path}'"))
            .into()),
        Err(code) => Err(JsNativeError::error()
            .with_message(format!("fs.readFileSync: error {code} reading '{path}'"))
            .into()),
    }
}

/// js: fs.readFile(path[, options][, callback])
/// callback signature: (err, data: string) → void
fn js_fs_read_file(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let path = args.first().unwrap_or(&JsValue::undefined()).to_string(ctx)?.to_std_string_escaped();
    let cb = last_callable(args);
    match vfs_read(&path) {
        Ok(bytes) => {
            let data = JsValue::from(JsString::from(
                String::from_utf8_lossy(&bytes).as_ref(),
            ));
            if let Some(cb_obj) = cb {
                cb_obj.call(&JsValue::undefined(), &[JsValue::null(), data], ctx)?;
                Ok(JsValue::undefined())
            } else {
                Ok(data)
            }
        }
        Err(code) => {
            let msg = if code == -1 {
                format!("ENOENT: no such file or directory, open '{path}'")
            } else {
                format!("fs.readFile: error {code} reading '{path}'")
            };
            if let Some(cb_obj) = cb {
                let err_val = JsValue::from(JsString::from(msg.as_str()));
                cb_obj.call(&JsValue::undefined(), &[err_val], ctx)?;
                Ok(JsValue::undefined())
            } else {
                Err(JsNativeError::error().with_message(msg).into())
            }
        }
    }
}

/// js: fs.writeFileSync(path, data[, options]) → undefined  (throws on error)
fn js_fs_write_file_sync(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let path = args.first().unwrap_or(&JsValue::undefined()).to_string(ctx)?.to_std_string_escaped();
    let content = args.get(1).unwrap_or(&JsValue::undefined()).to_string(ctx)?.to_std_string_escaped();
    match vfs_write(&path, content.as_bytes()) {
        Ok(()) => Ok(JsValue::undefined()),
        Err(code) => Err(JsNativeError::error()
            .with_message(format!("fs.writeFileSync: error {code} writing '{path}'"))
            .into()),
    }
}

/// js: fs.writeFile(path, data[, options][, callback])
/// callback signature: (err) → void
fn js_fs_write_file(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let path = args.first().unwrap_or(&JsValue::undefined()).to_string(ctx)?.to_std_string_escaped();
    let content = args.get(1).unwrap_or(&JsValue::undefined()).to_string(ctx)?.to_std_string_escaped();
    let cb = last_callable(args);
    match vfs_write(&path, content.as_bytes()) {
        Ok(()) => {
            if let Some(cb_obj) = cb {
                cb_obj.call(&JsValue::undefined(), &[JsValue::null()], ctx)?;
            }
            Ok(JsValue::undefined())
        }
        Err(code) => {
            let msg = format!("fs.writeFile: error {code} writing '{path}'");
            if let Some(cb_obj) = cb {
                let err_val = JsValue::from(JsString::from(msg.as_str()));
                cb_obj.call(&JsValue::undefined(), &[err_val], ctx)?;
                Ok(JsValue::undefined())
            } else {
                Err(JsNativeError::error().with_message(msg).into())
            }
        }
    }
}

/// js: fs.readdirSync(path[, options]) → Array<string>  (throws on error)
fn js_fs_readdir_sync(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let dir = args.first().unwrap_or(&JsValue::undefined()).to_string(ctx)?.to_std_string_escaped();
    match vfs_list(&dir) {
        Ok(listing) => {
            let arr = JsArray::new(ctx);
            for entry in listing.lines().filter(|s| !s.is_empty()) {
                arr.push(JsValue::from(JsString::from(entry)), ctx)?;
            }
            Ok(arr.into())
        }
        Err(-1) => Err(JsNativeError::error()
            .with_message(format!("ENOENT: no such file or directory, scandir '{dir}'"))
            .into()),
        Err(code) => Err(JsNativeError::error()
            .with_message(format!("fs.readdirSync: error {code}"))
            .into()),
    }
}

/// js: fs.readdir(path[, options][, callback])
/// callback signature: (err, entries: Array<string>) → void
fn js_fs_readdir(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let dir = args.first().unwrap_or(&JsValue::undefined()).to_string(ctx)?.to_std_string_escaped();
    let cb = last_callable(args);
    match vfs_list(&dir) {
        Ok(listing) => {
            let arr = JsArray::new(ctx);
            for entry in listing.lines().filter(|s| !s.is_empty()) {
                arr.push(JsValue::from(JsString::from(entry)), ctx)?;
            }
            let arr_val: JsValue = arr.into();
            if let Some(cb_obj) = cb {
                cb_obj.call(&JsValue::undefined(), &[JsValue::null(), arr_val], ctx)?;
                Ok(JsValue::undefined())
            } else {
                Ok(arr_val)
            }
        }
        Err(code) => {
            let msg = if code == -1 {
                format!("ENOENT: no such file or directory, scandir '{dir}'")
            } else {
                format!("fs.readdir: error {code}")
            };
            if let Some(cb_obj) = cb {
                let err_val = JsValue::from(JsString::from(msg.as_str()));
                cb_obj.call(&JsValue::undefined(), &[err_val], ctx)?;
                Ok(JsValue::undefined())
            } else {
                Err(JsNativeError::error().with_message(msg).into())
            }
        }
    }
}

fn make_fs_obj(ctx: &mut BoaContext) -> JsValue {
    JsValue::from(
        ObjectInitializer::new(ctx)
            .function(NativeFunction::from_fn_ptr(js_fs_read_file),      js_string!("readFile"),      3)
            .function(NativeFunction::from_fn_ptr(js_fs_read_file_sync), js_string!("readFileSync"), 2)
            .function(NativeFunction::from_fn_ptr(js_fs_write_file),      js_string!("writeFile"),     4)
            .function(NativeFunction::from_fn_ptr(js_fs_write_file_sync), js_string!("writeFileSync"), 3)
            .function(NativeFunction::from_fn_ptr(js_fs_readdir),         js_string!("readdir"),       3)
            .function(NativeFunction::from_fn_ptr(js_fs_readdir_sync),    js_string!("readdirSync"),   2)
            .build(),
    )
}

fn make_shell_obj(ctx: &mut BoaContext) -> JsValue {
    JsValue::from(
        ObjectInitializer::new(ctx)
            .function(NativeFunction::from_fn_ptr(js_shell_run), js_string!("run"), 2)
            .function(NativeFunction::from_fn_ptr(js_shell_stdin), js_string!("stdin"), 2)
            .function(NativeFunction::from_fn_ptr(js_shell_kill), js_string!("kill"), 2)
            .build(),
    )
}

/// js: require(path) → evaluates the .tcow file at path as JS, returns its result.
/// Special cases:
///   require('child_process') → AI policy error
///   require('shell')         → returns a fresh shell object (same API as global was)
fn js_require(_this: &JsValue, args: &[JsValue], ctx: &mut BoaContext) -> JsResult<JsValue> {
    let path = args
        .first()
        .unwrap_or(&JsValue::undefined())
        .to_string(ctx)?
        .to_std_string_escaped();

    // Intercept well-known module names before any VFS lookup
    match path.as_str() {
        "child_process" => {
            return Err(JsNativeError::error()
                .with_message(CHILD_PROCESS_POLICY_MSG)
                .into());
        }
        "shell" => {
            return Ok(make_shell_obj(ctx));
        }
        "fs" => {
            return Ok(make_fs_obj(ctx));
        }
        _ => {}
    }

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

    let mut history: Vec<HistoryEntry> = Vec::new();
    let mut validation_feedback: Vec<String> = Vec::new();
    let mut validation_fail_count: u32 = 0;
    let validate_fn = read_validate()?;
    let max_validation_fails = unsafe { get_max_validation_fails() };
    // max_steps: -1 = unlimited (default); >=0 = hard cap from template spec.max_steps
    let max_steps: i32 = unsafe { get_max_steps() };
    let mut step: u32 = 0;

    loop {
        if max_steps >= 0 && step >= max_steps as u32 {
            emit_final_line("Stopped: reached max_steps limit.");
            return Ok(());
        }
        let req = GuestRequest {
            prompt: &prompt,
            history: &history,
            validation_feedback: &validation_feedback,
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
                tool_call_id,
                assistant_msg_json,
                code,
                args,
            } => {
                log_line(&format!("Tool call requested: {tool}"));
                let result = if tool == "js_exec" {
                    let src = code
                        .or_else(|| {
                            args.as_ref()
                                .and_then(|a| a["code"].as_str())
                                .map(str::to_string)
                        })
                        .unwrap_or_default();
                    run_js_in_boa(&src)
                } else {
                    let args_str = args
                        .as_ref()
                        .map(|a| a.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    call_tool_dispatch(&tool, &args_str)
                };
                log_line(&format!("Tool result: {result}"));
                history.push(HistoryEntry {
                    tool_call_id,
                    tool_name: tool,
                    assistant_msg_json,
                    result_json: result,
                });
            }
            LlmDecision::Final { answer, thought } => {
                if let Some(t) = thought {
                    log_line(&format!("Model thought: {t}"));
                }
                if !validate_fn.trim().is_empty() {
                    match evaluate_validate(&validate_fn, &answer) {
                        Ok((true, _)) => {
                            emit_final_line(&answer);
                            return Ok(());
                        }
                        Ok((false, result_text)) => {
                            validation_fail_count += 1;
                            log_line(&format!(
                                "Validation failed ({}/{})",
                                validation_fail_count,
                                if max_validation_fails >= 0 {
                                    max_validation_fails.to_string()
                                } else {
                                    "∞".to_string()
                                }
                            ));
                            if max_validation_fails >= 0
                                && validation_fail_count >= max_validation_fails as u32
                            {
                                let msg = format!(
                                    "No answer could be returned; the LLM failed to construct an answer that could pass validation in ({}) attempts.",
                                    max_validation_fails
                                );
                                emit_final_line(&msg);
                                return Ok(());
                            }
                            let validation_msg = format!(
                                "Your reply failed validation because the validation function returned: {}. Please review the javascript validation function code provided, and adapt your reply to conform strictly.",
                                result_text
                            );
                            validation_feedback.push(validation_msg);
                        }
                        Err(err) => {
                            validation_fail_count += 1;
                            if max_validation_fails >= 0
                                && validation_fail_count >= max_validation_fails as u32
                            {
                                let msg = format!(
                                    "No answer could be returned; the LLM failed to construct an answer that could pass validation in ({}) attempts.",
                                    max_validation_fails
                                );
                                emit_final_line(&msg);
                                return Ok(());
                            }
                            let validation_msg = format!(
                                "Your reply failed validation because the validation function returned: \"Validation error: {}\". Please review the javascript validation function code provided, and adapt your reply to conform strictly.",
                                err
                            );
                            validation_feedback.push(validation_msg);
                        }
                    }
                } else {
                    emit_final_line(&answer);
                    return Ok(());
                }
            }
            LlmDecision::Error { message } => {
                return Err(format!("llm error: {message}"));
            }
        }
        step += 1;
    }
}

fn read_validate() -> Result<String, String> {
    let mut out = vec![0u8; BUF_SIZE];
    let written = unsafe { get_validate(out.as_mut_ptr() as i32, out.len() as i32) };
    if written < 0 {
        return Err(format!("get_validate failed with {written}"));
    }
    String::from_utf8(out[..written as usize].to_vec()).map_err(|e| e.to_string())
}

fn evaluate_validate(validate_fn: &str, reply: &str) -> Result<(bool, String), String> {
    let mut ctx = BoaContext::default();
    let validate_src = serde_json::to_string(validate_fn).map_err(|e| e.to_string())?;
    let reply_src = serde_json::to_string(reply).map_err(|e| e.to_string())?;
    let script = format!(
        "(function(){{ const __fn = new Function('reply', {validate_src}); return __fn({reply_src}); }})()"
    );
    let value = ctx
        .eval(Source::from_bytes(script.as_bytes()))
        .map_err(|e| format!("{e}"))?;
    let passed = value.to_boolean();
    let rendered = value
        .to_string(&mut ctx)
        .map(|s| s.to_std_string_escaped())
        .unwrap_or_else(|_| "false".to_string());
    Ok((passed, rendered))
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
        serde_json::to_string(&JsExecResult {
            stdout: String::new(),
            result: String::new(),
            error: Some(err_msg),
        })
        .unwrap_or_else(|_| r#"{"stdout":"","result":"","error":"dispatch failed"}"#.to_string())
    } else {
        let raw = String::from_utf8_lossy(&out[..written as usize]).into_owned();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
        if let Some(r) = parsed["result"].as_str() {
            serde_json::to_string(&JsExecResult {
                stdout: String::new(),
                result: r.to_string(),
                error: None,
            })
            .unwrap_or_else(|_| r#"{"stdout":"","result":"","error":null}"#.to_string())
        } else if let Some(e) = parsed["error"].as_str() {
            serde_json::to_string(&JsExecResult {
                stdout: String::new(),
                result: String::new(),
                error: Some(e.to_string()),
            })
            .unwrap_or_else(|_| r#"{"stdout":"","result":"","error":"dispatch error"}"#.to_string())
        } else {
            serde_json::to_string(&JsExecResult {
                stdout: String::new(),
                result: raw,
                error: None,
            })
            .unwrap_or_default()
        }
    }
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
    let fs_obj = make_fs_obj(&mut ctx);
    if let Err(e) = ctx.register_global_property(js_string!("fs"), fs_obj, Attribute::all()) {
        let r = JsExecResult {
            stdout: String::new(),
            result: String::new(),
            error: Some(format!("fs setup failed: {e:?}")),
        };
        return serde_json::to_string(&r).unwrap_or_default();
    }

    // ── child_process mock (policy object — all methods throw) ────────────────
    let cp_obj = ObjectInitializer::new(&mut ctx)
        .function(
            NativeFunction::from_fn_ptr(js_child_process_policy),
            js_string!("exec"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(js_child_process_policy),
            js_string!("execSync"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(js_child_process_policy),
            js_string!("spawn"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(js_child_process_policy),
            js_string!("spawnSync"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(js_child_process_policy),
            js_string!("execFile"),
            0,
        )
        .function(
            NativeFunction::from_fn_ptr(js_child_process_policy),
            js_string!("fork"),
            0,
        )
        .build();
    if let Err(e) =
        ctx.register_global_property(js_string!("child_process"), cp_obj, Attribute::all())
    {
        let r = JsExecResult {
            stdout: String::new(),
            result: String::new(),
            error: Some(format!("child_process setup failed: {e:?}")),
        };
        return serde_json::to_string(&r).unwrap_or_default();
    }

    // ── shell object available only via require('shell') ─────────────────────
    // (not a global — agent must: const shell = require('shell'))

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

    // Drain the Promise job queue. Boa's SimpleJobQueue::run_jobs loops internally
    // until the queue is empty, so one call handles all chained awaits:
    //   await shell.run() → PromiseReactionJob → resume → next await → ... → done.
    // We call it twice as a belt-and-suspenders: in case a job enqueued during the
    // first drain itself enqueues more jobs that slipped through.
    let _ = ctx.run_jobs();
    let _ = ctx.run_jobs();

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