use anyhow::{anyhow, Context, Result};
use dotenvy::from_filename;
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tcow::TcowFile;
use wasmtime::{Caller, Config, Engine, Linker, Module, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, add_to_linker as wasi_add_to_linker};

const DEFAULT_MODEL: &str = "grok-4-1-fast-reasoning";
const GUEST_WASM_PATH: &str = "guest/target/wasm32-wasip1/debug/guest.wasm";
const FUEL_LIMIT: u64 = 2_000_000_000;
/// `None` = wait indefinitely (default when no template or template omits timeout_secs).
const SHELL_TIMEOUT_DEFAULT: Option<u64> = None;

// ── Template YAML structs ─────────────────────────────────────────────────────
#[derive(Debug, Deserialize, Default)]
struct Template {
    metadata: Option<TemplateMetadata>,
    spec: Option<TemplateSpec>,
}

#[derive(Debug, Deserialize, Default)]
struct TemplateMetadata {
    #[allow(dead_code)]
    description: Option<String>,
    #[allow(dead_code)]
    model: Option<String>,
    /// Fallback context window size in tokens, used if the model API lookup fails.
    context_window: Option<u64>,
    shell: Option<ShellConfig>,
    /// Explicit tool allowlist for the session. Absent = all tools.
    tools: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
struct ShellConfig {
    allow: Option<Vec<String>>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct TemplateSpec {
    system_prompt: Option<String>,
    max_steps: Option<u32>,
}

// ── Shell output YAML ─────────────────────────────────────────────────────────
#[derive(Debug, Serialize)]
struct ShellOut {
    pid: u32,
    status: String,
    cmd: String,
    args: Vec<String>,
    started_ms: u64,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
}

struct HostState {
    prompt: String,
    final_answer: Option<String>,
    api_key: String,
    model: String,
    client: Client,
    wasi: WasiCtx,
    /// Path to the persistent .tcow virtual filesystem file.
    tcow_path: String,
    /// Writes buffered in memory during this run; flushed after guest returns.
    pending_writes: Vec<(String, Vec<u8>)>,
    /// Compiled allow-list from template metadata.shell.allow.
    /// Empty vec = all shell commands denied.
    shell_allow: Vec<Regex>,
    /// Wall-clock timeout for shell commands; `None` = wait indefinitely.
    shell_timeout: Option<u64>,
    /// Live child processes spawned this session, keyed by PID.
    running_processes: HashMap<u32, Child>,
    /// System prompt from template spec.system_prompt, if any.
    system_prompt: Option<String>,
    /// Maximum agent loop steps; None = unlimited.
    max_steps: Option<u32>,
    /// Tool names available to the LLM (controls tools_json).
    #[allow(dead_code)]
    enabled_tools: Vec<String>,
    /// Known context window size for the model (tokens); None = unknown.
    context_window: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct HistoryEntry {
    tool_call_id: String,
    tool_name: String,
    assistant_msg_json: String,
    result_json: String,
}

#[derive(Debug, Deserialize)]
struct GuestRequest {
    prompt: String,
    history: Vec<HistoryEntry>,
    step: u32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum LlmDecision {
    #[serde(rename = "tool_call")]
    ToolCall {
        tool: String,
        /// xAI tool call ID — stored in history for role:tool reply.
        tool_call_id: String,
        /// Serialized assistant message (with tool_calls) to replay in history.
        assistant_msg_json: String,
        /// JS source — only for js_exec.
        #[serde(default)]
        code: Option<String>,
        /// Structured args — for non-js_exec tools.
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

fn resolve_template(name: &str) -> Result<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        if p.exists() {
            return Ok(p.to_path_buf());
        } else {
            return Err(anyhow!("template not found: {name}"));
        }
    }
    let basename = if name.ends_with(".yaml") {
        name.to_string()
    } else {
        format!("{name}.yaml")
    };
    let home = env::var("HOME").unwrap_or_default();
    let candidates = [
        PathBuf::from(".agent/templates").join(&basename),
        PathBuf::from(&home)
            .join(".config/daemon/agent/templates")
            .join(&basename),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }
    Err(anyhow!(
        "template '{name}' not found in .agent/templates/ or ~/.config/daemon/agent/templates/"
    ))
}

fn load_template(path: &Path) -> Result<(Vec<Regex>, Option<u64>, Option<String>, Option<u32>, Vec<String>, Option<u64>)> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read template: {}", path.display()))?;
    let template: Template = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse template YAML: {}", path.display()))?;
    let shell_cfg = template
        .metadata
        .as_ref()
        .and_then(|m| m.shell.as_ref());
    let allow_patterns = shell_cfg
        .and_then(|s| s.allow.as_ref())
        .cloned()
        .unwrap_or_default();
    let timeout = shell_cfg.and_then(|s| s.timeout_secs);
    let regexes = allow_patterns
        .iter()
        .map(|pat| Regex::new(pat).with_context(|| format!("invalid regex in template: {pat}")))
        .collect::<Result<Vec<_>>>()?;
    let system_prompt = template.spec.as_ref().and_then(|s| s.system_prompt.clone());
    let max_steps = template.spec.as_ref().and_then(|s| s.max_steps);
    // tools: absent → all built-in tools; explicit list → only those
    let template_context_window = template.metadata.as_ref().and_then(|m| m.context_window);
    let tools = template.metadata
        .and_then(|m| m.tools)
        .unwrap_or_else(all_tool_names);
    println!(
        "[HOST] Template loaded: {} shell allow-list entries, timeout: {}, system_prompt: {}, max_steps: {}, tools: [{}], context_window: {}",
        regexes.len(),
        timeout.map(|t| format!("{t}s")).unwrap_or_else(|| "indefinite".into()),
        system_prompt.as_deref().map(|s| format!("{} chars", s.len())).unwrap_or_else(|| "none".into()),
        max_steps.map(|n| n.to_string()).unwrap_or_else(|| "indefinite".into()),
        tools.join(", "),
        template_context_window.map(|n| format!("{n}")).unwrap_or_else(|| "unset".into()),
    );
    Ok((regexes, timeout, system_prompt, max_steps, tools, template_context_window))
}

/// Query the xAI models endpoint to discover the context window for `model`.
/// Returns `None` on any error (network failure, unknown model, parse error).
/// Return the known context-window size (tokens) for an xAI model by name.
/// The xAI `/v1/models` endpoint does not expose this field, so we use a
/// static table. The template `metadata.context_window` overrides this.
/// Source: https://docs.x.ai/docs/models (as of early 2026)
fn lookup_model_context_window(model: &str) -> Option<u64> {
    // Strip optional "xai:" provider prefix before matching.
    let name = model.strip_prefix("xai:").unwrap_or(model);
    // Match longest prefix first so "grok-4-fast-reasoning" beats "grok-4".
    let window = if name.starts_with("grok-4-1-fast") {
        2_000_000   // grok-4-1-fast-reasoning, grok-4-1-fast-non-reasoning
    } else if name.starts_with("grok-4-fast") {
        2_000_000   // grok-4-fast, grok-4-fast-reasoning
    } else if name.starts_with("grok-4") {
        256_000     // grok-4, grok-4-0709
    } else if name.starts_with("grok-3-mini") {
        131_072     // grok-3-mini, grok-3-mini-fast
    } else if name.starts_with("grok-3") {
        1_000_000   // grok-3
    } else if name.starts_with("grok-2") {
        32_768      // grok-2-vision-1212 etc.
    } else {
        return None;
    };
    Some(window)
}

fn main() -> Result<()> {
    let _ = from_filename(".env");

    // Parse CLI args: [-t <template>] <prompt>
    let mut args_iter = env::args().skip(1);
    let mut template_name: Option<String> = None;
    let mut prompt: Option<String> = None;
    while let Some(arg) = args_iter.next() {
        if arg == "-t" || arg == "--template" {
            template_name = args_iter.next();
        } else {
            prompt = Some(arg);
        }
    }
    let prompt =
        prompt.ok_or_else(|| anyhow!("usage: cargo run -- [-t <template>] \"<prompt>\""))?;

    // Load template allow-list if -t was supplied
    let (shell_allow, shell_timeout, system_prompt, max_steps, enabled_tools, template_context_window) = if let Some(ref name) = template_name {
        let path = resolve_template(name)?;
        println!("[HOST] Using template: {}", path.display());
        load_template(&path)?
    } else {
        (Vec::new(), SHELL_TIMEOUT_DEFAULT, None, None, all_tool_names(), None)
    };

    println!("[HOST] Starting agent with prompt: {:?}", prompt);

    let api_key = env::var("XAI_API_KEY")
        .context("XAI_API_KEY is required (set it in environment or .env)")?;
    let model = env::var("XAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let client = Client::new();
    let context_window = lookup_model_context_window(&model)
        .or(template_context_window);
    println!(
        "[HOST] Model: {model} | API key: loaded | context_window: {}",
        context_window.map(|n| format!("{n} tokens")).unwrap_or_else(|| "unknown (set metadata.context_window in template)".into()),
    );

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
        "get_max_steps",
        |caller: Caller<'_, HostState>| -> i32 {
            caller.data().max_steps.map(|n| n as i32).unwrap_or(-1)
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

    // ── fs_read: resolve path through union view of .tcow + pending writes ────
    linker.func_wrap(
        "host",
        "fs_read",
        |mut caller: Caller<'_, HostState>,
         path_ptr: i32,
         path_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            let path = match read_memory(&mut caller, path_ptr, path_len) {
                Ok(p) => p.trim_start_matches('/').trim_start_matches("./").to_string(),
                Err(_) => return -3,
            };
            // pending writes shadow disk: check most-recent first
            let pending_hit = caller
                .data()
                .pending_writes
                .iter()
                .rev()
                .find(|(p, _)| p == &path)
                .map(|(_, d)| d.clone());

            let bytes = if let Some(data) = pending_hit {
                data
            } else {
                let tcow_path = caller.data().tcow_path.clone();
                if !Path::new(&tcow_path).exists() {
                    return -1;
                }
                match TcowFile::open(&tcow_path) {
                    Err(_) => return -4,
                    Ok(tf) => match tf.resolve(&path) {
                        None => return -1,
                        Some((entry, _)) => entry.data,
                    },
                }
            };

            if bytes.len() > out_cap as usize {
                return -2;
            }
            write_memory_bytes(&mut caller, out_ptr, &bytes)
        },
    )?;

    // ── fs_write: buffer write; flushed to .tcow after guest returns ──────────
    linker.func_wrap(
        "host",
        "fs_write",
        |mut caller: Caller<'_, HostState>,
         path_ptr: i32,
         path_len: i32,
         data_ptr: i32,
         data_len: i32|
         -> i32 {
            let path = match read_memory(&mut caller, path_ptr, path_len) {
                Ok(p) => p.trim_start_matches('/').trim_start_matches("./").to_string(),
                Err(_) => return -3,
            };
            let data = read_memory_bytes(&mut caller, data_ptr, data_len);
            caller.data_mut().pending_writes.push((path, data));
            0
        },
    )?;

    // ── fs_list: newline-delimited entries visible under a directory ──────────
    linker.func_wrap(
        "host",
        "fs_list",
        |mut caller: Caller<'_, HostState>,
         dir_ptr: i32,
         dir_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            let dir = match read_memory(&mut caller, dir_ptr, dir_len) {
                Ok(p) => {
                    let s = p.trim_start_matches('/').trim_end_matches('/').to_string();
                    if s == "." { String::new() } else { s }
                }
                Err(_) => return -3,
            };
            let prefix = if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            };

            let tcow_path = caller.data().tcow_path.clone();
            let pending = caller.data().pending_writes.clone();

            let mut visible: std::collections::HashMap<String, ()> =
                std::collections::HashMap::new();

            if Path::new(&tcow_path).exists() {
                if let Ok(tf) = TcowFile::open(&tcow_path) {
                    for (path, _) in tf.union_view() {
                        if path.starts_with(&prefix) {
                            let rest = &path[prefix.len()..];
                            if !rest.is_empty() && !rest.contains('/') {
                                visible.insert(rest.to_string(), ());
                            }
                        }
                    }
                }
            }
            for (path, _) in &pending {
                if path.starts_with(&prefix) {
                    let rest = &path[prefix.len()..];
                    if !rest.is_empty() && !rest.contains('/') {
                        visible.insert(rest.to_string(), ());
                    }
                }
            }

            let mut names: Vec<_> = visible.into_keys().collect();
            names.sort();
            let result = names.join("\n");
            write_memory(&mut caller, out_ptr, out_cap, &result)
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

            if let LlmDecision::ToolCall { tool, tool_call_id, .. } = &decision {
                println!("[LLM → GUEST] Tool call: {tool} (id={tool_call_id})");
            }

            let response = serde_json::to_string(&decision).unwrap_or_else(|_| {
                "{\"type\":\"error\",\"message\":\"serialization failed\"}".to_string()
            });
            write_memory(&mut caller, out_ptr, out_cap, &response)
        },
    )?;

