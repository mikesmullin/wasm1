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
    shell: Option<ShellConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct ShellConfig {
    allow: Option<Vec<String>>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
struct TemplateSpec {
    system_prompt: Option<String>,
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

fn load_template(path: &Path) -> Result<(Vec<Regex>, Option<u64>, Option<String>)> {
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
    let system_prompt = template.spec.and_then(|s| s.system_prompt);
    println!(
        "[HOST] Template loaded: {} shell allow-list entries, timeout: {}, system_prompt: {}",
        regexes.len(),
        timeout.map(|t| format!("{t}s")).unwrap_or_else(|| "indefinite".into()),
        system_prompt.as_deref().map(|s| format!("{} chars", s.len())).unwrap_or_else(|| "none".into())
    );
    Ok((regexes, timeout, system_prompt))
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
    let (shell_allow, shell_timeout, system_prompt) = if let Some(ref name) = template_name {
        let path = resolve_template(name)?;
        println!("[HOST] Using template: {}", path.display());
        load_template(&path)?
    } else {
        (Vec::new(), SHELL_TIMEOUT_DEFAULT, None)
    };

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
                Ok(p) => p.trim_start_matches('/').to_string(),
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
                Ok(p) => p.trim_start_matches('/').to_string(),
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
                Ok(p) => p.trim_start_matches('/').trim_end_matches('/').to_string(),
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

    let tcow_path = env::var("TCOW_PATH").unwrap_or_else(|_| "agent.tcow".into());
    println!("[HOST] TCOW virtual FS: {tcow_path}");

    let wasi = WasiCtxBuilder::new().inherit_stdio().build();
    let state = HostState {
        prompt,
        final_answer: None,
        api_key,
        model,
        client: Client::new(),
        wasi,
        tcow_path,
        pending_writes: Vec::new(),
        shell_allow,
        shell_timeout,
        running_processes: HashMap::new(),
        system_prompt,
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

fn llm_decide(state: &HostState, req: &GuestRequest) -> Result<LlmDecision> {
    let tools_json = r#"[
  {
    "type": "function",
    "function": {
      "name": "js_exec",
      "description": "Execute JavaScript in a sandboxed Boa ES2020 interpreter inside a Wasmtime Wasm guest. Available globals: console.log; fs.readFile(path)->string (throws on not-found); fs.writeFile(path, content) writes to virtual .tcow FS; fs.readdir(dir)->string newline-delimited; require(path) evaluates a .tcow file as JS. The real host filesystem is NOT accessible. No fetch, no Node built-ins.",
      "parameters": {
        "type": "object",
        "properties": {
          "code": { "type": "string", "description": "JS code to run. Use fs.readFile / fs.writeFile for virtual FS access." }
        },
        "required": ["code"]
      }
    }
  }
]"#;

    let base_system = "You are an agent planner. Return ONLY valid JSON with schema:\n\
{\"type\":\"tool_call\",\"tool\":\"js_exec\",\"code\":\"...\",\"thought\":\"...\"}\n\
or\n\
{\"type\":\"final\",\"answer\":\"...\",\"thought\":\"...\"}.\n\
IMPORTANT: When you receive a js_exec result, the 'stdout' field IS the output of the code you ran. \
Whatever value is printed by console.log or returned is the CORRECT answer from the sandbox. \
Accept it and return a final answer. Do NOT re-run the same code hoping for a different result.\n\
JS runs in Boa inside Wasm. Available globals: console.log, fs.readFile(path), fs.writeFile(path, content), fs.readdir(dir), require(path). The real host filesystem is NOT accessible from JS. No markdown, no code fences.";

    let system_owned: String;
    let system: &str = if let Some(ref sp) = state.system_prompt {
        system_owned = format!("{}\n\n---\n{}", sp, base_system);
        &system_owned
    } else {
        base_system
    };

    let user = match &req.tool_result {
        Some(result) => {
            // Parse tool result to present stdout/result plainly so the LLM
            // doesn't have to interpret raw JSON.
            let parsed: serde_json::Value = serde_json::from_str(result).unwrap_or_default();
            let stdout = parsed["stdout"].as_str().unwrap_or("").trim();
            let ret    = parsed["result"].as_str().unwrap_or("").trim();
            let err    = parsed["error"].as_str();
            let summary = if let Some(e) = err {
                format!("ERROR: {e}")
            } else if !stdout.is_empty() && ret.is_empty() || ret == "undefined" {
                format!("stdout:\n{stdout}")
            } else if !stdout.is_empty() {
                format!("stdout:\n{stdout}\nreturn value: {ret}")
            } else {
                format!("return value: {ret}")
            };
            format!(
                "Original prompt:\n{}\n\nTool available:\n{}\n\njs_exec result (step {}):\n{}\n\nIf this provides enough information, return a final answer.",
                req.prompt, tools_json, req.step, summary
            )
        }
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