    // ── shell_run ─────────────────────────────────────────────────────────────
    linker.func_wrap(
        "host",
        "shell_run",
        |mut caller: Caller<'_, HostState>,
         cmd_ptr: i32,
         cmd_len: i32,
         args_ptr: i32,
         args_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            let cmd = match read_memory(&mut caller, cmd_ptr, cmd_len) {
                Ok(c) => c,
                Err(_) => return -3,
            };
            let args_json = match read_memory(&mut caller, args_ptr, args_len) {
                Ok(j) => j,
                Err(_) => return -3,
            };
            let args: Vec<String> =
                serde_json::from_str(&args_json).unwrap_or_default();

            // Allow-list check
            let full_cmd = if args.is_empty() {
                cmd.clone()
            } else {
                format!("{cmd} {}", args.join(" "))
            };
            let allowed = caller
                .data()
                .shell_allow
                .iter()
                .any(|re| re.is_match(&full_cmd));
            if !allowed {
                println!(
                    "[HOST] shell_run: command denied by allow-list: {full_cmd:?}"
                );
                return -1;
            }

            // Generate output path
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let hash_input = format!("{full_cmd}\t{now_ms}");
            let mut hasher = Sha1::new();
            hasher.update(hash_input.as_bytes());
            let sha_bytes = hasher.finalize();
            let sha_hex: String =
                sha_bytes.iter().take(3).map(|b| format!("{b:02x}")).collect();
            // Store in virtual FS under "tmp/..." (leading slash stripped)
            let vfs_path = format!("tmp/{}_{}.out.json", now_ms, sha_hex);
            let guest_path = format!("/tmp/{}_{}.out.json", now_ms, sha_hex);

            // Write initial YAML to pending_writes
            let initial = ShellOut {
                pid: 0,
                status: "running".into(),
                cmd: cmd.clone(),
                args: args.clone(),
                started_ms: now_ms,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                elapsed_ms: None,
                timeout_secs: None,
            };
            let initial_json = serde_json::to_string(&initial)
                .unwrap_or_else(|_| "{\"status\":\"running\"}".into());
            caller
                .data_mut()
                .pending_writes
                .push((vfs_path.clone(), initial_json.into_bytes()));

            println!("[HOST] shell_run: spawning {:?} {:?}", cmd, args);
            let start = Instant::now();

            // Spawn child
            let child_result = Command::new(&cmd)
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();

            let child = match child_result {
                Err(e) => {
                    println!("[HOST] shell_run: spawn failed: {e}");
                    return -2;
                }
                Ok(c) => c,
            };

            let pid = child.id();
            // Register in session for shell_stdin / shell_kill
            caller.data_mut().running_processes.insert(pid, child);
            // Take it back out to wait (single-threaded: nothing else running)
            let mut child = caller
                .data_mut()
                .running_processes
                .remove(&pid)
                .unwrap();

            // Wait for process, with optional timeout
            let shell_timeout = caller.data().shell_timeout;
            drop(child.stdin.take()); // close stdin so child isn't waiting on input

            let output_result = std::thread::scope(|s| {
                let handle = s.spawn(|| child.wait_with_output());
                let wait_start = Instant::now();
                loop {
                    if handle.is_finished() {
                        return handle
                            .join()
                            .ok()
                            .and_then(|r| r.ok())
                            .map(|o| (o, false));
                    }
                    if let Some(secs) = shell_timeout {
                        if wait_start.elapsed() >= Duration::from_secs(secs) {
                            return None;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });

            let elapsed_ms = start.elapsed().as_millis() as u64;

            let final_out = match output_result {
                Some((output, _)) => ShellOut {
                    pid,
                    status: "ended".into(),
                    cmd: cmd.clone(),
                    args: args.clone(),
                    started_ms: now_ms,
                    exit_code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    elapsed_ms: Some(elapsed_ms),
                    timeout_secs: None,
                },
                None => {
                    let t = shell_timeout.unwrap_or(0);
                    println!("[HOST] shell_run: timed out after {t}s");
                    ShellOut {
                        pid,
                        status: "timeout".into(),
                        cmd: cmd.clone(),
                        args: args.clone(),
                        started_ms: now_ms,
                        exit_code: Some(-1),
                        stdout: String::new(),
                        stderr: format!("Command timed out after {t}s"),
                        elapsed_ms: Some(elapsed_ms),
                        timeout_secs: shell_timeout,
                    }
                }
            };

            let final_json = serde_json::to_string(&final_out)
                .unwrap_or_else(|_| "{\"status\":\"ended\"}".into());

            // Update the .out.json entry in pending_writes (push a new shadow entry)
            caller
                .data_mut()
                .pending_writes
                .push((vfs_path, final_json.into_bytes()));

            println!(
                "[HOST] shell_run: exit_code={:?} elapsed={elapsed_ms}ms",
                final_out.exit_code
            );

            // Return JSON so the guest can read both pid and path in one call
            let response = format!("{{\"pid\":{pid},\"path\":\"{guest_path}\"}}");
            write_memory(&mut caller, out_ptr, out_cap, &response)
        },
    )?;

    // ── shell_stdin ───────────────────────────────────────────────────────────
    linker.func_wrap(
        "host",
        "shell_stdin",
        |mut caller: Caller<'_, HostState>,
         pid: i32,
         keys_ptr: i32,
         keys_len: i32|
         -> i32 {
            let pid_u32 = pid as u32;
            if !caller.data().running_processes.contains_key(&pid_u32) {
                return -1; // PID not found / already ended
            }
            let data = read_memory_bytes(&mut caller, keys_ptr, keys_len);
            let child = match caller.data_mut().running_processes.get_mut(&pid_u32) {
                Some(c) => c,
                None => return -1,
            };
            match child.stdin.as_mut() {
                Some(stdin) => {
                    if stdin.write_all(&data).is_err() {
                        return -2;
                    }
                    let _ = stdin.flush();
                    0
                }
                None => -2,
            }
        },
    )?;

    // ── shell_kill ────────────────────────────────────────────────────────────
    linker.func_wrap(
        "host",
        "shell_kill",
        |mut caller: Caller<'_, HostState>, pid: i32, sig_ptr: i32, sig_len: i32| -> i32 {
            let pid_u32 = pid as u32;
            // Validate signal name first so -4 is returned even for unknown PIDs
            let sig_name = match read_memory(&mut caller, sig_ptr, sig_len) {
                Ok(s) if s.is_empty() => "SIGTERM".to_string(),
                Ok(s) => s,
                Err(_) => "SIGTERM".to_string(),
            };
            let signum: libc::c_int = match sig_name.as_str() {
                "SIGTERM" => libc::SIGTERM,
                "SIGKILL" => libc::SIGKILL,
                "SIGINT" => libc::SIGINT,
                "SIGHUP" => libc::SIGHUP,
                _ => return -4,
            };
            if !caller.data().running_processes.contains_key(&pid_u32) {
                return -1; // PID not found / already ended
            }
            let rc = unsafe { libc::kill(pid_u32 as libc::pid_t, signum) };
            if rc != 0 {
                return -2;
            }
            // Collect exit code non-blocking
            let exit_code = caller
                .data_mut()
                .running_processes
                .get_mut(&pid_u32)
                .and_then(|c| c.try_wait().ok().flatten())
                .and_then(|s| s.code());
            caller.data_mut().running_processes.remove(&pid_u32);

            // Update .out entry: state = killed
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            // Find the matching .out path and push a killed snapshot
            // (We don't have the original vfs_path here; emit a generic update key)
            // Best effort: push a kill record keyed by pid
            let kill_json = format!(
                "{{\"pid\":{pid_u32},\"status\":\"killed\",\"exit_code\":{}}}",
                exit_code.map(|c| c.to_string()).unwrap_or_else(|| "null".into())
            );
            let kill_key = format!("tmp/killed_{pid_u32}_{now_ms}.out.json");
            caller
                .data_mut()
                .pending_writes
                .push((kill_key, kill_json.into_bytes()));
            0
        },
    )?;

    // ── tool_dispatch: direct host-side execution of named agent tools ─────────
    linker.func_wrap(
        "host",
        "tool_dispatch",
        |mut caller: Caller<'_, HostState>,
         name_ptr: i32,
         name_len: i32,
         args_ptr: i32,
         args_len: i32,
         out_ptr: i32,
         out_cap: i32|
         -> i32 {
            let name = match read_memory(&mut caller, name_ptr, name_len) {
                Ok(s) => s,
                Err(_) => {
                    return write_memory(&mut caller, out_ptr, out_cap, r#"{"error":"bad name ptr"}"#);
                }
            };
            let args_str = read_memory(&mut caller, args_ptr, args_len)
                .unwrap_or_else(|_| "{}".to_string());
            let args: serde_json::Value =
                serde_json::from_str(&args_str).unwrap_or(serde_json::Value::Object(Default::default()));

            let result: Result<String, String> = match name.as_str() {
                "fs__file__view" => {
                    let file_path = args["filePath"]
                        .as_str()
                        .unwrap_or("")
                        .trim_start_matches('/')
                        .trim_start_matches("./")
                        .to_string();
                    if file_path.is_empty() {
                        Err("missing filePath".to_string())
                    } else {
                        let pending_hit = caller
                            .data()
                            .pending_writes
                            .iter()
                            .rev()
                            .find(|(p, _)| p == &file_path)
                            .map(|(_, d)| d.clone());
                        let bytes = if let Some(data) = pending_hit {
                            Ok(data)
                        } else {
                            let tcow_path = caller.data().tcow_path.clone();
                            if !Path::new(&tcow_path).exists() {
                                Err(format!("file not found: {file_path}"))
                            } else {
                                match TcowFile::open(&tcow_path) {
                                    Err(e) => Err(format!("tcow open error: {e}")),
                                    Ok(tf) => match tf.resolve(&file_path) {
                                        None => Err(format!("file not found: {file_path}")),
                                        Some((entry, _)) => Ok(entry.data),
                                    },
                                }
                            }
                        };
                        bytes.and_then(|b| {
                            String::from_utf8(b).map_err(|e| format!("utf8 error: {e}"))
                        })
                    }
                }
                "fs__file__create" => {
                    let file_path = args["filePath"]
                        .as_str()
                        .unwrap_or("")
                        .trim_start_matches('/')
                        .trim_start_matches("./")
                        .to_string();
                    let content = args["content"].as_str().unwrap_or("").to_string();
                    if file_path.is_empty() {
                        Err("missing filePath".to_string())
                    } else {
                        caller
                            .data_mut()
                            .pending_writes
                            .push((file_path.clone(), content.into_bytes()));
                        Ok(format!("created {file_path}"))
                    }
                }
                "fs__file__edit" => {
                    let file_path = args["filePath"]
                        .as_str()
                        .unwrap_or("")
                        .trim_start_matches('/')
                        .trim_start_matches("./")
                        .to_string();
                    let old_string = args["oldString"].as_str().unwrap_or("").to_string();
                    let new_string = args["newString"].as_str().unwrap_or("").to_string();
                    if file_path.is_empty() {
                        Err("missing filePath".to_string())
                    } else {
                        let pending_hit = caller
                            .data()
                            .pending_writes
                            .iter()
                            .rev()
                            .find(|(p, _)| p == &file_path)
                            .map(|(_, d)| d.clone());
                        let read_result = if let Some(data) = pending_hit {
                            String::from_utf8(data)
                                .map_err(|e| format!("utf8 error: {e}"))
                        } else {
                            let tcow_path = caller.data().tcow_path.clone();
                            if !Path::new(&tcow_path).exists() {
                                Err(format!("file not found: {file_path}"))
                            } else {
                                match TcowFile::open(&tcow_path) {
                                    Err(e) => Err(format!("tcow open error: {e}")),
                                    Ok(tf) => match tf.resolve(&file_path) {
                                        None => Err(format!("file not found: {file_path}")),
                                        Some((entry, _)) => String::from_utf8(entry.data)
                                            .map_err(|e| format!("utf8 error: {e}")),
                                    },
                                }
                            }
                        };
                        read_result.and_then(|text| {
                            if !text.contains(old_string.as_str()) {
                                Err(format!("oldString not found in {file_path}"))
                            } else {
                                let new_text = text.replacen(old_string.as_str(), &new_string, 1);
                                caller.data_mut().pending_writes.push((
                                    file_path.clone(),
                                    new_text.into_bytes(),
                                ));
                                Ok(format!("edited {file_path}"))
                            }
                        })
                    }
                }
                "fs__directory__list" => {
                    let raw_dir = args["path"]
                        .as_str()
                        .unwrap_or("")
                        .trim_start_matches('/')
                        .trim_end_matches('/')
                        .to_string();
                    let dir = if raw_dir == "." { String::new() } else { raw_dir };
                    let prefix = if dir.is_empty() {
                        String::new()
                    } else {
                        format!("{dir}/")
                    };
                    let tcow_path = caller.data().tcow_path.clone();
                    let pending = caller.data().pending_writes.clone();
                    let mut visible: std::collections::HashMap<String, ()> =
                        std::collections::HashMap::new();
                    if Path::new(&tcow_path).exists() {
                        if let Ok(tf) = TcowFile::open(&tcow_path) {
                            for (path, _) in tf.union_view() {
                                if path.starts_with(&prefix) {
                                    let rest = &path[prefix.len()..];
                                    if !rest.is_empty() {
                                        let top = rest.split('/').next().unwrap_or(rest);
                                        visible.insert(top.to_string(), ());
                                    }
                                }
                            }
                        }
                    }
                    for (path, _) in &pending {
                        if path.starts_with(&prefix) {
                            let rest = &path[prefix.len()..];
                            if !rest.is_empty() {
                                let top = rest.split('/').next().unwrap_or(rest);
                                visible.insert(top.to_string(), ());
                            }
                        }
                    }
                    let mut names: Vec<_> = visible.into_keys().collect();
                    names.sort();
                    Ok(names.join("\n"))
                }
                _ => Err(format!("unknown tool: {name}")),
            };

            let resp = match result {
                Ok(r) => {
                    let escaped = serde_json::to_string(&r).unwrap_or_else(|_| "\"\"".to_string());
                    format!(r#"{{"result":{escaped}}}"#)
                }
                Err(e) => {
                    let escaped = serde_json::to_string(&e).unwrap_or_else(|_| "\"\"".to_string());
                    format!(r#"{{"error":{escaped}}}"#)
                }
            };
            write_memory(&mut caller, out_ptr, out_cap, &resp)
        },
    )?;

    let tcow_path = env::var("TCOW_PATH").unwrap_or_else(|_| "agent.tcow".into());
    println!("[HOST] TCOW virtual FS: {tcow_path}");

    let wasi = WasiCtxBuilder::new().inherit_stdio().build();
    let state = HostState {
        prompt,
        final_answer: None,
        api_key,
        model,
        client,
        wasi,
        tcow_path,
        pending_writes: Vec::new(),
        shell_allow,
        shell_timeout,
        running_processes: HashMap::new(),
        system_prompt,
        max_steps,
        enabled_tools,
        context_window,
    };

    let mut store = Store::new(&engine, state);
    store
        .add_fuel(FUEL_LIMIT)
        .context("failed to set fuel limit")?;

    let instance = linker.instantiate(&mut store, &module)?;
    let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;
    run.call(&mut store, ())?;

    // Flush buffered writes to the .tcow file
    {
        let state = store.data();
        if !state.pending_writes.is_empty() {
            let tcow_path = &state.tcow_path;
            let writes = &state.pending_writes;
            println!("[HOST] Flushing {} write(s) to {tcow_path}", writes.len());
            if Path::new(tcow_path).exists() {
                TcowFile::append_delta(tcow_path, writes, &[])
                    .context("failed to append delta layer to .tcow")?;
            } else {
                TcowFile::create(tcow_path, writes, &[], None)
                    .context("failed to create .tcow file")?;
            }
            println!("[HOST] TCOW flush complete.");
        } else {
            println!("[HOST] No TCOW writes this run.");
        }
    }

    let consumed = store.fuel_consumed().unwrap_or(0);
    let remaining = FUEL_LIMIT.saturating_sub(consumed);
    println!("[HOST] Fuel consumed: {consumed} / {FUEL_LIMIT} (remaining: {remaining})");

    if store.data().final_answer.is_none() {
        println!("[HOST] Agent completed without final answer export.");
    }

    Ok(())
}

fn all_tool_names() -> Vec<String> {
    ["js_exec", "fs__file__view", "fs__file__create", "fs__file__edit", "fs__directory__list"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[allow(dead_code)]
fn build_tools_json(enabled: &[String]) -> String {
    let all: &[(&str, &str, &str)] = &[
        (
            "js_exec",
            "Execute JavaScript in a sandboxed Boa ES2020 interpreter inside a Wasmtime Wasm guest. \
Available globals: console.log; fs.readFile(path)->string (throws on not-found); \
fs.writeFile(path, content) writes to virtual .tcow FS; fs.readdir(dir)->string newline-delimited; \
require(path) evaluates a .tcow file as JS. The real host filesystem is NOT accessible. \
No fetch, no Node built-ins.",
            r#"{"type":"object","properties":{"code":{"type":"string","description":"JS code to run."}},"required":["code"]}"#,
        ),
        (
            "fs__file__view",
            "Read and return the full contents of a file in the virtual .tcow filesystem.",
            r#"{"type":"object","properties":{"filePath":{"type":"string","description":"Path to the file to read."}},"required":["filePath"]}"#,
        ),
        (
            "fs__file__create",
            "Create or overwrite a file in the virtual .tcow filesystem with the given content.",
            r#"{"type":"object","properties":{"filePath":{"type":"string","description":"Path to create."},"content":{"type":"string","description":"File content."}},"required":["filePath","content"]}"#,
        ),
        (
            "fs__file__edit",
            "Replace the first occurrence of oldString with newString in a .tcow file.",
            r#"{"type":"object","properties":{"filePath":{"type":"string"},"oldString":{"type":"string","description":"Exact text to find."},"newString":{"type":"string","description":"Replacement text."}},"required":["filePath","oldString","newString"]}"#,
        ),
        (
            "fs__directory__list",
            "List the top-level entries under a directory in the virtual .tcow filesystem.",
            r#"{"type":"object","properties":{"path":{"type":"string","description":"Directory path to list."}},"required":["path"]}"#,
        ),
    ];

    let entries: Vec<String> = all
        .iter()
        .filter(|(name, _, _)| enabled.contains(&name.to_string()))
        .map(|(name, desc, params)| {
            format!(
                r#"{{"type":"function","function":{{"name":"{name}","description":"{desc}","parameters":{params}}}}}"#,
                name = name,
                desc = desc.replace('"', "\\\""),
                params = params
            )
        })
        .collect();
    format!("[{}]", entries.join(","))
}

fn format_tool_result(result_json: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(result_json).unwrap_or_default();
    let stdout = parsed["stdout"].as_str().unwrap_or("").trim();
    let ret    = parsed["result"].as_str().unwrap_or("").trim();
    let err    = parsed["error"].as_str();
    if let Some(e) = err {
        format!("ERROR: {e}")
    } else if !stdout.is_empty() && (ret.is_empty() || ret == "undefined") {
        format!("stdout:\n{stdout}")
    } else if !stdout.is_empty() {
        format!("stdout:\n{stdout}\nreturn value: {ret}")
    } else {
        format!("return value: {ret}")
    }
}

fn llm_decide(state: &HostState, req: &GuestRequest) -> Result<LlmDecision> {
    // Build native xAI tool definitions
    let tools_json_str = build_tools_json(&state.enabled_tools);
    let tools_value: serde_json::Value = serde_json::from_str(&tools_json_str)
        .unwrap_or(serde_json::json!([]));

    let system = state.system_prompt.as_deref().unwrap_or("");
    let initial_user = &req.prompt;

    // Build messages: system → user(prompt) → [assistant{tool_calls} → tool{result}]...
    let mut messages: Vec<serde_json::Value> = vec![
        serde_json::json!({"role": "system", "content": system}),
        serde_json::json!({"role": "user", "content": initial_user}),
    ];
    for entry in &req.history {
        // Re-insert assistant message exactly as the API returned it (contains tool_calls)
        let assistant: serde_json::Value = serde_json::from_str(&entry.assistant_msg_json)
            .unwrap_or_else(|_| serde_json::json!({"role": "assistant", "content": ""}));
        messages.push(assistant);
        // Tool result as role:tool
        let summary = format_tool_result(&entry.result_json);
        messages.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": entry.tool_call_id,
            "name": entry.tool_name,
            "content": summary,
        }));
    }

    let body = serde_json::json!({
        "model": state.model,
        "temperature": 0.1,
        "messages": messages,
        "tools": tools_value,
        "tool_choice": "auto",
    });

    let resp = state
        .client
        .post("https://api.x.ai/v1/chat/completions")
        .bearer_auth(&state.api_key)
        .json(&body)
        .send()
        .context("request to xAI failed")?;

    let status = resp.status();
    let payload: serde_json::Value = resp.json().context("failed to parse xAI response JSON")?;
    if !status.is_success() {
        return Err(anyhow!("xAI API error {status}: {payload}"));
    }

    // Print context window usage for this step
    {
        let u = &payload["usage"];
        let prompt     = u["prompt_tokens"].as_u64().unwrap_or(0);
        let completion = u["completion_tokens"].as_u64().unwrap_or(0);
        let total      = u["total_tokens"].as_u64().unwrap_or(prompt + completion);
        if let Some(window) = state.context_window {
            let pct = total * 100 / window.max(1);
            println!("[CTX] step={} {total}/{window} tokens ({pct}%)  [prompt={prompt} completion={completion}]", req.step);
        } else {
            println!("[CTX] step={} prompt={prompt} completion={completion} total={total}", req.step);
        }
    }

    let message = &payload["choices"][0]["message"];

    // Native tool calling: if model returned tool_calls, dispatch the first one
    if let Some(tool_calls) = message["tool_calls"].as_array() {
        if let Some(tc) = tool_calls.first() {
            let tool_call_id = tc["id"].as_str().unwrap_or("").to_string();
            let tool_name = tc["function"]["name"].as_str().unwrap_or("").to_string();
            let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
            let args: serde_json::Value = serde_json::from_str(args_str)
                .unwrap_or_else(|_| serde_json::json!({}));

            // Serialize the full assistant message for replay in the next turn
            let assistant_msg_json = serde_json::to_string(message)
                .unwrap_or_else(|_| "{}".to_string());

            let code = if tool_name == "js_exec" {
                args["code"].as_str().map(str::to_string)
            } else {
                None
            };
            let args_field = if tool_name != "js_exec" {
                Some(args)
            } else {
                None
            };

            return Ok(LlmDecision::ToolCall {
                tool: tool_name,
                tool_call_id,
                assistant_msg_json,
                code,
                args: args_field,
            });
        }
    }

    // No tool_calls → model is done; return its text as the final answer
    let content = message["content"].as_str().unwrap_or("").trim().to_string();
    Ok(LlmDecision::Final {
        answer: content,
        thought: None,
    })
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

fn read_memory_bytes(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Vec<u8> {
    if ptr < 0 || len < 0 {
        return Vec::new();
    }
    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return Vec::new(),
    };
    let mut bytes = vec![0u8; len as usize];
    let _ = memory.read(caller, ptr as usize, &mut bytes);
    bytes
}

/// Write raw bytes into guest memory; returns byte count or negative error.
fn write_memory_bytes(
    caller: &mut Caller<'_, HostState>,
    out_ptr: i32,
    data: &[u8],
) -> i32 {
    if out_ptr < 0 {
        return -1;
    }
    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return -2,
    };
    if memory
        .write(&mut *caller, out_ptr as usize, data)
        .is_err()
    {
        return -3;
    }
    data.len() as i32
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
