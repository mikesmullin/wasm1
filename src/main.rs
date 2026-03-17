use anyhow::{anyhow, Context, Result};
use dotenvy::from_filename;
use minijinja::{Environment as MiniJinjaEnv, context as mj_context};
use regex::Regex;
use reqwest::blocking::{Client, ClientBuilder};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tcow::TcowFile;
use wasmtime::{Caller, Config, Engine, Linker, Module, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, add_to_linker as wasi_add_to_linker};

const DEFAULT_MODEL: &str = "grok-4-1-fast-reasoning";
const GUEST_WASM_PATH: &str = "guest/target/wasm32-wasip1/debug/guest.wasm";
const FUEL_LIMIT: u64 = 2_000_000_000;
const DEFAULT_CRON_INTERVAL_MS: u64 = 60_000;
/// `None` = wait indefinitely (default when no template or template omits timeout_secs).
const SHELL_TIMEOUT_DEFAULT: Option<u64> = None;
static MSG_ID_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelProvider {
    Xai,
    Copilot,
}

// ── Template YAML structs ─────────────────────────────────────────────────────
#[derive(Debug, Deserialize, Default)]
struct Template {
    #[serde(rename = "apiVersion")]
    api_version: Option<String>,
    kind: Option<String>,
    metadata: Option<TemplateMetadata>,
    spec: Option<TemplateSpec>,
}

#[derive(Debug, Deserialize, Default)]
struct TemplateMetadata {
    description: Option<String>,
    model: Option<String>,
    /// Fallback context window size in tokens, used if the model API lookup fails.
    context_window: Option<u64>,
    labels: Option<Vec<String>>,
    hooks: Option<Vec<HookDef>>,
    max_steps: Option<u32>,
    validate: Option<String>,
    max_validation_fails: Option<u32>,
    shell: Option<ShellConfig>,
    auto_approve: Option<Vec<AutoApproveRuleDef>>,
    /// Explicit tool allowlist for the session. Absent = all tools.
    tools: Option<Vec<String>>,
    /// If true, disable SSL certificate validation for HTTP requests.
    ignore_ssl: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct ShellConfig {
    allow: Option<Vec<String>>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct AutoApproveRuleDef {
    tool: String,
    #[serde(default)]
    pattern: Option<String>,
}

#[derive(Debug, Clone)]
struct AutoApproveRule {
    tool: String,
    pattern: Option<Regex>,
}

#[derive(Debug, Deserialize, Default)]
struct TemplateSpec {
    system_prompt: Option<String>,
    max_steps: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct HookDef {
    name: String,
    on: String,
    enabled: Option<bool>,
    #[serde(default)]
    when: HashMap<String, serde_yaml::Value>,
    #[serde(default)]
    jobs: HashMap<String, HookJob>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct HookJob {
    #[serde(default)]
    needs: Vec<String>,
    #[serde(default)]
    steps: Vec<HookStep>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct HookStep {
    id: Option<String>,
    #[serde(rename = "type")]
    step_type: String,
    command: Option<String>,
    template: Option<String>,
    prompt: Option<String>,
    stdin: Option<String>,
    #[serde(default)]
    data: serde_yaml::Value,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
struct HookFile {
    #[serde(default)]
    hooks: Vec<HookDef>,
}

#[derive(Debug, Clone, Default)]
struct HookRunResult {
    blocked_reason: Option<String>,
    last_llm_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MsgEnvelope {
    id: String,
    #[serde(rename = "type")]
    msg_type: String,
    sender: String,
    recipient: String,
    priority: String,
    status: String,
    assignee: Option<String>,
    #[serde(default)]
    #[serde(rename = "blockedBy")]
    blocked_by: Vec<String>,
    #[serde(default)]
    payload: serde_json::Value,
    #[serde(default)]
    history: Vec<serde_json::Value>,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TeamMember {
    index: usize,
    session_id: String,
    pid: Option<u32>,
    template: Option<String>,
    output: Option<String>,
    status: String,
    launched_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TeamFile {
    team_id: String,
    status: String,
    created_at: String,
    members: Vec<TeamMember>,
}

#[derive(Debug, Serialize)]
struct SessionSnapshot {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    metadata: SessionMetadata,
    spec: SessionSpec,
}

#[derive(Debug, Serialize)]
struct SessionMetadata {
    id: String,
    name: String,
    model: String,
    status: String,
    created: String,
    last_pid: u32,
    tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_steps: Option<u32>,
    labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_transition: Option<SessionTransition>,
}

#[derive(Debug, Deserialize)]
struct SessionSnapshotIn {
    metadata: SessionMetadataIn,
    spec: SessionSpecIn,
}

#[derive(Debug, Deserialize)]
struct SessionMetadataIn {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionSpecIn {
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    messages: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct SessionTransition {
    action: String,
    from: String,
    to: String,
    timestamp: String,
}

#[derive(Debug, Serialize)]
struct SessionSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_prompt: Option<String>,
    messages: Vec<SessionMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMessage {
    role: String,
    verbatim: serde_json::Value,
    meta: serde_json::Value,
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
    provider: ModelProvider,
    /// Provider-native model name (without optional provider prefix).
    model_name: String,
    /// Base URL used by provider APIs.
    provider_api_url: String,
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
    /// Optional JS validation function from template metadata.validate.
    validate_fn: Option<String>,
    /// Maximum validation retries before failing the run.
    max_validation_fails: Option<u32>,
    /// Tool names available to the LLM (controls tools_json).
    enabled_tools: Vec<String>,
    /// Template-based auto-approval rules for tool invocations.
    auto_approve_rules: Vec<AutoApproveRule>,
    /// Known context window size for the model (tokens); None = unknown.
    context_window: Option<u64>,
    /// Canonical session id `<timestampMs>-<pid>-<hex4>`.
    session_id: String,
    /// Workspace root path used by host-side tools.
    workspace_root: PathBuf,
    /// Directory the user invoked the binary from (cwd before any cd).
    invocation_cwd: PathBuf,
    /// Effective merged hooks (template > user > repo).
    hooks: Vec<HookDef>,
    /// Session creation timestamp used in snapshots.
    session_created: String,
    /// Current session state for transition tracking.
    session_state: String,
    /// Template display name used in session snapshots.
    template_name: String,
    /// Optional template description for session snapshots.
    template_description: Option<String>,
    /// Template labels for session snapshots.
    template_labels: Vec<String>,
    /// Seeded history restored from session YAML on resume.
    seed_history: Vec<HistoryEntry>,
    /// Seeded validation feedback restored from session YAML on resume.
    seed_validation_feedback: Vec<String>,
    /// Seeded logical step restored from session YAML on resume.
    seed_step: u32,
    /// Seeded ordered messages restored from session YAML on resume.
    seed_messages: Vec<SessionMessage>,
    /// Tool calls approved in session YAML but not yet executed (no tool result entry yet).
    seed_pending_calls: Vec<PendingToolCall>,
    /// Resume-mode gate: true when waiting for manual approvals and no executable work exists.
    seed_blocked_on_approval: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct HistoryEntry {
    tool_call_id: String,
    tool_name: String,
    assistant_msg_json: String,
    result_json: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct GuestRequest {
    prompt: String,
    history: Vec<HistoryEntry>,
    #[serde(default)]
    validation_feedback: Vec<String>,
    step: u32,
    #[serde(default)]
    unsent_tool_result_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PendingToolCall {
    tool: String,
    tool_call_id: String,
    #[serde(default)]
    args: serde_json::Value,
    assistant_msg_json: String,
}

#[derive(Debug, Serialize)]
struct SessionSeedPayload {
    prompt: String,
    history: Vec<HistoryEntry>,
    validation_feedback: Vec<String>,
    step: u32,
    pending_tool_calls: Vec<PendingToolCall>,
    blocked_on_approval: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ToolInvocation {
    tool: String,
    tool_call_id: String,
    #[serde(default)]
    args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum LlmDecision {
    #[serde(rename = "tool_call")]
    ToolCall {
        tool_calls: Vec<ToolInvocation>,
        /// Serialized assistant message (with tool_calls) to replay in history.
        assistant_msg_json: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        perf: Option<serde_json::Value>,
    },
    #[serde(rename = "final")]
    Final {
        answer: String,
        thought: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        perf: Option<serde_json::Value>,
    },
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        perf: Option<serde_json::Value>,
    },
}

/// Display `path` relative to `base` if possible, otherwise absolute.
fn rel_path(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn resolve_template(name: &str, extra_roots: &[&Path]) -> Result<PathBuf> {
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
    // Search each root in order: extra roots (e.g. invocation cwd) first, then
    // workspace root (current directory after cd).
    let mut roots: Vec<PathBuf> = extra_roots.iter().map(|r| r.to_path_buf()).collect();
    roots.push(PathBuf::from("."));
    for root in &roots {
        let candidate = root.join(".agent/templates").join(&basename);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "template '{name}' not found in .agent/templates/ (searched {} root(s))",
        roots.len()
    ))
}

fn load_template(path: &Path) -> Result<(Vec<Regex>, Option<u64>, Option<String>, Option<u32>, Option<String>, Option<u32>, Vec<String>, Vec<AutoApproveRule>, Option<u64>, Vec<HookDef>, Option<String>, Vec<String>, Option<String>, bool)> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read template: {}", path.display()))?;
    let template: Template = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse template YAML: {}", path.display()))?;
    if let Some(api_version) = template.api_version.as_deref() {
        if api_version != "daemon/v1" {
            return Err(anyhow!("unsupported apiVersion '{api_version}', expected daemon/v1"));
        }
    }
    if let Some(kind) = template.kind.as_deref() {
        if kind != "Agent" {
            return Err(anyhow!("unsupported kind '{kind}', expected Agent"));
        }
    }
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
    let auto_defs = template
        .metadata
        .as_ref()
        .and_then(|m| m.auto_approve.as_ref())
        .cloned()
        .unwrap_or_default();
    let auto_approve_rules = auto_defs
        .iter()
        .map(|rule| {
            let compiled = rule
                .pattern
                .as_deref()
                .map(|pat| Regex::new(pat).with_context(|| format!("invalid auto_approve regex in template: {pat}")))
                .transpose()?;
            Ok(AutoApproveRule {
                tool: rule.tool.clone(),
                pattern: compiled,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut system_prompt = template.spec.as_ref().and_then(|s| s.system_prompt.clone());
    let max_steps = template
        .metadata
        .as_ref()
        .and_then(|m| m.max_steps)
        .or_else(|| template.spec.as_ref().and_then(|s| s.max_steps));
    let validate_fn = template
        .metadata
        .as_ref()
        .and_then(|m| m.validate.clone())
        .filter(|s| !s.trim().is_empty());
    let max_validation_fails = template
        .metadata
        .as_ref()
        .and_then(|m| m.max_validation_fails);
    if let Some(validate_code) = validate_fn.as_deref() {
        let validation_prompt = format!(
            "\n\nYour final/stop (non-intermediate) reply must cause the following function to return truthy:\n```js\n{validate_code}```"
        );
        let mut base = system_prompt.unwrap_or_default();
        base.push_str(&validation_prompt);
        system_prompt = Some(base);
    }
    // tools: absent → all built-in tools; explicit list → only those
    let template_context_window = template.metadata.as_ref().and_then(|m| m.context_window);
    let hooks = template
        .metadata
        .as_ref()
        .and_then(|m| m.hooks.clone())
        .unwrap_or_default();
    let labels = template
        .metadata
        .as_ref()
        .and_then(|m| m.labels.clone())
        .unwrap_or_default();
    let description = template
        .metadata
        .as_ref()
        .and_then(|m| m.description.clone());
    let template_model = template
        .metadata
        .as_ref()
        .and_then(|m| m.model.clone())
        .filter(|m| !m.trim().is_empty());
    let tools = template
        .metadata
        .as_ref()
        .and_then(|m| m.tools.clone())
        .unwrap_or_else(all_tool_names);
    let ignore_ssl = template
        .metadata
        .as_ref()
        .and_then(|m| m.ignore_ssl)
        .unwrap_or(false);
    println!(
        "[HOST] Template loaded: {} shell allow-list entries, timeout: {}, model: {}, system_prompt: {}, max_steps: {}, validate: {}, max_validation_fails: {}, tools: [{}], hooks: {}, labels: {}, context_window: {}, ignore_ssl: {}",
        regexes.len(),
        timeout.map(|t| format!("{t}s")).unwrap_or_else(|| "indefinite".into()),
        template_model.clone().unwrap_or_else(|| "unset".into()),
        system_prompt.as_deref().map(|s| format!("{} chars", s.len())).unwrap_or_else(|| "none".into()),
        max_steps.map(|n| n.to_string()).unwrap_or_else(|| "indefinite".into()),
        if validate_fn.is_some() { "yes" } else { "no" },
        max_validation_fails
            .map(|n| n.to_string())
            .unwrap_or_else(|| "indefinite".into()),
        tools.join(", "),
        hooks.len(),
        labels.len(),
        template_context_window.map(|n| format!("{n}")).unwrap_or_else(|| "unset".into()),
        ignore_ssl,
    );
    Ok((regexes, timeout, system_prompt, max_steps, validate_fn, max_validation_fails, tools, auto_approve_rules, template_context_window, hooks, description, labels, template_model, ignore_ssl))
}

fn parse_provider_model(raw_model: &str) -> (ModelProvider, String) {
    if let Some(name) = raw_model.strip_prefix("copilot:") {
        return (ModelProvider::Copilot, name.to_string());
    }
    if let Some(name) = raw_model.strip_prefix("xai:") {
        return (ModelProvider::Xai, name.to_string());
    }
    (ModelProvider::Xai, raw_model.to_string())
}

// ── Copilot Internal API Auth ─────────────────────────────────────────────────

/// Copilot auth config (mimics VS Code behavior)
const COPILOT_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const COPILOT_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98"; // VS Code client ID
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const COPILOT_API_URL: &str = "https://api.githubcopilot.com";
const COPILOT_USER_AGENT: &str = "GitHubCopilot/1.155.0";
const COPILOT_EDITOR_VERSION: &str = "vscode/1.85.1";
const COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot/1.155.0";

#[derive(Debug, Serialize, Deserialize, Default)]
struct CopilotTokens {
    github_token: Option<String>,
    copilot_token: Option<String>,
    expires_at: Option<u64>,
    api_url: Option<String>,
}

fn copilot_tokens_path() -> PathBuf {
    PathBuf::from(".tokens.yaml")
}

fn load_copilot_tokens() -> CopilotTokens {
    let path = copilot_tokens_path();
    let abs = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if path.exists() {
        if let Ok(content) = fs::read_to_string(&path) {
            match serde_yaml::from_str(&content) {
                Ok(tokens) => {
                    println!("[HOST] Loaded tokens from {}", abs.display());
                    return tokens;
                }
                Err(e) => {
                    eprintln!("[HOST] Failed to parse {}: {e}", abs.display());
                }
            }
        }
    } else {
        println!("[HOST] No tokens file at {}", abs.display());
    }
    CopilotTokens::default()
}

fn save_copilot_tokens(tokens: &CopilotTokens) -> Result<()> {
    let path = copilot_tokens_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_yaml::to_string(tokens)?;
    fs::write(&path, content)?;
    Ok(())
}

/// Start GitHub device OAuth flow, returns (device_code, user_code, verification_uri, interval)
fn copilot_start_device_flow(client: &Client) -> Result<(String, String, String, u64)> {
    #[derive(Deserialize)]
    struct DeviceFlowResponse {
        device_code: String,
        user_code: String,
        verification_uri: String,
        interval: u64,
    }

    let resp = client
        .post(COPILOT_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("User-Agent", COPILOT_USER_AGENT)
        .json(&serde_json::json!({
            "client_id": COPILOT_CLIENT_ID,
            "scope": "read:user"
        }))
        .send()
        .context("failed to start device flow")?;

    if !resp.status().is_success() {
        return Err(anyhow!("device flow failed: {}", resp.status()));
    }

    let data: DeviceFlowResponse = resp.json().context("failed to parse device flow response")?;
    Ok((data.device_code, data.user_code, data.verification_uri, data.interval))
}

/// Poll for GitHub access token after user authenticates
fn copilot_poll_for_access_token(client: &Client, device_code: &str, interval: u64) -> Result<String> {
    let max_attempts = 180; // ~15 minutes with 5s interval
    for attempt in 1..=max_attempts {
        std::thread::sleep(Duration::from_secs(interval));

        let resp = client
            .post(COPILOT_ACCESS_TOKEN_URL)
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .header("User-Agent", COPILOT_USER_AGENT)
            .json(&serde_json::json!({
                "client_id": COPILOT_CLIENT_ID,
                "device_code": device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
            }))
            .send()
            .context("failed to poll for access token")?;

        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: Option<String>,
            error: Option<String>,
            error_description: Option<String>,
        }

        let data: TokenResponse = resp.json().context("failed to parse token response")?;

        if let Some(token) = data.access_token {
            return Ok(token);
        }
        if let Some(err) = data.error {
            if err == "authorization_pending" {
                if attempt % 6 == 0 {
                    eprintln!("[HOST] Still waiting for GitHub authorization... ({attempt}/{max_attempts})");
                }
                continue;
            }
            return Err(anyhow!("auth failed: {}", data.error_description.unwrap_or(err)));
        }
    }
    Err(anyhow!("authentication timed out after 15 minutes"))
}

/// Exchange GitHub token for Copilot internal token
fn copilot_get_token(client: &Client, github_token: &str) -> Result<(String, u64, String)> {
    let resp = client
        .get(COPILOT_TOKEN_URL)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {github_token}"))
        .header("User-Agent", COPILOT_USER_AGENT)
        .header("Editor-Version", COPILOT_EDITOR_VERSION)
        .header("Editor-Plugin-Version", COPILOT_EDITOR_PLUGIN_VERSION)
        .send()
        .context("failed to get Copilot token")?;

    if !resp.status().is_success() {
        return Err(anyhow!("failed to get Copilot token: {}", resp.status()));
    }

    #[derive(Deserialize)]
    struct CopilotTokenResponse {
        token: String,
        expires_at: u64,
        endpoints: Option<CopilotEndpoints>,
    }
    #[derive(Deserialize)]
    struct CopilotEndpoints {
        api: Option<String>,
    }

    let data: CopilotTokenResponse = resp.json().context("failed to parse Copilot token response")?;
    let api_url = data.endpoints.and_then(|e| e.api).unwrap_or_else(|| COPILOT_API_URL.to_string());
    Ok((data.token, data.expires_at, api_url))
}

/// Resolve Copilot authentication using internal API (like VS Code)
fn resolve_copilot_internal_auth(client: &Client) -> Result<(String, String)> {
    let mut tokens = load_copilot_tokens();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

    // 1. Valid Copilot token exists
    if let (Some(ref copilot_token), Some(expires_at)) = (&tokens.copilot_token, tokens.expires_at) {
        if expires_at > now + 60 {
            let api_url = tokens.api_url.clone().unwrap_or_else(|| COPILOT_API_URL.to_string());
            println!("[HOST] Using cached Copilot token (expires in {}s)", expires_at - now);
            return Ok((copilot_token.clone(), api_url));
        }
    }

    // 2. Refresh with GitHub token
    if let Some(ref github_token) = tokens.github_token {
        println!("[HOST] Refreshing Copilot token...");
        match copilot_get_token(client, github_token) {
            Ok((copilot_token, expires_at, api_url)) => {
                tokens.copilot_token = Some(copilot_token.clone());
                tokens.expires_at = Some(expires_at);
                tokens.api_url = Some(api_url.clone());
                let _ = save_copilot_tokens(&tokens);
                println!("[HOST] ✅ Copilot token refreshed");
                return Ok((copilot_token, api_url));
            }
            Err(e) => {
                eprintln!("[HOST] ⚠ Failed to refresh Copilot token: {e}");
                // Clear invalid GitHub token
                tokens.github_token = None;
                let _ = save_copilot_tokens(&tokens);
            }
        }
    }

    // 3. Fresh authentication via device flow
    println!("[HOST] Starting Copilot authentication via GitHub device flow...");
    let (device_code, user_code, verification_uri, interval) = copilot_start_device_flow(client)?;

    eprintln!("\n╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║             GITHUB COPILOT AUTHENTICATION                    ║");
    eprintln!("╠══════════════════════════════════════════════════════════════╣");
    eprintln!("║                                                              ║");
    eprintln!("║  📋 Visit: {:<47} ║", verification_uri);
    eprintln!("║  🔑 Enter code: {:<42} ║", user_code);
    eprintln!("║                                                              ║");
    eprintln!("╚══════════════════════════════════════════════════════════════╝\n");

    let github_token = copilot_poll_for_access_token(client, &device_code, interval)?;
    println!("[HOST] ✅ GitHub authenticated!");

    let (copilot_token, expires_at, api_url) = copilot_get_token(client, &github_token)?;
    println!("[HOST] ✅ Copilot token obtained!");

    tokens.github_token = Some(github_token);
    tokens.copilot_token = Some(copilot_token.clone());
    tokens.expires_at = Some(expires_at);
    tokens.api_url = Some(api_url.clone());
    save_copilot_tokens(&tokens)?;

    Ok((copilot_token, api_url))
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
    // Capture where the user actually ran the command from, before any cd.
    let invocation_cwd = env::current_dir().context("failed to get current directory")?;

    // Parse CLI args:
    //   wasm1 cron [once]
    //   wasm1 [-t <template>] <prompt>
    let all_args: Vec<String> = env::args().skip(1).collect();
    if all_args.first().map(String::as_str) == Some("cron") {
        let mode = all_args.get(1).map(String::as_str).unwrap_or("once");
        let mut cron_template: Option<String> = None;
        let mut cron_verbose = false;
        let mut idx = 2usize;
        while idx < all_args.len() {
            match all_args[idx].as_str() {
                "-t" | "--template" => {
                    if idx + 1 >= all_args.len() {
                        return Err(anyhow!("usage: cargo run -- cron [once] [-t <template>] [-v]"));
                    }
                    cron_template = Some(all_args[idx + 1].clone());
                    idx += 2;
                }
                "-v" | "--verbose" => {
                    cron_verbose = true;
                    idx += 1;
                }
                other => {
                    return Err(anyhow!(
                        "unknown cron arg: {other}. usage: cargo run -- cron [once] [-t <template>] [-v]"
                    ));
                }
            }
        }
        return run_cron(mode, cron_template.as_deref(), cron_verbose);
    }
    if all_args.first().map(String::as_str) == Some("clean") {
        return run_clean();
    }

    let mut args_iter = all_args.into_iter();
    let mut template_name: Option<String> = None;
    let mut session_name: Option<String> = None;
    let mut read_stdin = false;
    let mut prompt: Option<String> = None;
    while let Some(arg) = args_iter.next() {
        if arg == "-t" || arg == "--template" {
            template_name = Some(
                args_iter
                    .next()
                    .ok_or_else(|| anyhow!("missing template name after {arg}"))?,
            );
        } else if arg == "-s" || arg == "--session" {
            session_name = Some(
                args_iter
                    .next()
                    .ok_or_else(|| anyhow!("missing session id after {arg}"))?,
            );
        } else if arg == "-i" || arg == "--stdin" {
            read_stdin = true;
        } else {
            prompt = Some(match prompt {
                Some(existing) => format!("{existing} {arg}"),
                None => arg,
            });
        }
    }
    if template_name.is_some() && session_name.is_some() {
        return Err(anyhow!("-t/--template and -s/--session are mutually exclusive"));
    }
    if session_name.is_none() && template_name.is_none() && prompt.is_none() {
        return Err(anyhow!(
            "usage: cargo run -- [clean|cron [once]|-t <template> [prompt]|-s <session_id> [prompt]]"
        ));
    }
    let is_resume_mode = session_name.is_some();
    let init_only_new_session = !is_resume_mode && template_name.is_some() && prompt.is_none();

    // Resolve the workspace root (walks up from binary's real path if needed)
    // and cd there so .agent/, .tokens.yaml, .env, guest.wasm etc. all resolve
    // correctly regardless of where the binary was invoked from.
    let workspace_root = resolve_workspace_root()?;
    if workspace_root != invocation_cwd {
        println!("[HOST] Workspace root: {}", rel_path(&invocation_cwd, &workspace_root));
    }
    env::set_current_dir(&workspace_root)
        .with_context(|| format!("failed to set working directory to {}", workspace_root.display()))?;
    // Load .env from workspace root (now the cwd).
    let _ = from_filename(".env");

    // For template lookup: search invocation cwd first, then workspace root.
    let extra_roots: Vec<&Path> = if invocation_cwd != workspace_root {
        vec![invocation_cwd.as_path()]
    } else {
        vec![]
    };

    // Load template allow-list if -t was supplied
    let (shell_allow, shell_timeout, mut system_prompt, max_steps, validate_fn, max_validation_fails, mut enabled_tools, mut auto_approve_rules, template_context_window, template_hooks, mut template_description, mut template_labels, template_model, ignore_ssl_template) = if let Some(ref name) = template_name {
        let path = resolve_template(name, &extra_roots)?;
        println!("[HOST] Using template: {}", path.display());
        load_template(&path)?
    } else {
        (Vec::new(), SHELL_TIMEOUT_DEFAULT, None, None, None, None, all_tool_names(), Vec::new(), None, Vec::new(), None, Vec::new(), None, false)
    };
    // WASM1_IGNORE_SSL=1/true/yes overrides the template's ignore_ssl field.
    let ignore_ssl_env = env::var("WASM1_IGNORE_SSL")
        .ok()
        .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let ignore_ssl = ignore_ssl_template || ignore_ssl_env;
    if ignore_ssl_env && !ignore_ssl_template {
        println!("[HOST] SSL certificate validation disabled via WASM1_IGNORE_SSL env var");
    }
    let effective_hooks = merge_hooks(load_global_hooks()?, template_hooks);
    println!("[HOST] Hooks loaded: {}", effective_hooks.len());

    let mut seed_history: Vec<HistoryEntry> = Vec::new();
    let mut seed_validation_feedback: Vec<String> = Vec::new();
    let mut seed_step: u32 = 0;
    let mut seed_messages: Vec<SessionMessage> = Vec::new();
    let mut seed_pending_calls: Vec<PendingToolCall> = Vec::new();
    let mut seed_blocked_on_approval = false;
    let mut resume_model: Option<String> = None;
    let mut resume_prev_state: Option<String> = None;
    let (session_id, session_created, prompt) = if let Some(session_id) = session_name.clone() {
        let session_path = invocation_cwd
            .join(".agent/sessions")
            .join(format!("{session_id}.yaml"));
        let snapshot = load_session_seed(&session_path)?;
        let SessionSnapshotIn { metadata, spec } = snapshot;
        seed_messages = normalize_session_messages(&spec.messages);
        let (seed_prompt, parsed_history, parsed_feedback, parsed_step, parsed_pending_calls, blocked_on_approval) =
            session_seed_from_messages(&spec.messages);
        seed_history = parsed_history;
        seed_validation_feedback = parsed_feedback;
        seed_step = parsed_step;
        seed_pending_calls = parsed_pending_calls;
        seed_blocked_on_approval = blocked_on_approval;
        if system_prompt.is_none() {
            system_prompt = spec.system_prompt;
        }
        if !metadata.tools.is_empty() {
            enabled_tools = metadata.tools.clone();
        }
        if template_description.is_none() {
            template_description = metadata.description.clone();
        }
        if template_labels.is_empty() {
            template_labels = metadata.labels.clone();
        }
        if template_name.is_none() && !metadata.name.is_empty() {
            template_name = Some(metadata.name.clone());
        }
        if !metadata.model.is_empty() {
            resume_model = Some(metadata.model.clone());
        }
        resume_prev_state = metadata
            .status
            .as_deref()
            .map(normalize_stepwise_status)
            .or_else(|| metadata.state.as_deref().map(normalize_stepwise_status));
        let created = metadata.created.clone().unwrap_or_else(now_stamp);
        let effective_prompt = if let Some(new_prompt) = prompt.clone() {
            // A newly supplied resume prompt starts a fresh user turn.
            // Do not keep the session parked behind prior pending approvals.
            seed_blocked_on_approval = false;
            seed_pending_calls.clear();
            seed_messages.push(SessionMessage {
                role: "user".to_string(),
                verbatim: serde_json::json!({
                    "content": new_prompt,
                    "timestamp": now_stamp(),
                }),
                meta: serde_json::json!({
                    "sent": false,
                    "visible": true,
                }),
            });
            new_prompt
        } else {
            seed_prompt
        };
        (metadata.id, created, effective_prompt)
    } else {
        // Generate canonical session ID: <timestampMs>-<pid>-<hex4>
        let session_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let session_pid = std::process::id();
        let session_rand = {
            let mut h = Sha1::new();
            h.update(session_ts.to_le_bytes());
            h.update(session_pid.to_le_bytes());
            let digest = h.finalize();
            format!("{:02x}{:02x}", digest[0], digest[1])
        };
        let new_id = format!("{session_ts}-{session_pid}-{session_rand}");
        (new_id, now_stamp(), prompt.unwrap_or_default())
    };

    if !is_resume_mode {
        let stdin_text = if read_stdin { read_stdin_if_present()? } else { String::new() };
        if let Some(raw_system_prompt) = system_prompt.clone() {
            system_prompt = Some(render_system_prompt_template(
                &raw_system_prompt,
                &workspace_root,
                &stdin_text,
            )?);
        }
    }

    if is_resume_mode && auto_approve_rules.is_empty() {
        if let Some(ref name) = template_name {
            if let Ok(path) = resolve_template(name, &extra_roots) {
                if let Ok((_, _, _, _, _, _, _, rules, _, _, _, _, _, _)) = load_template(&path) {
                    auto_approve_rules = rules;
                }
            }
        }
    }

    println!("[HOST] Starting agent with prompt: {:?}", prompt);

    let model = template_model
        .or(resume_model)
        .or_else(|| env::var("XAI_MODEL").ok())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    if init_only_new_session {
        let sessions_dir = invocation_cwd.join(".agent/sessions");
        std::fs::create_dir_all(&sessions_dir).context("failed to create .agent/sessions/")?;
        let snapshot = SessionSnapshot {
            api_version: "daemon/v1".to_string(),
            kind: "Agent".to_string(),
            metadata: SessionMetadata {
                id: session_id.clone(),
                name: template_name.clone().unwrap_or_else(|| "solo".to_string()),
                model: model.clone(),
                status: "IDLE".to_string(),
                created: session_created.clone(),
                last_pid: std::process::id(),
                tools: enabled_tools.clone(),
                max_steps,
                labels: template_labels.clone(),
                description: template_description.clone(),
                last_transition: Some(SessionTransition {
                    action: "create".to_string(),
                    from: "IDLE".to_string(),
                    to: "IDLE".to_string(),
                    timestamp: now_stamp(),
                }),
            },
            spec: SessionSpec {
                system_prompt: system_prompt.clone(),
                messages: Vec::new(),
            },
        };
        let yaml = serde_yaml::to_string(&snapshot).context("failed to serialize session snapshot")?;
        let session_path = sessions_dir.join(format!("{}.yaml", session_id));
        fs::write(&session_path, yaml).with_context(|| format!("failed to write {}", session_path.display()))?;
        println!("{} IDLE .agent/sessions/{}.yaml", session_id, session_id);
        return Ok(());
    }

    let client = ClientBuilder::new()
        .danger_accept_invalid_certs(ignore_ssl)
        .build()
        .context("failed to build HTTP client")?;
    let (provider, model_name) = parse_provider_model(&model);
    let (api_key, provider_api_url, context_window) = match provider {
        ModelProvider::Xai => {
            let key = env::var("XAI_API_KEY")
                .context("xai provider requires XAI_API_KEY (set it in environment or .env)")?;
            let ctx = lookup_model_context_window(&model).or(template_context_window);
            (key, "https://api.x.ai".to_string(), ctx)
        }
        ModelProvider::Copilot => {
            // Use VS Code-style internal API authentication
            let (token, api_url) = resolve_copilot_internal_auth(&client)?;
            (token, api_url, template_context_window)
        }
    };
    let provider_name = match provider {
        ModelProvider::Xai => "xai",
        ModelProvider::Copilot => "copilot",
    };
    println!(
        "[HOST] Provider: {provider_name} | Model: {model} (native={model_name}) | Auth: loaded | context_window: {}",
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
        "get_session_seed",
        |mut caller: Caller<'_, HostState>, out_ptr: i32, out_cap: i32| -> i32 {
            let seed = SessionSeedPayload {
                prompt: caller.data().prompt.clone(),
                history: caller.data().seed_history.clone(),
                validation_feedback: caller.data().seed_validation_feedback.clone(),
                step: caller.data().seed_step,
                pending_tool_calls: caller.data().seed_pending_calls.clone(),
                blocked_on_approval: caller.data().seed_blocked_on_approval,
            };
            let raw = serde_json::to_string(&seed).unwrap_or_else(|_| "{}".to_string());
            write_memory(&mut caller, out_ptr, out_cap, &raw)
        },
    )?;

    linker.func_wrap(
        "host",
        "save_session_checkpoint",
        |mut caller: Caller<'_, HostState>, req_ptr: i32, req_len: i32, state_ptr: i32, state_len: i32, action_ptr: i32, action_len: i32| -> i32 {
            let req_json = match read_memory(&mut caller, req_ptr, req_len) {
                Ok(v) => v,
                Err(_) => return -1,
            };
            let req: GuestRequest = match serde_json::from_str(&req_json) {
                Ok(v) => v,
                Err(_) => return -2,
            };
            let session_state = match read_memory(&mut caller, state_ptr, state_len) {
                Ok(v) if !v.trim().is_empty() => v,
                _ => "IDLE".to_string(),
            };
            let action = match read_memory(&mut caller, action_ptr, action_len) {
                Ok(v) if !v.trim().is_empty() => v,
                _ => "checkpoint".to_string(),
            };
            if action == "tool_call_pending" {
                // grok_chat already persisted the assistant tool-call batch for this turn.
                // Avoid clobbering it with a req-only snapshot.
                return 0;
            }
            if write_session_snapshot(caller.data_mut(), &session_state, &action, Some(&req), None).is_err() {
                return -3;
            }
            0
        },
    )?;

    linker.func_wrap(
        "host",
        "get_max_steps",
        |caller: Caller<'_, HostState>| -> i32 {
            let one_turn_cap = caller.data().seed_step.saturating_add(1) as i32;
            match caller.data().max_steps {
                Some(template_cap) => one_turn_cap.min(template_cap as i32),
                None => one_turn_cap,
            }
        },
    )?;

    linker.func_wrap(
        "host",
        "get_validate",
        |mut caller: Caller<'_, HostState>, out_ptr: i32, out_cap: i32| -> i32 {
            let validate_fn = caller
                .data()
                .validate_fn
                .as_deref()
                .unwrap_or("")
                .to_string();
            write_memory(&mut caller, out_ptr, out_cap, &validate_fn)
        },
    )?;

    linker.func_wrap(
        "host",
        "get_max_validation_fails",
        |caller: Caller<'_, HostState>| -> i32 {
            caller
                .data()
                .max_validation_fails
                .map(|n| n as i32)
                .unwrap_or(-1)
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
                        perf: None,
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
                        perf: None,
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
                    perf: None,
                },
            };

            if let LlmDecision::ToolCall { tool_calls, .. } = &decision {
                println!("[LLM → GUEST] Tool calls: {}", tool_calls.len());
            }

            let (status, action) = match &decision {
                LlmDecision::Final { .. } => ("SUCCESS", "complete"),
                LlmDecision::Error { .. } => ("FAIL", "fail"),
                LlmDecision::ToolCall { .. } => ("IDLE", "tool_call"),
            };
            if let Err(e) = write_session_snapshot(caller.data_mut(), status, action, Some(&req), Some(&decision)) {
                eprintln!("[HOST] session snapshot write failed: {e:#}");
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

            // Return human-readable summary string
            let stdout = &final_out.stdout;
            let byte_count = stdout.len();
            let line_count = if stdout.is_empty() { 0 } else { stdout.lines().count() };
            let exit_code = final_out.exit_code.unwrap_or(-1);
            let response = format!(
                "Program PID {pid} exited with code {exit_code} and its output \
                 ({line_count} line(s), {byte_count} byte(s)) was written to file: {guest_path}"
            );
            write_memory(&mut caller, out_ptr, out_cap, &response)
        },
    )?;

    // ── shell_run_async ───────────────────────────────────────────────────────
    linker.func_wrap(
        "host",
        "shell_run_async",
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
                    "[HOST] shell_run_async: command denied by allow-list: {full_cmd:?}"
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
            let vfs_path = format!("tmp/{}_{}.out.json", now_ms, sha_hex);
            let guest_path = format!("/tmp/{}_{}.out.json", now_ms, sha_hex);

            // Write initial JSON to pending_writes
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
                .push((vfs_path, initial_json.into_bytes()));

            println!("[HOST] shell_run_async: spawning {:?} {:?}", cmd, args);

            // Spawn child without waiting
            let child_result = Command::new(&cmd)
                .args(&args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();

            let child = match child_result {
                Err(e) => {
                    println!("[HOST] shell_run_async: spawn failed: {e}");
                    return -2;
                }
                Ok(c) => c,
            };

            let pid = child.id();
            // Register in session — caller can use shell.kill / shell.stdin
            caller.data_mut().running_processes.insert(pid, child);

            // Return launch confirmation string immediately (no blocking wait)
            let response = format!(
                "Program PID {pid} launched and output is streaming to file: {guest_path}"
            );
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

            if !caller.data().enabled_tools.iter().any(|t| t == &name) {
                let resp = format!(
                    r#"{{"error":"tool disabled by template: {}"}}"#,
                    serde_json::to_string(&name).unwrap_or_else(|_| "\"unknown\"".to_string())
                );
                return write_memory(&mut caller, out_ptr, out_cap, &resp);
            }

            let hook_payload = serde_json::json!({
                "tool_name": name.clone(),
                "tool_input": args.clone(),
            });
            match run_hooks(caller.data_mut(), "pre_tool_call", &hook_payload, true) {
                Ok(Some(reason)) => {
                    let escaped = serde_json::to_string(&reason).unwrap_or_else(|_| "\"blocked\"".to_string());
                    let resp = format!(r#"{{"error":{escaped}}}"#);
                    return write_memory(&mut caller, out_ptr, out_cap, &resp);
                }
                Ok(None) => {}
                Err(e) => {
                    let escaped = serde_json::to_string(&format!("hook error: {e}")).unwrap_or_else(|_| "\"hook error\"".to_string());
                    let resp = format!(r#"{{"error":{escaped}}}"#);
                    return write_memory(&mut caller, out_ptr, out_cap, &resp);
                }
            }

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
                "msgq__append" => msgq_append(caller.data().workspace_root.as_path(), &args),
                "msgq__claim" => msgq_claim(caller.data().workspace_root.as_path(), &args),
                "msgq__list" => msgq_list(caller.data().workspace_root.as_path(), &args),
                "msgq__await" => msgq_await(caller.data().workspace_root.as_path(), &args),
                "msgq__update" => msgq_update(caller.data().workspace_root.as_path(), &args),
                "msgq__archive" => msgq_archive(caller.data_mut(), &args),
                "msgq__bcast" => msgq_bcast(caller.data().workspace_root.as_path(), &args),
                "team__create" => team_create(caller.data().workspace_root.as_path(), &args),
                "team__destroy" => team_destroy(caller.data().workspace_root.as_path(), &args),
                _ => Err(format!("unknown tool: {name}")),
            };

            match &result {
                Ok(output) => {
                    let _ = run_hooks(
                        caller.data_mut(),
                        "post_tool_call",
                        &serde_json::json!({
                            "tool_name": name,
                            "tool_input": args,
                            "tool_output": output,
                        }),
                        false,
                    );
                }
                Err(error_message) => {
                    let _ = run_hooks(
                        caller.data_mut(),
                        "post_tool_failure",
                        &serde_json::json!({
                            "tool_name": name,
                            "tool_input": args,
                            "error_message": error_message,
                        }),
                        false,
                    );
                }
            }

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

    println!("[HOST] Session: {session_id}");

    // .agent/fs is local to the invocation cwd (each working directory gets its own vfs)
    let tcow_dir = invocation_cwd.join(".agent/fs");
    std::fs::create_dir_all(&tcow_dir).context("failed to create .agent/fs/")?;
    let tcow_path_buf = tcow_dir.join(format!("{session_id}.tcow"));
    println!("[HOST] TCOW virtual FS: {}", rel_path(&invocation_cwd, &tcow_path_buf));
    let tcow_path = tcow_path_buf.to_string_lossy().into_owned();

    // .agent/sessions resolves relative to invocation cwd
    let sessions_dir = invocation_cwd.join(".agent/sessions");
    std::fs::create_dir_all(&sessions_dir).context("failed to create .agent/sessions/")?;

    let wasi = WasiCtxBuilder::new().inherit_stdio().build();
    let state = HostState {
        prompt,
        final_answer: None,
        api_key,
        provider,
        model_name,
        provider_api_url,
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
        validate_fn,
        max_validation_fails,
        enabled_tools,
        auto_approve_rules,
        context_window,
        session_id,
        workspace_root,
        invocation_cwd,
        hooks: effective_hooks,
        session_created,
        session_state: resume_prev_state.unwrap_or_else(|| "RUNNING".to_string()),
        template_name: template_name.clone().unwrap_or_else(|| "solo".to_string()),
        template_description,
        template_labels,
        seed_history,
        seed_validation_feedback,
        seed_step,
        seed_messages,
        seed_pending_calls,
        seed_blocked_on_approval,
    };

    let mut store = Store::new(&engine, state);
    if !is_resume_mode {
        write_session_snapshot(store.data_mut(), "RUNNING", "create", None, None)?;
    }
    store
        .add_fuel(FUEL_LIMIT)
        .context("failed to set fuel limit")?;

    let instance = linker.instantiate(&mut store, &module)?;
    let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;

    {
        let payload = serde_json::json!({
            "template": template_name.clone().unwrap_or_else(|| "(none)".to_string()),
            "prompt": store.data().prompt,
        });
        if let Some(reason) = run_hooks(store.data_mut(), "before_agent_start", &payload, true)? {
            return Err(anyhow!("blocked by hook before_agent_start: {reason}"));
        }
        if !is_resume_mode {
            write_session_snapshot(store.data_mut(), "RUNNING", "start", None, None)?;
        }
        let _ = run_hooks(
            store.data_mut(),
            "session_start",
            &serde_json::json!({}),
            false,
        )?;
    }

    run.call(&mut store, ())?;

    // Flush buffered writes to the .tcow file
    {
        let state = store.data();
        if !state.pending_writes.is_empty() {
            let tcow_path = &state.tcow_path;
            let writes = &state.pending_writes;
            println!("[HOST] Flushing {} write(s) to {}", writes.len(), rel_path(&state.invocation_cwd, Path::new(tcow_path)));
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

    let had_final = store.data().final_answer.is_some();
    let needs_stop_fallback = !had_final && normalize_stepwise_status(&store.data().session_state) == "RUNNING";
    if needs_stop_fallback {
        write_session_snapshot(
            store.data_mut(),
            "IDLE",
            "stop",
            None,
            None,
        )?;
    }
    let _ = run_hooks(
        store.data_mut(),
        "session_end",
        &serde_json::json!({
            "exit_reason": if had_final { "success" } else { "no_final" }
        }),
        false,
    )?;

    let summary_path = format!(".agent/sessions/{}.yaml", store.data().session_id);
    println!("{} {} {}", store.data().session_id, store.data().session_state, summary_path);

    Ok(())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_stamp() -> String {
    now_millis().to_string()
}

fn looks_like_workspace_root(dir: &Path) -> bool {
    // Require .agent/templates/ — the presence of just .agent/ is not strong
    // enough since agent runs create .agent/fs/ in the invocation cwd.
    dir.join(".agent/templates").exists()
        || dir.join("Cargo.toml").exists()
        || dir.join("config.yaml").exists()
}

fn resolve_workspace_root() -> Result<PathBuf> {
    // 1. Explicit env override
    if let Ok(from_env) = env::var("WASM1_WORKSPACE_ROOT") {
        let p = PathBuf::from(from_env);
        if looks_like_workspace_root(&p) {
            return Ok(p);
        }
    }

    // 2. Derive from binary location by walking upward from the resolved
    //    executable path (after following symlinks).
    if let Ok(exe) = env::current_exe() {
        let exe_resolved = fs::canonicalize(&exe).unwrap_or(exe);
        let mut cur = exe_resolved.parent().map(Path::to_path_buf);
        while let Some(dir) = cur {
            if looks_like_workspace_root(&dir) {
                return Ok(dir);
            }
            cur = dir.parent().map(Path::to_path_buf);
        }
    }

    // 3. Final fallback: use invocation cwd.
    let cwd = env::current_dir().context("failed to get current directory")?;
    Ok(cwd)
}

fn os_release_string() -> String {
    if let Ok(text) = fs::read_to_string("/proc/sys/kernel/osrelease") {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(out) = Command::new("uname").arg("-r").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "unknown".to_string()
}

fn read_stdin_if_present() -> Result<String> {
    let is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
    if is_tty {
        return Ok(String::new());
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("failed reading stdin for -i")?;
    Ok(buf)
}

fn resolve_workspace_relative_file(workspace_root: &Path, rel: &str) -> Result<PathBuf> {
    let candidate = workspace_root.join(rel);
    let canonical_candidate = fs::canonicalize(&candidate)
        .with_context(|| format!("includePrompt path not found: {rel}"))?;
    let canonical_root = fs::canonicalize(workspace_root)
        .with_context(|| format!("failed to resolve workspace root: {}", workspace_root.display()))?;
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(anyhow!("includePrompt path escapes workspace root: {rel}"));
    }
    Ok(canonical_candidate)
}

fn render_system_prompt_template(raw: &str, workspace_root: &Path, stdin_text: &str) -> Result<String> {
    let mut env = MiniJinjaEnv::new();
    let stdin_owned = stdin_text.to_string();
    env.add_function("readStdin", move || -> String { stdin_owned.clone() });

    let include_root = workspace_root.to_path_buf();
    env.add_function("includePrompt", move |rel_path: String| -> Result<String, minijinja::Error> {
        if Path::new(&rel_path).is_absolute() {
            return Err(minijinja::Error::new(
                minijinja::ErrorKind::InvalidOperation,
                "includePrompt expects a workspace-relative path",
            ));
        }
        let path = resolve_workspace_relative_file(&include_root, &rel_path).map_err(|e| {
            minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, e.to_string())
        })?;
        fs::read_to_string(&path)
            .map_err(|e| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, e.to_string()))
    });

    env.add_function("shell", move |cmd: String| -> Result<String, minijinja::Error> {
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let out = Command::new(shell)
            .arg("-lc")
            .arg(&cmd)
            .output()
            .map_err(|e| minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, e.to_string()))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(minijinja::Error::new(
                minijinja::ErrorKind::InvalidOperation,
                format!("shell helper command failed: {stderr}"),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    });

    let process_shell = env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let ctx = mj_context! {
        process => serde_json::json!({
            "cwd": workspace_root.display().to_string(),
            "env": env::vars().collect::<HashMap<String, String>>(),
            "platform": env::consts::OS,
            "shell": process_shell,
        }),
        os => serde_json::json!({
            "release": os_release_string(),
        }),
    };

    env.add_template("system_prompt", raw)
        .map_err(|e| anyhow!("invalid system_prompt template: {e}"))?;
    let tmpl = env
        .get_template("system_prompt")
        .map_err(|e| anyhow!("failed loading system_prompt template: {e}"))?;
    tmpl.render(ctx)
        .map_err(|e| anyhow!("failed rendering system_prompt template: {e}"))
}

fn load_or_init_cron_interval_ms(workspace_root: &Path) -> Result<u64> {
    let path = workspace_root.join("config.yaml");
    if !path.exists() {
        let default_cfg = serde_yaml::to_string(&serde_json::json!({
            "cron": {
                "interval_ms": DEFAULT_CRON_INTERVAL_MS
            }
        }))
        .context("failed to serialize default config.yaml")?;
        fs::write(&path, default_cfg)
            .with_context(|| format!("failed to create {}", path.display()))?;
        return Ok(DEFAULT_CRON_INTERVAL_MS);
    }

    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(DEFAULT_CRON_INTERVAL_MS);
    }

    let yaml_val: serde_yaml::Value = serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let json_val = serde_json::to_value(yaml_val).context("failed to convert config YAML")?;

    let interval = json_val
        .get("cron")
        .and_then(|v| v.get("interval_ms"))
        .and_then(value_as_i64)
        .filter(|v| *v > 0)
        .map(|v| v as u64)
        .unwrap_or(DEFAULT_CRON_INTERVAL_MS);

    Ok(interval)
}

#[derive(Debug, Default)]
struct CronIterationStats {
    hooks_ran: usize,
    hooks_skipped: usize,
    hooks_success: usize,
    hooks_failed: usize,
    elapsed_ms: u128,
}

fn c24(text: &str, r: u8, g: u8, b: u8) -> String {
    format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
}

fn run_cron(mode: &str, template_name: Option<&str>, verbose: bool) -> Result<()> {
    let workspace_root = resolve_workspace_root()?;
    env::set_current_dir(&workspace_root)
        .with_context(|| format!("failed to set current dir to {}", workspace_root.display()))?;
    let cron_interval_ms = load_or_init_cron_interval_ms(&workspace_root)?;
    let session_id = format!("cron-{}-{}", std::process::id(), now_millis());
    let global_hooks = load_global_hooks()?;
    let template_hooks = if let Some(name) = template_name {
        let path = resolve_template(name, &[])?;
        read_hooks_from_template_file(&path)?
    } else {
        load_all_template_hooks()?
    };
    let hooks = merge_hooks(global_hooks, template_hooks)
        .into_iter()
        .filter(|h| h.on == "cron_tick")
        .collect::<Vec<_>>();

    if verbose {
        println!(
            "{} {}",
            c24("[cron]", 120, 180, 255),
            c24("startup: discovered cron_tick hooks", 200, 210, 220)
        );
        println!(
            "{} {} {}ms ({})",
            c24("[cron]", 120, 180, 255),
            c24("loop interval:", 200, 210, 220),
            c24(&cron_interval_ms.to_string(), 160, 200, 255),
            workspace_root.join("config.yaml").display(),
        );
        for hook in &hooks {
            let enabled = hook.enabled.unwrap_or(true);
            let mark = if enabled {
                c24("✓", 110, 220, 140)
            } else {
                c24("✗", 255, 160, 100)
            };
            let state = if enabled {
                c24("enabled", 110, 220, 140)
            } else {
                c24("disabled", 255, 160, 100)
            };
            println!("  {} {} ({})", mark, hook.name, state);
        }
        println!(
            "{} {} {}",
            c24("[cron]", 120, 180, 255),
            c24("hook count:", 200, 210, 220),
            c24(&hooks.len().to_string(), 160, 200, 255)
        );
    }

    match mode {
        "once" => {
            let stats = run_cron_tick(
                &hooks,
                &workspace_root,
                &session_id,
                template_name,
                "once",
                false,
                verbose,
            )?;
            if verbose {
                println!(
                    "{} {} ran={}, skipped={}, success={}, failed={}, total={}ms",
                    c24("[cron]", 120, 180, 255),
                    c24("summary:", 200, 210, 220),
                    c24(&stats.hooks_ran.to_string(), 160, 200, 255),
                    c24(&stats.hooks_skipped.to_string(), 160, 200, 255),
                    c24(&stats.hooks_success.to_string(), 110, 220, 140),
                    c24(&stats.hooks_failed.to_string(), 255, 120, 120),
                    c24(&stats.elapsed_ms.to_string(), 180, 180, 255),
                );
            }
            Ok(())
        }
        _ => Err(anyhow!("usage: wasm1 cron [once] [-t <template>] [-v]")),
    }
}

fn cron_state_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".agent/cron/state.yaml")
}

fn load_cron_state(workspace_root: &Path) -> Result<HashMap<String, serde_json::Map<String, serde_json::Value>>> {
    let path = cron_state_path(workspace_root);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("invalid cron state path"))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read cron state: {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let yaml_val: serde_yaml::Value = serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse cron state YAML: {}", path.display()))?;
    let json_val = serde_json::to_value(yaml_val).context("failed to convert cron state YAML to JSON")?;
    let mut out: HashMap<String, serde_json::Map<String, serde_json::Value>> = HashMap::new();
    if let Some(obj) = json_val.as_object() {
        for (k, v) in obj {
            if let Some(entry_obj) = v.as_object() {
                out.insert(k.clone(), entry_obj.clone());
            }
        }
    }
    Ok(out)
}

fn save_cron_state(
    workspace_root: &Path,
    state: &HashMap<String, serde_json::Map<String, serde_json::Value>>,
) -> Result<()> {
    let mut root = serde_json::Map::new();
    for (k, v) in state {
        root.insert(k.clone(), serde_json::Value::Object(v.clone()));
    }
    let yaml = serde_yaml::to_string(&serde_json::Value::Object(root))
        .context("failed to serialize cron state")?;
    fs::write(cron_state_path(workspace_root), yaml).context("failed to write cron state")
}

fn value_as_i64(value: &serde_json::Value) -> Option<i64> {
    if let Some(v) = value.as_i64() {
        return Some(v);
    }
    if let Some(v) = value.as_u64() {
        return i64::try_from(v).ok();
    }
    if let Some(v) = value.as_f64() {
        return Some(v as i64);
    }
    if let Some(s) = value.as_str() {
        return s.trim().parse::<i64>().ok();
    }
    None
}

fn resolve_cron_agent_name(hook: &HookDef, template_name: Option<&str>) -> String {
    for job in hook.jobs.values() {
        for step in &job.steps {
            if step.step_type == "llm" {
                if let Some(t) = step.template.as_deref() {
                    if !t.trim().is_empty() {
                        return t.trim().to_string();
                    }
                }
            }
        }
    }
    template_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| "global".to_string())
}

fn apply_cron_schedule_from_output(
    output: Option<&str>,
    now_ms: u64,
    entry: &mut serde_json::Map<String, serde_json::Value>,
) {
    let mut computed_next_run: Option<i64> = None;
    if let Some(raw) = output {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) {
            if let Some(obj) = value.as_object() {
                for (k, v) in obj {
                    entry.insert(k.clone(), v.clone());
                }
                if let Some(ms_rel) = obj.get("nextRunInMs").and_then(value_as_i64) {
                    let next = now_ms as i64 + ms_rel.max(0);
                    computed_next_run = Some(next);
                    entry.insert("nextRunAt".to_string(), serde_json::json!(next));
                } else if let Some(abs) = obj.get("nextRunAt").and_then(value_as_i64) {
                    computed_next_run = Some(abs.max(0));
                    entry.insert("nextRunAt".to_string(), serde_json::json!(abs.max(0)));
                }
            }
        }
    }
    if computed_next_run.is_none() && !entry.contains_key("nextRunAt") {
        entry.insert("nextRunAt".to_string(), serde_json::json!(0));
    }
}

fn run_cron_tick(
    hooks: &[HookDef],
    workspace_root: &Path,
    session_id: &str,
    template_name: Option<&str>,
    trigger: &str,
    respect_schedule: bool,
    verbose: bool,
) -> Result<CronIterationStats> {
    let tick_started = Instant::now();
    let base_tick = now_millis();
    let mut stats = CronIterationStats::default();

    for hook in hooks.iter().filter(|h| h.on == "cron_tick") {
        let enabled = hook.enabled.unwrap_or(true);
        if !enabled {
            stats.hooks_skipped += 1;
            if verbose {
                println!(
                    "{} {} {} ({})",
                    c24("[cron]", 120, 180, 255),
                    c24("SKIP", 255, 160, 100),
                    hook.name,
                    c24("disabled", 255, 160, 100)
                );
            }
            continue;
        }

        let cron_state = load_cron_state(workspace_root)?;
        let agent_name = resolve_cron_agent_name(hook, template_name);
        let state_key = format!("{}:{}", agent_name, hook.name);
        let existing_entry = cron_state
            .get(&state_key)
            .cloned()
            .unwrap_or_default();

        let next_run_at = existing_entry
            .get("nextRunAt")
            .and_then(value_as_i64)
            .unwrap_or(0);

        if respect_schedule && next_run_at > base_tick as i64 {
            stats.hooks_skipped += 1;
            if verbose {
                let wait_ms = next_run_at.saturating_sub(base_tick as i64);
                println!(
                    "{} {} {} ({})",
                    c24("[cron]", 120, 180, 255),
                    c24("SKIP", 255, 200, 120),
                    hook.name,
                    c24(&format!("nextRunAt in {wait_ms}ms"), 255, 200, 120)
                );
            }
            continue;
        }

        let mut payload = serde_json::json!({
            "trigger": trigger,
            "tick_at": now_stamp(),
            "cron": {
                "stateKey": state_key,
                "nowMs": base_tick,
                "state": serde_json::Value::Object(existing_entry.clone()),
            }
        });
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("agent_name".to_string(), serde_json::json!(agent_name));
            obj.insert("hook_name".to_string(), serde_json::json!(hook.name.clone()));
        }

        let mut base = serde_json::json!({
            "hook": "cron_tick",
            "session_id": session_id,
            "timestamp": now_stamp(),
            "agent_id": "main",
            "workspace": workspace_root.display().to_string(),
        });
        merge_json_object(&mut base, &payload);

        if !hook_matches(hook, &base) {
            continue;
        }

        let hook_started = Instant::now();
        stats.hooks_ran += 1;
        let run = execute_hook_collect(hook, &base, workspace_root);
        match run {
            Ok(run_result) => {
                let mut latest_state = load_cron_state(workspace_root)?;
                let mut entry = existing_entry;
                entry.insert("lastRunAt".to_string(), serde_json::json!(now_millis()));
                apply_cron_schedule_from_output(run_result.last_llm_output.as_deref(), now_millis(), &mut entry);
                latest_state.insert(state_key, entry);
                save_cron_state(workspace_root, &latest_state)?;
                let elapsed = hook_started.elapsed().as_millis();
                if let Some(reason) = run_result.blocked_reason {
                    stats.hooks_failed += 1;
                    eprintln!("[HOOK:{}] blocked: {reason}", hook.name);
                    if verbose {
                        println!(
                            "{} {} {} ({}; {}ms)",
                            c24("[cron]", 120, 180, 255),
                            c24("RUN", 140, 220, 255),
                            hook.name,
                            c24("blocked", 255, 160, 100),
                            elapsed
                        );
                    }
                } else {
                    stats.hooks_success += 1;
                    if verbose {
                        println!(
                            "{} {} {} ({}; {}ms)",
                            c24("[cron]", 120, 180, 255),
                            c24("RUN", 140, 220, 255),
                            hook.name,
                            c24("success", 110, 220, 140),
                            elapsed
                        );
                    }
                }
            }
            Err(err) => {
                let mut latest_state = load_cron_state(workspace_root)?;
                eprintln!("[HOOK:{}] error: {err}", hook.name);
                let mut entry = existing_entry;
                entry.insert("lastRunAt".to_string(), serde_json::json!(now_millis()));
                latest_state.insert(state_key, entry);
                save_cron_state(workspace_root, &latest_state)?;
                stats.hooks_failed += 1;
                if verbose {
                    println!(
                        "{} {} {} ({}; {}ms)",
                        c24("[cron]", 120, 180, 255),
                        c24("RUN", 140, 220, 255),
                        hook.name,
                        c24("failed", 255, 120, 120),
                        hook_started.elapsed().as_millis()
                    );
                }
            }
        }
    }

    stats.elapsed_ms = tick_started.elapsed().as_millis();
    Ok(stats)
}

fn read_hooks_from_template_file(path: &Path) -> Result<Vec<HookDef>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read template: {}", path.display()))?;
    let root: serde_yaml::Value = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse template YAML: {}", path.display()))?;
    let hooks_value = root
        .get("metadata")
        .and_then(|m| m.get("hooks"))
        .cloned()
        .unwrap_or_else(|| serde_yaml::Value::Sequence(Vec::new()));

    if matches!(hooks_value, serde_yaml::Value::Null) {
        return Ok(Vec::new());
    }

    let hooks: Vec<HookDef> = serde_yaml::from_value(hooks_value)
        .with_context(|| format!("failed to parse metadata.hooks in template: {}", path.display()))?;
    Ok(hooks)
}

fn load_all_template_hooks() -> Result<Vec<HookDef>> {
    let mut hooks = Vec::new();
    let dirs = vec![PathBuf::from(".agent/templates")];
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(&dir)
            .with_context(|| format!("failed to read template dir: {}", dir.display()))?
        {
            let entry = match entry {
                Ok(v) => v,
                Err(_) => continue,
            };
            let path = entry.path();
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext != "yaml" && ext != "yml" {
                continue;
            }
            match read_hooks_from_template_file(&path) {
                Ok(mut hs) => hooks.append(&mut hs),
                Err(e) => eprintln!("[HOST] skipping template hooks from {}: {e:#}", path.display()),
            }
        }
    }
    Ok(hooks)
}

fn read_hook_dir(dir: &Path) -> Result<Vec<HookDef>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut hooks = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = match entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read hook file: {}", path.display()))?;
        let parsed: HookFile = serde_yaml::from_str(&content)
            .with_context(|| format!("failed to parse hook YAML: {}", path.display()))?;
        hooks.extend(parsed.hooks);
    }
    Ok(hooks)
}

fn load_global_hooks() -> Result<Vec<HookDef>> {
    let repo_hooks = read_hook_dir(Path::new(".agent/hooks"))?;
    Ok(repo_hooks)
}

fn merge_hooks(low: Vec<HookDef>, high: Vec<HookDef>) -> Vec<HookDef> {
    let mut index: HashMap<(String, String), HookDef> = HashMap::new();
    for hook in low {
        index.insert((hook.on.clone(), hook.name.clone()), hook);
    }
    for hook in high {
        index.insert((hook.on.clone(), hook.name.clone()), hook);
    }
    let mut out: Vec<HookDef> = index.into_values().collect();
    out.sort_by(|a, b| a.on.cmp(&b.on).then(a.name.cmp(&b.name)));
    out
}

fn run_hooks(
    state: &mut HostState,
    event: &str,
    payload: &serde_json::Value,
    blocking: bool,
) -> Result<Option<String>> {
    run_hooks_impl(
        &state.hooks,
        &state.workspace_root,
        &state.session_id,
        event,
        payload,
        blocking,
    )
}

fn run_hooks_impl(
    hooks: &[HookDef],
    workspace_root: &Path,
    session_id: &str,
    event: &str,
    payload: &serde_json::Value,
    blocking: bool,
) -> Result<Option<String>> {
    let mut base = serde_json::json!({
        "hook": event,
        "session_id": session_id,
        "timestamp": now_stamp(),
        "agent_id": "main",
        "workspace": workspace_root.display().to_string(),
    });
    merge_json_object(&mut base, payload);

    for hook in hooks
        .iter()
        .filter(|h| h.on == event && h.enabled.unwrap_or(true))
    {
        if !hook_matches(hook, &base) {
            continue;
        }
        match execute_hook(hook, &base, workspace_root) {
            Ok(Some(reason)) => {
                if blocking {
                    return Ok(Some(reason));
                }
            }
            Ok(None) => {}
            Err(err) => {
                if blocking {
                    return Ok(Some(format!("hook '{}' failed: {err}", hook.name)));
                }
                eprintln!("[HOOK:{}] error: {err}", hook.name);
            }
        }
    }
    Ok(None)
}

fn hook_matches(hook: &HookDef, payload: &serde_json::Value) -> bool {
    for (key, matcher) in &hook.when {
        let target = get_json_path(payload, key).and_then(|v| v.as_str()).unwrap_or("");
        match matcher {
            serde_yaml::Value::String(s) => {
                if let Some(prefix) = s.strip_suffix('*') {
                    if !target.starts_with(prefix) {
                        return false;
                    }
                } else if target != s {
                    return false;
                }
            }
            serde_yaml::Value::Sequence(seq) => {
                let mut any = false;
                for entry in seq {
                    if let serde_yaml::Value::String(s) = entry {
                        if s == target {
                            any = true;
                            break;
                        }
                    }
                }
                if !any {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn execute_hook(hook: &HookDef, payload: &serde_json::Value, workspace_root: &Path) -> Result<Option<String>> {
    Ok(execute_hook_collect(hook, payload, workspace_root)?.blocked_reason)
}

fn execute_hook_collect(
    hook: &HookDef,
    payload: &serde_json::Value,
    workspace_root: &Path,
) -> Result<HookRunResult> {
    let mut completed: HashMap<String, ()> = HashMap::new();
    let mut outputs: HashMap<String, String> = HashMap::new();
    let mut last_llm_output: Option<String> = None;

    while completed.len() < hook.jobs.len() {
        let mut progressed = false;
        let job_names: Vec<String> = hook.jobs.keys().cloned().collect();
        for job_name in job_names {
            if completed.contains_key(&job_name) {
                continue;
            }
            let job = match hook.jobs.get(&job_name) {
                Some(v) => v,
                None => continue,
            };
            if !job.needs.iter().all(|n| completed.contains_key(n)) {
                continue;
            }
            let mut job_outputs: HashMap<String, String> = HashMap::new();
            for (idx, step) in job.steps.iter().enumerate() {
                let step_out = execute_hook_step(step, payload, &outputs, workspace_root)
                    .with_context(|| format!("job={job_name} step={}", step.id.clone().unwrap_or_else(|| idx.to_string())))?;
                let step_id = step.id.clone().unwrap_or_else(|| format!("step_{idx}"));
                job_outputs.insert(step_id.clone(), step_out.clone());
                outputs.insert(step_id, step_out.clone());

                let parsed: serde_json::Value = serde_json::from_str(&step_out).unwrap_or(serde_json::Value::Null);
                if parsed["blocked"].as_bool().unwrap_or(false) {
                    let reason = parsed["reason"].as_str().unwrap_or("blocked by hook").to_string();
                    return Ok(HookRunResult {
                        blocked_reason: Some(reason),
                        last_llm_output,
                    });
                }
                if step.step_type == "llm" {
                    last_llm_output = Some(step_out);
                }
            }
            let _ = job_outputs;
            completed.insert(job_name, ());
            progressed = true;
        }
        if !progressed {
            break;
        }
    }

    Ok(HookRunResult {
        blocked_reason: None,
        last_llm_output,
    })
}

fn execute_hook_step(
    step: &HookStep,
    payload: &serde_json::Value,
    outputs: &HashMap<String, String>,
    workspace_root: &Path,
) -> Result<String> {
    match step.step_type.as_str() {
        "shell" => {
            let command = render_expr_template(step.command.as_deref().unwrap_or(""), payload, outputs)?;
            let stdin_text = render_expr_template(step.stdin.as_deref().unwrap_or(""), payload, outputs)?;
            let mut cmd = Command::new("bash");
            cmd.arg("-lc")
                .arg(command)
                .current_dir(workspace_root)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .stdin(Stdio::piped());
            let mut child = cmd.spawn().context("failed to spawn hook shell step")?;
            if !stdin_text.is_empty() {
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin.write_all(stdin_text.as_bytes()).ok();
                }
            }
            let out = child.wait_with_output().context("hook shell wait failed")?;
            if !out.status.success() {
                return Err(anyhow!(
                    "hook shell failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        "llm" => {
            let template = step.template.clone().unwrap_or_default();
            let prompt = render_expr_template(step.prompt.as_deref().unwrap_or(""), payload, outputs)?;
            if template.is_empty() || prompt.is_empty() {
                return Err(anyhow!("llm step requires template and prompt"));
            }
            let exe = env::current_exe().context("cannot resolve current executable")?;
            let mut cmd = Command::new(exe);
            cmd.arg("-t").arg(template).arg(prompt).current_dir(workspace_root);
            let out = cmd.output().context("failed to run nested llm hook step")?;
            if !out.status.success() {
                return Err(anyhow!("nested llm hook failed"));
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines().rev() {
                if let Some(rest) = line.strip_prefix("[HOST] Agent loop complete. Final answer: ") {
                    return Ok(rest.to_string());
                }
            }
            Ok(stdout.trim().to_string())
        }
        other => Err(anyhow!("unsupported hook step type: {other}")),
    }
}

fn render_expr_template(input: &str, payload: &serde_json::Value, outputs: &HashMap<String, String>) -> Result<String> {
    let re = Regex::new(r#"\$\{\{\s*([^}]*)\s*\}\}"#).context("failed to compile hook expression regex")?;
    let mut out = String::new();
    let mut last = 0;
    for caps in re.captures_iter(input) {
        let m = match caps.get(0) {
            Some(v) => v,
            None => continue,
        };
        out.push_str(&input[last..m.start()]);
        let expr = caps.get(1).map(|x| x.as_str()).unwrap_or("");
        let value = eval_hook_expr(expr, payload, outputs)?;
        if let Some(s) = value.as_str() {
            out.push_str(s);
        } else if value.is_null() {
        } else {
            out.push_str(&value.to_string());
        }
        last = m.end();
    }
    out.push_str(&input[last..]);
    Ok(out)
}

fn eval_hook_expr(expr: &str, payload: &serde_json::Value, outputs: &HashMap<String, String>) -> Result<serde_json::Value> {
    let expr = expr.trim();
    if expr.starts_with("parseJSON(") {
        let close = expr.find(')').ok_or_else(|| anyhow!("invalid parseJSON expression"))?;
        let inner = &expr[10..close];
        let inner_value = eval_hook_expr(inner, payload, outputs)?;
        let parsed: serde_json::Value = serde_json::from_str(inner_value.as_str().unwrap_or(""))
            .context("parseJSON received invalid JSON")?;
        let rest = expr[close + 1..].trim();
        if let Some(path) = rest.strip_prefix('.') {
            return Ok(get_json_path(&parsed, path).cloned().unwrap_or(serde_json::Value::Null));
        }
        return Ok(parsed);
    }
    if let Some(step_path) = expr.strip_prefix("steps.") {
        let mut parts = step_path.split('.');
        let id = parts.next().unwrap_or("");
        let field = parts.next().unwrap_or("output");
        if field == "output" {
            return Ok(serde_json::Value::String(outputs.get(id).cloned().unwrap_or_default()));
        }
    }
    Ok(get_json_path(payload, expr).cloned().unwrap_or(serde_json::Value::Null))
}

fn get_json_path<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for seg in path.split('.') {
        if seg.is_empty() {
            continue;
        }
        current = current.get(seg)?;
    }
    Some(current)
}

fn merge_json_object(base: &mut serde_json::Value, extra: &serde_json::Value) {
    if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
}

fn session_snapshot_path(state: &HostState) -> PathBuf {
    state
        .invocation_cwd
        .join(".agent/sessions")
        .join(format!("{}.yaml", state.session_id))
}

fn clear_dir(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut removed = 0u64;
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            fs::remove_dir_all(&p)
                .with_context(|| format!("failed to remove directory {}", p.display()))?;
        } else {
            fs::remove_file(&p)
                .with_context(|| format!("failed to remove file {}", p.display()))?;
        }
        removed += 1;
    }
    Ok(removed)
}

fn run_clean() -> Result<()> {
    let invocation_cwd = env::current_dir().context("failed to get current directory")?;
    let workspace_root = resolve_workspace_root()?;

    // Collect the roots to clean, cwd first, then workspace root.
    // Deduplicate so we don't double-clean when cwd == workspace root.
    let mut roots: Vec<&Path> = vec![invocation_cwd.as_path()];
    if workspace_root != invocation_cwd {
        roots.push(workspace_root.as_path());
    }

    for root in roots {
        let fs_dir = root.join(".agent/fs");
        let msgq_dir = root.join(".agent/msgq");
        let sessions_dir = root.join(".agent/sessions");

        let removed_fs = if fs_dir.exists() { clear_dir(&fs_dir)? } else { 0 };
        let removed_msgq = if msgq_dir.exists() { clear_dir(&msgq_dir)? } else { 0 };
        let removed_sessions = if sessions_dir.exists() { clear_dir(&sessions_dir)? } else { 0 };

        println!(
            "[HOST] clean {}: .agent/fs={removed_fs}, .agent/msgq={removed_msgq}, .agent/sessions={removed_sessions}",
            root.display(),
        );
    }

    Ok(())
}

fn parse_assistant_message(assistant_msg_json: &str) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_str(assistant_msg_json)
        .unwrap_or_else(|_| serde_json::json!({"role":"assistant","content":""}));
    if value.get("role").is_none() {
        value["role"] = serde_json::Value::String("assistant".to_string());
    }
    if value.get("timestamp").is_none() {
        value["timestamp"] = serde_json::Value::String(now_stamp());
    }
    if value.get("finish_reason").is_none() {
        let finish_reason = if value
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
        {
            "tool_calls"
        } else {
            "stop"
        };
        value["finish_reason"] = serde_json::Value::String(finish_reason.to_string());
    }
    value
}

fn normalize_stepwise_status(raw: &str) -> String {
    match raw.trim().to_ascii_uppercase().as_str() {
        "RUNNING" => "RUNNING".to_string(),
        "SUCCESS" => "SUCCESS".to_string(),
        "IDLE" => "IDLE".to_string(),
        "FAIL" => "FAIL".to_string(),
        _ => "RUNNING".to_string(),
    }
}

fn message_verbatim(msg: &serde_json::Value) -> serde_json::Value {
    msg.get("verbatim")
        .cloned()
        .unwrap_or_else(|| msg.clone())
}

fn normalize_session_messages(messages: &[serde_json::Value]) -> Vec<SessionMessage> {
    messages
        .iter()
        .map(|msg| {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("assistant")
                .to_string();
            let verbatim = message_verbatim(msg);
            let meta = msg
                .get("meta")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({"visible": true, "sent": true}));
            SessionMessage { role, verbatim, meta }
        })
        .collect()
}

fn message_text_content(msg: &serde_json::Value) -> String {
    let verbatim = message_verbatim(msg);
    verbatim["content"].as_str().unwrap_or("").to_string()
}

fn message_visible(msg: &serde_json::Value) -> bool {
    msg.get("meta")
        .and_then(|m| m.get("visible"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

fn message_sent(msg: &serde_json::Value) -> bool {
    msg.get("meta")
        .and_then(|m| m.get("sent"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

fn assistant_call_sent(msg: &serde_json::Value, call_id: &str) -> bool {
    msg.get("meta")
        .and_then(|m| m.get("calls"))
        .and_then(|c| c.get(call_id))
        .and_then(|c| c.get("sent"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

fn assistant_call_status(msg: &serde_json::Value, call_id: &str) -> String {
    msg.get("meta")
        .and_then(|m| m.get("calls"))
        .and_then(|c| c.get(call_id))
        .and_then(|c| c.get("approval"))
        .and_then(|a| a.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("approved")
        .to_string()
}

fn assistant_call_modified_args(msg: &serde_json::Value, call_id: &str) -> Option<serde_json::Value> {
    msg.get("meta")
        .and_then(|m| m.get("calls"))
        .and_then(|c| c.get(call_id))
        .and_then(|c| c.get("approval"))
        .and_then(|a| a.get("modified_args"))
        .cloned()
        .filter(|v| !v.is_null())
}

fn tool_result_status(msg: &serde_json::Value) -> String {
    msg.get("meta")
        .and_then(|m| m.get("approval"))
        .and_then(|a| a.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("approved")
        .to_string()
}

fn tool_result_modified_content(msg: &serde_json::Value) -> Option<String> {
    msg.get("meta")
        .and_then(|m| m.get("approval"))
        .and_then(|a| a.get("modified_content"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn session_seed_from_messages(messages: &[serde_json::Value]) -> (String, Vec<HistoryEntry>, Vec<String>, u32, Vec<PendingToolCall>, bool) {
    let mut prompt = String::new();
    let mut history: Vec<HistoryEntry> = Vec::new();
    let mut validation_feedback: Vec<String> = Vec::new();
    let mut pending_assistant: HashMap<String, (String, String)> = HashMap::new();
    let mut approved_calls: HashMap<String, PendingToolCall> = HashMap::new();
    let mut seen_tool_results: HashSet<String> = HashSet::new();
    let mut pending_human_approvals = false;

    for msg in messages {
        let role = msg["role"].as_str().unwrap_or("");
        match role {
            "user" => {
                if !message_visible(msg) {
                    continue;
                }
                let content = message_text_content(msg);
                if content.is_empty() {
                    continue;
                }
                if prompt.is_empty() {
                    prompt = content;
                } else {
                    validation_feedback.push(content);
                }
            }
            "assistant" => {
                if !message_visible(msg) {
                    continue;
                }
                let mut payload = message_verbatim(msg);
                if payload.get("role").is_none() {
                    payload["role"] = serde_json::Value::String("assistant".to_string());
                }
                if let Some(calls) = payload["tool_calls"].as_array() {
                    let assistant_msg_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
                    for tc in calls {
                        let tool_call_id = tc["id"].as_str().unwrap_or("").to_string();
                        if tool_call_id.is_empty() {
                            continue;
                        }
                        let tool_name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                        let status = assistant_call_status(msg, &tool_call_id);
                        if status == "rejected" {
                            validation_feedback.push(format!(
                                "policy rejection: user rejected tool call id={} name={} before execution.",
                                tool_call_id, tool_name
                            ));
                            continue;
                        }
                        if status == "pending" {
                            pending_human_approvals = true;
                        }
                        if !assistant_call_sent(msg, &tool_call_id) {
                            continue;
                        }

                        pending_assistant.insert(tool_call_id.clone(), (tool_name.clone(), assistant_msg_json.clone()));

                        let base_args = tc["function"]["arguments"]
                            .as_str()
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                            .unwrap_or_else(|| serde_json::json!({}));
                        let args = if status == "modified" {
                            assistant_call_modified_args(msg, &tool_call_id).unwrap_or(base_args)
                        } else {
                            base_args
                        };
                        approved_calls.insert(
                            tool_call_id.clone(),
                            PendingToolCall {
                                tool: tool_name,
                                tool_call_id,
                                args,
                                assistant_msg_json: assistant_msg_json.clone(),
                            },
                        );
                    }
                }
            }
            "tool" => {
                let payload = message_verbatim(msg);
                let maybe_id = payload["tool_call_id"].as_str().map(str::to_string).unwrap_or_default();
                let status = tool_result_status(msg);
                if status == "rejected" {
                    if !maybe_id.is_empty() {
                        seen_tool_results.insert(maybe_id.clone());
                    }
                    validation_feedback.push(
                        "policy error: the tool was executed successfully, but the user censored the response from this tool because it may have contained sensitive information.".to_string()
                    );
                    continue;
                }

                if !message_visible(msg) || !message_sent(msg) {
                    if status == "pending" {
                        pending_human_approvals = true;
                    }
                    if let Some(id) = payload["tool_call_id"].as_str() {
                        if !id.is_empty() {
                            seen_tool_results.insert(id.to_string());
                        }
                    }
                    continue;
                }
                let tool_call_id = payload["tool_call_id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_default();
                if tool_call_id.is_empty() {
                    continue;
                }
                seen_tool_results.insert(tool_call_id.clone());
                let (fallback_name, fallback_assistant_json) = pending_assistant
                    .remove(&tool_call_id)
                    .unwrap_or_else(|| (String::new(), "{\"role\":\"assistant\",\"content\":\"\"}".to_string()));
                let tool_name = payload["name"].as_str().unwrap_or(&fallback_name).to_string();
                let assistant_msg_json = fallback_assistant_json;

                let result_json = if status == "modified" {
                    tool_result_modified_content(msg)
                        .map(|content| serde_json::json!({"result": content}).to_string())
                        .unwrap_or_else(|| payload["result_json"].as_str().map(str::to_string).unwrap_or_else(|| {
                            let content = payload["content"].as_str().unwrap_or("");
                            serde_json::json!({"result": content}).to_string()
                        }))
                } else {
                    payload["result_json"].as_str().map(str::to_string).unwrap_or_else(|| {
                        let content = payload["content"].as_str().unwrap_or("");
                        serde_json::json!({"result": content}).to_string()
                    })
                };

                history.push(HistoryEntry {
                    tool_call_id,
                    tool_name,
                    assistant_msg_json,
                    result_json,
                });
            }
            _ => {}
        }
    }

    let step = history.len() as u32;
    let pending_tool_calls = approved_calls
        .into_iter()
        .filter_map(|(id, call)| {
            if seen_tool_results.contains(&id) {
                None
            } else {
                Some(call)
            }
        })
        .collect::<Vec<_>>();
    let blocked_on_approval = pending_human_approvals && pending_tool_calls.is_empty();
    if prompt.is_empty() {
        for msg in messages.iter().rev() {
            if msg["role"].as_str().unwrap_or("") != "user" {
                continue;
            }
            if !message_visible(msg) {
                continue;
            }
            let content = message_text_content(msg);
            if !content.is_empty() {
                prompt = content;
                break;
            }
        }
    }
    (prompt, history, validation_feedback, step, pending_tool_calls, blocked_on_approval)
}

fn load_session_seed(path: &Path) -> Result<SessionSnapshotIn> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read session file: {}", path.display()))?;
    let snapshot: SessionSnapshotIn = serde_yaml::from_str(&raw)
        .with_context(|| format!("failed to parse session YAML: {}", path.display()))?;
    Ok(snapshot)
}

fn build_session_messages(req: Option<&GuestRequest>, decision: Option<&LlmDecision>, auto_rules: &[AutoApproveRule]) -> Vec<SessionMessage> {
    let mut messages: Vec<SessionMessage> = Vec::new();
    let mut unsent_tool_result_ids: HashSet<String> = HashSet::new();
    if let Some(req) = req {
        for id in &req.unsent_tool_result_ids {
            unsent_tool_result_ids.insert(id.clone());
        }
    }
    if let Some(req) = req {
        if !req.prompt.is_empty() {
            messages.push(SessionMessage {
                role: "user".to_string(),
                verbatim: serde_json::json!({
                    "content": req.prompt,
                    "timestamp": now_stamp(),
                }),
                meta: serde_json::json!({
                    "visible": true,
                    "sent": true,
                }),
            });
        }

        let mut emitted_assistant_batches: HashSet<String> = HashSet::new();
        for entry in &req.history {
            if !emitted_assistant_batches.contains(&entry.assistant_msg_json) {
                emitted_assistant_batches.insert(entry.assistant_msg_json.clone());
                let assistant_payload = parse_assistant_message(&entry.assistant_msg_json);
                let executed_call_ids: HashSet<String> = req
                    .history
                    .iter()
                    .filter(|h| h.assistant_msg_json == entry.assistant_msg_json)
                    .map(|h| h.tool_call_id.clone())
                    .collect();
                let mut calls_meta = serde_json::Map::new();
                if let Some(calls) = assistant_payload["tool_calls"].as_array() {
                    for tc in calls {
                        let id = tc["id"].as_str().unwrap_or("");
                        if id.is_empty() {
                            continue;
                        }
                        let executed = executed_call_ids.contains(id);
                        calls_meta.insert(
                            id.to_string(),
                            serde_json::json!({
                                "sent": executed,
                                "approval": {
                                    "status": if executed { "approved" } else { "pending" },
                                    "reviewed_at": null,
                                    "reason": null,
                                    "modified_args": null,
                                }
                            }),
                        );
                    }
                }
                messages.push(SessionMessage {
                    role: "assistant".to_string(),
                    verbatim: assistant_payload,
                    meta: serde_json::json!({
                        "kind": "tool_call_batch",
                        "visible": true,
                        "calls": calls_meta,
                    }),
                });
            }
            messages.push(SessionMessage {
                role: "tool".to_string(),
                verbatim: serde_json::json!({
                    "tool_call_id": entry.tool_call_id,
                    "name": entry.tool_name,
                    "content": format_tool_result(&entry.result_json),
                    "result_json": entry.result_json,
                    "timestamp": now_stamp(),
                }),
                meta: {
                    let force_pending = unsent_tool_result_ids.contains(&entry.tool_call_id);
                    let auto_result = should_auto_approve_result(auto_rules, &entry.tool_name);
                    let sent = if force_pending { auto_result } else { true };
                    let status = if force_pending {
                        if auto_result { "approved" } else { "pending" }
                    } else {
                        "approved"
                    };
                    serde_json::json!({
                    "kind": "tool_result",
                    "visible": true,
                    "sent": sent,
                    "approval": {
                        "status": status,
                        "reviewed_at": null,
                        "reason": null,
                        "modified_content": null,
                    }
                    })
                },
            });
        }

        for feedback in &req.validation_feedback {
            if feedback.is_empty() {
                continue;
            }
            messages.push(SessionMessage {
                role: "user".to_string(),
                verbatim: serde_json::json!({
                    "content": feedback,
                    "timestamp": now_stamp(),
                }),
                meta: serde_json::json!({
                    "visible": true,
                    "sent": true,
                }),
            });
        }
    }

    if let Some(decision) = decision {
        match decision {
            LlmDecision::ToolCall {
                tool_calls,
                assistant_msg_json,
                perf,
            } => {
                let assistant_payload = parse_assistant_message(assistant_msg_json);
                let mut calls_meta = serde_json::Map::new();
                for call in tool_calls {
                    let approved = should_auto_approve_call(auto_rules, call);
                    calls_meta.insert(
                        call.tool_call_id.clone(),
                        serde_json::json!({
                            "sent": approved,
                            "approval": {
                                "status": if approved { "approved" } else { "pending" },
                                "reviewed_at": null,
                                "reason": null,
                                "modified_args": null,
                            }
                        }),
                    );
                }
                let mut assistant_meta = serde_json::json!({
                    "kind": "tool_call_batch",
                    "visible": true,
                    "calls": calls_meta,
                });
                if let Some(perf_val) = perf {
                    assistant_meta["perf"] = perf_val.clone();
                }
                messages.push(SessionMessage {
                    role: "assistant".to_string(),
                    verbatim: assistant_payload,
                    meta: assistant_meta,
                });
            }
            LlmDecision::Final { answer, perf, .. } => {
                let mut assistant_meta = serde_json::json!({
                    "visible": true,
                    "sent": true,
                });
                if let Some(perf_val) = perf {
                    assistant_meta["perf"] = perf_val.clone();
                }
                messages.push(SessionMessage {
                    role: "assistant".to_string(),
                    verbatim: serde_json::json!({
                        "content": answer,
                        "tool_calls": [],
                        "finish_reason": "stop",
                        "timestamp": now_stamp(),
                    }),
                    meta: assistant_meta,
                });
            }
            LlmDecision::Error { message, perf } => {
                let mut assistant_meta = serde_json::json!({
                    "visible": true,
                    "sent": true,
                });
                if let Some(perf_val) = perf {
                    assistant_meta["perf"] = perf_val.clone();
                }
                messages.push(SessionMessage {
                    role: "assistant".to_string(),
                    verbatim: serde_json::json!({
                        "content": format!("ERROR: {message}"),
                        "tool_calls": [],
                        "finish_reason": "error",
                        "timestamp": now_stamp(),
                    }),
                    meta: assistant_meta,
                });
            }
        }
    }

    messages
}

fn write_session_snapshot(
    state: &mut HostState,
    session_state: &str,
    action: &str,
    req: Option<&GuestRequest>,
    decision: Option<&LlmDecision>,
) -> Result<()> {
    let from = state.session_state.clone();
    let messages = if !state.seed_messages.is_empty() {
        let mut merged = state.seed_messages.clone();
        if req.is_some() {
            for msg in &mut merged {
                if msg.role != "user" {
                    continue;
                }
                let content = msg
                    .verbatim
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if content.is_empty() {
                    continue;
                }
                let visible = msg
                    .meta
                    .get("visible")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let sent = msg
                    .meta
                    .get("sent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                if visible && !sent {
                    if let Some(meta_obj) = msg.meta.as_object_mut() {
                        meta_obj.insert("sent".to_string(), serde_json::json!(true));
                    } else {
                        msg.meta = serde_json::json!({"visible": true, "sent": true});
                    }
                }
            }
        }
        if let Some(dec) = decision {
            merged.extend(build_session_messages(None, Some(dec), &state.auto_approve_rules));
        }
        state.seed_messages = merged.clone();
        merged
    } else {
        let built = build_session_messages(req, decision, &state.auto_approve_rules);
        state.seed_messages = built.clone();
        built
    };
    let snapshot = SessionSnapshot {
        api_version: "daemon/v1".to_string(),
        kind: "Agent".to_string(),
        metadata: SessionMetadata {
            id: state.session_id.clone(),
            name: state.template_name.clone(),
            model: state.model.clone(),
            status: session_state.to_string(),
            created: state.session_created.clone(),
            last_pid: std::process::id(),
            tools: state.enabled_tools.clone(),
            max_steps: state.max_steps,
            labels: state.template_labels.clone(),
            description: state.template_description.clone(),
            last_transition: Some(SessionTransition {
                action: action.to_string(),
                from,
                to: session_state.to_string(),
                timestamp: now_stamp(),
            }),
        },
        spec: SessionSpec {
            system_prompt: state.system_prompt.clone(),
            messages,
        },
    };
    let yaml = serde_yaml::to_string(&snapshot).context("failed to serialize session snapshot")?;
    fs::write(session_snapshot_path(state), yaml).context("failed to write session snapshot")?;
    state.session_state = session_state.to_string();
    Ok(())
}

fn msgq_dirs(workspace_root: &Path) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf), String> {
    let root = workspace_root.join(".agent/msgq");
    let pending = root.join("pending");
    let assigned = root.join("assigned");
    let archive = root.join("archive");
    let teams = root.join("teams");
    fs::create_dir_all(&pending).map_err(|e| e.to_string())?;
    fs::create_dir_all(&assigned).map_err(|e| e.to_string())?;
    fs::create_dir_all(&archive).map_err(|e| e.to_string())?;
    fs::create_dir_all(&teams).map_err(|e| e.to_string())?;
    Ok((pending, assigned, archive, teams))
}

fn parse_message(content: &str) -> Result<(MsgEnvelope, String), String> {
    if !content.starts_with("---\n") {
        return Err("message missing YAML frontmatter".to_string());
    }
    let rest = &content[4..];
    let marker = rest.find("\n---\n").ok_or_else(|| "message frontmatter terminator not found".to_string())?;
    let front = &rest[..marker];
    let body = &rest[marker + 5..];
    let env: MsgEnvelope = serde_yaml::from_str(front).map_err(|e| format!("invalid frontmatter: {e}"))?;
    Ok((env, body.to_string()))
}

fn render_message(env: &MsgEnvelope, body: &str) -> Result<String, String> {
    let yaml = serde_yaml::to_string(env).map_err(|e| e.to_string())?;
    Ok(format!("---\n{}---\n\n{}", yaml, body))
}

fn load_msg(path: &Path) -> Result<(MsgEnvelope, String), String> {
    let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
    parse_message(&raw)
}

fn save_msg(path: &Path, env: &MsgEnvelope, body: &str) -> Result<(), String> {
    let raw = render_message(env, body)?;
    fs::write(path, raw).map_err(|e| e.to_string())
}

fn msg_id(prefix: &str) -> String {
    let ts = now_millis();
    let seq = MSG_ID_SEQ.fetch_add(1, Ordering::Relaxed);
    let seed = format!("{}-{}-{}-{}", prefix, ts, std::process::id(), seq);
    let mut h = Sha1::new();
    h.update(seed.as_bytes());
    let digest = h.finalize();
    let suffix = format!("{:02x}{:02x}{:02x}", digest[0], digest[1], digest[2]);
    format!("{prefix}-{ts}-{seq:04x}-{suffix}")
}

fn priority_rank(priority: &str) -> i32 {
    match priority {
        "high" => 3,
        "normal" => 2,
        "low" => 1,
        _ => 0,
    }
}

fn is_unblocked(msg: &MsgEnvelope, archive_dir: &Path) -> bool {
    msg.blocked_by
        .iter()
        .all(|id| archive_dir.join(format!("{id}.md")).exists())
}

fn msgq_append(workspace_root: &Path, args: &serde_json::Value) -> Result<String, String> {
    let (pending, _, _, _) = msgq_dirs(workspace_root)?;
    let id = args["id"].as_str().map(|s| s.to_string()).unwrap_or_else(|| msg_id("msg"));
    let path = pending.join(format!("{id}.md"));
    if path.exists() {
        return Err(format!("message id already exists: {id}"));
    }
    let env = MsgEnvelope {
        id: id.clone(),
        msg_type: args["type"].as_str().unwrap_or("note").to_string(),
        sender: args["sender"].as_str().unwrap_or("agent:unknown").to_string(),
        recipient: args["recipient"].as_str().unwrap_or("broadcast").to_string(),
        priority: args["priority"].as_str().unwrap_or("normal").to_string(),
        status: "pending".to_string(),
        assignee: None,
        blocked_by: args["blockedBy"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        payload: args.get("payload").cloned().unwrap_or_else(|| serde_json::json!({})),
        history: Vec::new(),
        created_at: now_stamp(),
    };
    let body = args["body"].as_str().unwrap_or("");
    save_msg(&path, &env, body)?;
    Ok(serde_json::json!({"id": id, "state": "pending"}).to_string())
}

fn msgq_list_summaries(
    workspace_root: &Path,
    state: &str,
    recipient: Option<&str>,
    assignee: Option<&str>,
    msg_type: Option<&str>,
    limit: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let (pending, assigned, archive, _) = msgq_dirs(workspace_root)?;
    let dir = match state {
        "pending" => pending,
        "assigned" => assigned,
        "archive" => archive,
        _ => return Err(format!("invalid state: {state}")),
    };
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = match entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let (env, _) = match load_msg(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if recipient.map(|r| env.recipient != r).unwrap_or(false) {
            continue;
        }
        if assignee.map(|a| env.assignee.as_deref() != Some(a)).unwrap_or(false) {
            continue;
        }
        if msg_type.map(|t| env.msg_type != t).unwrap_or(false) {
            continue;
        }
        out.push(serde_json::json!({
            "id": env.id,
            "type": env.msg_type,
            "sender": env.sender,
            "recipient": env.recipient,
            "priority": env.priority,
            "status": env.status,
            "assignee": env.assignee,
            "created_at": env.created_at,
        }));
    }
    out.sort_by(|a, b| a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or("")));
    if out.len() > limit {
        out.truncate(limit);
    }
    Ok(out)
}

fn msgq_list(workspace_root: &Path, args: &serde_json::Value) -> Result<String, String> {
    let state = args["state"].as_str().unwrap_or("pending");
    let recipient = args["recipient"].as_str();
    let assignee = args["assignee"].as_str();
    let msg_type = args["type"].as_str();
    let limit = args["limit"].as_u64().unwrap_or(100) as usize;
    let items = msgq_list_summaries(workspace_root, state, recipient, assignee, msg_type, limit)?;
    Ok(serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string()))
}

fn msgq_claim(workspace_root: &Path, args: &serde_json::Value) -> Result<String, String> {
    let (pending, assigned, archive, _) = msgq_dirs(workspace_root)?;
    let assignee = args["assignee"].as_str().unwrap_or("agent:unknown").to_string();
    let recipient_filter = args["recipient"].as_str();
    let type_filter = args["type"].as_str();

    let mut candidates: Vec<(PathBuf, MsgEnvelope, String)> = Vec::new();
    if let Some(id) = args["id"].as_str() {
        let path = pending.join(format!("{id}.md"));
        if !path.exists() {
            return Err(format!("message not found in pending: {id}"));
        }
        let (env, body) = load_msg(&path)?;
        candidates.push((path, env, body));
    } else {
        for entry in fs::read_dir(&pending).map_err(|e| e.to_string())? {
            let entry = match entry {
                Ok(v) => v,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let (env, body) = match load_msg(&path) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if recipient_filter.map(|r| env.recipient != r).unwrap_or(false) {
                continue;
            }
            if type_filter.map(|t| env.msg_type != t).unwrap_or(false) {
                continue;
            }
            if !is_unblocked(&env, &archive) {
                continue;
            }
            candidates.push((path, env, body));
        }
        candidates.sort_by(|a, b| {
            priority_rank(&b.1.priority)
                .cmp(&priority_rank(&a.1.priority))
                .then(a.1.created_at.cmp(&b.1.created_at))
        });
    }

    for (from_path, mut env, body) in candidates {
        if !is_unblocked(&env, &archive) {
            continue;
        }
        let to_path = assigned.join(format!("{}.md", env.id));
        if fs::rename(&from_path, &to_path).is_err() {
            continue;
        }
        env.status = "assigned".to_string();
        env.assignee = Some(assignee.clone());
        env.history.push(serde_json::json!({
            "event": "claimed",
            "timestamp": now_stamp(),
            "assignee": assignee,
        }));
        save_msg(&to_path, &env, &body)?;
        return Ok(serde_json::json!({"id": env.id, "state": "assigned", "assignee": env.assignee}).to_string());
    }

    Err("no eligible pending message found".to_string())
}

fn msgq_await(workspace_root: &Path, args: &serde_json::Value) -> Result<String, String> {
    let state = args["state"].as_str().unwrap_or("pending");
    let recipient = args["recipient"].as_str();
    let assignee = args["assignee"].as_str();
    let msg_type = args["type"].as_str();
    let limit = args["limit"].as_u64().unwrap_or(100) as usize;
    let min_count = args["min_count"].as_u64().map(|v| v as usize);
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(0);
    let poll_ms = args["poll_ms"].as_u64().unwrap_or(500);

    let start = Instant::now();
    let mut prev = String::new();
    loop {
        let items = msgq_list_summaries(workspace_root, state, recipient, assignee, msg_type, limit)?;
        let fingerprint = serde_json::to_string(&items).unwrap_or_default();
        if let Some(min) = min_count {
            if items.len() >= min {
                return Ok(serde_json::json!({"reason":"min_count_reached","items":items}).to_string());
            }
        } else if !items.is_empty() && prev.is_empty() {
            return Ok(serde_json::json!({"reason":"items_available","items":items}).to_string());
        } else if !prev.is_empty() && prev != fingerprint {
            return Ok(serde_json::json!({"reason":"queue_changed","items":items}).to_string());
        }
        prev = fingerprint;

        if timeout_ms > 0 && start.elapsed() >= Duration::from_millis(timeout_ms) {
            let items = msgq_list_summaries(workspace_root, state, recipient, assignee, msg_type, limit)?;
            return Ok(serde_json::json!({"reason":"timeout","items":items}).to_string());
        }
        std::thread::sleep(Duration::from_millis(poll_ms.max(10)));
    }
}

fn msgq_update(workspace_root: &Path, args: &serde_json::Value) -> Result<String, String> {
    let (_, assigned, _, _) = msgq_dirs(workspace_root)?;
    let id = args["id"].as_str().ok_or_else(|| "missing id".to_string())?;
    let path = assigned.join(format!("{id}.md"));
    if !path.exists() {
        return Err(format!("assigned message not found: {id}"));
    }
    let (mut env, mut body) = load_msg(&path)?;
    if let Some(a) = args["assignee"].as_str() {
        if env.assignee.as_deref() != Some(a) {
            return Err("assignee mismatch".to_string());
        }
    }
    if let Some(status) = args["status"].as_str() {
        if status != "assigned" && status != "in_progress" {
            return Err("status must be assigned or in_progress".to_string());
        }
        env.status = status.to_string();
    }
    if args.get("payload").is_some() {
        env.payload = args["payload"].clone();
    }
    if let Some(extra) = args["body_append"].as_str() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(extra);
    }
    env.history.push(serde_json::json!({
        "event": args["history_event"].as_str().unwrap_or("updated"),
        "timestamp": now_stamp(),
    }));
    save_msg(&path, &env, &body)?;
    Ok(serde_json::json!({"id": id, "status": env.status}).to_string())
}

fn msgq_archive(state: &mut HostState, args: &serde_json::Value) -> Result<String, String> {
    let (pending, assigned, archive, _) = msgq_dirs(&state.workspace_root)?;
    let id = args["id"].as_str().ok_or_else(|| "missing id".to_string())?;
    let from_state = args["from_state"].as_str().map(|s| s.to_string()).unwrap_or_else(|| {
        if assigned.join(format!("{id}.md")).exists() {
            "assigned".to_string()
        } else {
            "pending".to_string()
        }
    });
    let from_path = match from_state.as_str() {
        "assigned" => assigned.join(format!("{id}.md")),
        "pending" => pending.join(format!("{id}.md")),
        _ => return Err("from_state must be assigned or pending".to_string()),
    };
    if !from_path.exists() {
        return Err(format!("message not found: {id}"));
    }
    let (mut env, body) = load_msg(&from_path)?;
    if from_state == "assigned" {
        if let Some(a) = args["assignee"].as_str() {
            if env.assignee.as_deref() != Some(a) {
                return Err("assignee mismatch".to_string());
            }
        }
    }

    let resolution = args["resolution"].as_str().unwrap_or("completed");
    if resolution == "completed" {
        let payload = serde_json::json!({
            "task_id": env.id,
            "task_type": env.msg_type,
            "result_summary": env.payload["summary"].as_str().unwrap_or(""),
            "files_changed": env.payload["files_changed"].clone(),
        });
        if let Some(reason) = run_hooks(state, "task_completed", &payload, true).map_err(|e| e.to_string())? {
            return Err(format!("task_completed blocked: {reason}"));
        }
    }

    if args.get("final_payload").is_some() {
        env.payload = args["final_payload"].clone();
    }
    env.status = "archive".to_string();
    env.history.push(serde_json::json!({
        "event": "archived",
        "resolution": resolution,
        "timestamp": now_stamp(),
    }));

    let to_path = archive.join(format!("{id}.md"));
    fs::rename(&from_path, &to_path).map_err(|e| e.to_string())?;
    save_msg(&to_path, &env, &body)?;
    Ok(serde_json::json!({"id": id, "state": "archive", "resolution": resolution}).to_string())
}

fn msgq_bcast(workspace_root: &Path, args: &serde_json::Value) -> Result<String, String> {
    let recipients = args["recipients"].as_array().ok_or_else(|| "missing recipients".to_string())?;
    let mut ids = Vec::new();
    for recipient in recipients {
        let recipient = match recipient.as_str() {
            Some(v) => v,
            None => continue,
        };
        let mut append_args = serde_json::json!({
            "type": args["type"].as_str().unwrap_or("note"),
            "sender": args["sender"].as_str().unwrap_or("agent:unknown"),
            "recipient": recipient,
            "priority": args["priority"].as_str().unwrap_or("normal"),
            "payload": args["payload"].clone(),
            "body": args["body"].as_str().unwrap_or(""),
        });
        if append_args["payload"].is_null() {
            append_args["payload"] = serde_json::json!({});
        }
        let created = msgq_append(workspace_root, &append_args)?;
        let parsed: serde_json::Value = serde_json::from_str(&created).unwrap_or_default();
        if let Some(id) = parsed["id"].as_str() {
            ids.push(id.to_string());
        }
    }
    Ok(serde_json::json!({"count": ids.len(), "ids": ids}).to_string())
}

fn team_create(workspace_root: &Path, args: &serde_json::Value) -> Result<String, String> {
    let (_, _, _, teams) = msgq_dirs(workspace_root)?;
    let workers = args["workers"].as_array().ok_or_else(|| "missing workers".to_string())?;
    let team_id = args["team_id"].as_str().map(|s| s.to_string()).unwrap_or_else(|| msg_id("team"));
    let mut members = Vec::new();
    let mut launched = 0usize;
    let mut failed = 0usize;
    let exe = env::current_exe().map_err(|e| e.to_string())?;

    for (index, worker) in workers.iter().enumerate() {
        let output_path = worker["output"].as_str().map(|s| s.to_string());
        let mut cmd = Command::new(&exe);
        if let Some(raw_args) = worker["args"].as_array() {
            for arg in raw_args {
                if let Some(s) = arg.as_str() {
                    cmd.arg(s);
                }
            }
        } else {
            let template = worker["template"].as_str().ok_or_else(|| "worker missing template".to_string())?;
            let prompt = worker["prompt"].as_str().ok_or_else(|| "worker missing prompt".to_string())?;
            cmd.arg("-t").arg(template).arg(prompt);
        }
        cmd.current_dir(workspace_root);
        if let Some(path) = &output_path {
            let abs = workspace_root.join(path);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(abs)
                .map_err(|e| e.to_string())?;
            let file2 = file.try_clone().map_err(|e| e.to_string())?;
            cmd.stdout(Stdio::from(file));
            cmd.stderr(Stdio::from(file2));
        } else {
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
        }

        let launched_at = now_stamp();
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                launched += 1;
                members.push(TeamMember {
                    index,
                    session_id: worker["session_id"].as_str().map(|s| s.to_string()).unwrap_or_else(|| format!("{}-{}-{:04x}", now_millis(), pid, (pid ^ (now_millis() as u32)) & 0xffff)),
                    pid: Some(pid),
                    template: worker["template"].as_str().map(str::to_string),
                    output: output_path,
                    status: "launched".to_string(),
                    launched_at,
                });
            }
            Err(_) => {
                failed += 1;
                members.push(TeamMember {
                    index,
                    session_id: worker["session_id"].as_str().unwrap_or("").to_string(),
                    pid: None,
                    template: worker["template"].as_str().map(str::to_string),
                    output: output_path,
                    status: "failed_fast".to_string(),
                    launched_at,
                });
            }
        }
    }

    let team_file = TeamFile {
        team_id: team_id.clone(),
        status: "active".to_string(),
        created_at: now_stamp(),
        members: members.clone(),
    };
    let path = teams.join(format!("{team_id}.yml"));
    let yaml = serde_yaml::to_string(&team_file).map_err(|e| e.to_string())?;
    fs::write(&path, yaml).map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "team_id": team_id,
        "status": "active",
        "path": path.strip_prefix(workspace_root).unwrap_or(&path).display().to_string(),
        "launched_count": launched,
        "failed_count": failed,
        "members": members,
    }).to_string())
}

fn team_destroy(workspace_root: &Path, args: &serde_json::Value) -> Result<String, String> {
    let (_, _, _, teams) = msgq_dirs(workspace_root)?;
    let team_id = args["team_id"].as_str().ok_or_else(|| "missing team_id".to_string())?;
    let path = teams.join(format!("{team_id}.yml"));
    if !path.exists() {
        return Err(format!("team not found: {team_id}"));
    }
    let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut team: TeamFile = serde_yaml::from_str(&raw).map_err(|e| e.to_string())?;
    let signal_name = args["signal"].as_str().unwrap_or("SIGQUIT");
    let force_after_ms = args["force_after_ms"].as_u64().unwrap_or(1500);
    let signum = match signal_name {
        "SIGTERM" => libc::SIGTERM,
        "SIGKILL" => libc::SIGKILL,
        "SIGINT" => libc::SIGINT,
        "SIGHUP" => libc::SIGHUP,
        "SIGQUIT" => libc::SIGQUIT,
        _ => return Err(format!("invalid signal: {signal_name}")),
    };

    let mut results = Vec::new();
    for member in &team.members {
        let status = if let Some(pid) = member.pid {
            let rc = unsafe { libc::kill(pid as libc::pid_t, signum) };
            if rc != 0 {
                "not_found".to_string()
            } else {
                let start = Instant::now();
                while start.elapsed() < Duration::from_millis(force_after_ms) {
                    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
                    if !alive {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
                if alive {
                    let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                    "signal_sent_still_alive".to_string()
                } else {
                    "stopped".to_string()
                }
            }
        } else {
            "missing_pid".to_string()
        };
        results.push(serde_json::json!({
            "index": member.index,
            "session_id": member.session_id,
            "pid": member.pid,
            "status": status,
        }));
    }

    team.status = "destroyed".to_string();
    if args["remove_file"].as_bool().unwrap_or(false) {
        let _ = fs::remove_file(&path);
    } else {
        let yaml = serde_yaml::to_string(&team).map_err(|e| e.to_string())?;
        fs::write(&path, yaml).map_err(|e| e.to_string())?;
    }

    Ok(serde_json::json!({"team_id": team_id, "members": results}).to_string())
}

fn all_tool_names() -> Vec<String> {
    [
        "js_exec",
        "fs__file__view",
        "fs__file__create",
        "fs__file__edit",
        "fs__directory__list",
        "msgq__append",
        "msgq__claim",
        "msgq__list",
        "msgq__await",
        "msgq__update",
        "msgq__archive",
        "msgq__bcast",
        "team__create",
        "team__destroy",
    ]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn build_tools_json(enabled: &[String]) -> String {
    let all: &[(&str, &str, &str)] = &[
        (
            "js_exec",
            "Execute server-side JavaScript in a secured environment. \
            To run a shell command synchronously (blocking), use `console.log(require('shell').runSync('echo', ['hello']))`, which blocks until \
            the process exits and returns a message like \"Program PID {pid} exited with code {exit_code} \
            and its output ({line_count} line(s), {byte_count} byte(s)) was written to file: {file}\". \
            To run a shell command asynchronously (non-blocking), use `console.log(require('shell').run('echo', ['hello']))`, which returns \
            immediately with \"Program PID {pid} launched and output is streaming to file: {file}\". \
            You can also read files with `console.log(fs.readFileSync(\"/somefile.txt\"))` and write files with \
            `fs.writeFileSync(\"/somefile.txt\", \"content\")`",
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
        (
            "msgq__append",
            "Create a new pending message in .agent/msgq/pending.",
            r#"{"type":"object","properties":{"id":{"type":"string"},"type":{"type":"string"},"sender":{"type":"string"},"recipient":{"type":"string"},"priority":{"type":"string"},"blockedBy":{"type":"array","items":{"type":"string"}},"payload":{"type":"object"},"body":{"type":"string"}}}"#,
        ),
        (
            "msgq__claim",
            "Claim one pending message and move it to assigned.",
            r#"{"type":"object","properties":{"id":{"type":"string"},"assignee":{"type":"string"},"recipient":{"type":"string"},"type":{"type":"string"}}}"#,
        ),
        (
            "msgq__list",
            "List msgq messages with optional state and field filters.",
            r#"{"type":"object","properties":{"state":{"type":"string"},"recipient":{"type":"string"},"assignee":{"type":"string"},"type":{"type":"string"},"limit":{"type":"number"}}}"#,
        ),
        (
            "msgq__await",
            "Block until a filtered queue view changes or min_count is reached.",
            r#"{"type":"object","properties":{"state":{"type":"string"},"recipient":{"type":"string"},"assignee":{"type":"string"},"type":{"type":"string"},"limit":{"type":"number"},"min_count":{"type":"number"},"timeout_ms":{"type":"number"},"poll_ms":{"type":"number"}}}"#,
        ),
        (
            "msgq__update",
            "Update an assigned message and append a history event.",
            r#"{"type":"object","properties":{"id":{"type":"string"},"assignee":{"type":"string"},"status":{"type":"string"},"payload":{"type":"object"},"body_append":{"type":"string"},"history_event":{"type":"string"}},"required":["id"]}"#,
        ),
        (
            "msgq__archive",
            "Move a message to archive with a resolution status.",
            r#"{"type":"object","properties":{"id":{"type":"string"},"from_state":{"type":"string"},"assignee":{"type":"string"},"resolution":{"type":"string"},"final_payload":{"type":"object"}},"required":["id"]}"#,
        ),
        (
            "msgq__bcast",
            "Fan out one payload to multiple recipients as pending messages.",
            r#"{"type":"object","properties":{"recipients":{"type":"array","items":{"type":"string"}},"sender":{"type":"string"},"type":{"type":"string"},"priority":{"type":"string"},"payload":{"type":"object"},"body":{"type":"string"}},"required":["recipients"]}"#,
        ),
        (
            "team__create",
            "Launch worker wasm1 processes asynchronously and persist team metadata.",
            r#"{"type":"object","properties":{"team_id":{"type":"string"},"workers":{"type":"array","items":{"type":"object"}}},"required":["workers"]}"#,
        ),
        (
            "team__destroy",
            "Send stop signals to all worker processes in a team.",
            r#"{"type":"object","properties":{"team_id":{"type":"string"},"signal":{"type":"string"},"force_after_ms":{"type":"number"},"remove_file":{"type":"boolean"}},"required":["team_id"]}"#,
        ),
    ];

    let entries: Vec<String> = all
        .iter()
        .filter(|(name, _, _)| enabled.contains(&name.to_string()))
        .map(|(name, desc, params)| {
            let desc_json = serde_json::to_string(desc).unwrap_or_else(|_| "\"\"" .into());
            format!(
                r#"{{"type":"function","function":{{"name":"{name}","description":{desc_json},"parameters":{params}}}}}"#,
                name = name,
                desc_json = desc_json,
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

fn auto_approve_match_text(call: &ToolInvocation) -> String {
    if call.tool == "shell__execute" {
        return call.args["command"].as_str().unwrap_or("").to_string();
    }
    if call.tool == "js_exec" {
        return call.args["code"].as_str().unwrap_or("").to_string();
    }
    call.args.to_string()
}

fn build_llm_messages_from_seed(state: &HostState) -> Vec<serde_json::Value> {
    let mut messages: Vec<serde_json::Value> = vec![serde_json::json!({
        "role": "system",
        "content": state.system_prompt.as_deref().unwrap_or(""),
    })];

    for msg in &state.seed_messages {
        let visible = msg
            .meta
            .get("visible")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if !visible {
            continue;
        }
        match msg.role.as_str() {
            "user" => {
                let content = msg
                    .verbatim
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !content.is_empty() {
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": content,
                    }));
                }
            }
            "assistant" => {
                let mut assistant = msg.verbatim.clone();
                if assistant.get("role").is_none() {
                    assistant["role"] = serde_json::json!("assistant");
                }
                messages.push(assistant);
            }
            "tool" => {
                let content = msg
                    .verbatim
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut tool_msg = serde_json::json!({
                    "role": "tool",
                    "content": content,
                });
                if let Some(id) = msg.verbatim.get("tool_call_id").and_then(|v| v.as_str()) {
                    tool_msg["tool_call_id"] = serde_json::json!(id);
                }
                if let Some(name) = msg.verbatim.get("name").and_then(|v| v.as_str()) {
                    tool_msg["name"] = serde_json::json!(name);
                }
                messages.push(tool_msg);
            }
            _ => {}
        }
    }

    messages
}

fn should_auto_approve_call(rules: &[AutoApproveRule], call: &ToolInvocation) -> bool {
    let text = auto_approve_match_text(call);
    rules.iter().any(|r| {
        if r.tool != call.tool {
            return false;
        }
        match &r.pattern {
            Some(pat) => pat.is_match(&text),
            None => true,
        }
    })
}

fn should_auto_approve_result(rules: &[AutoApproveRule], tool_name: &str) -> bool {
    rules.iter().any(|r| r.tool == tool_name)
}

/// Normalize model name for GitHub Copilot internal API.
/// The internal API (api.githubcopilot.com) accepts model names directly
/// without publisher prefixes (e.g., "gpt-4o", "claude-sonnet-4").
fn normalize_copilot_model_id(model_name: &str) -> String {
    // Strip any publisher prefix if present
    if let Some(idx) = model_name.find('/') {
        return model_name[idx + 1..].to_string();
    }
    model_name.to_string()
}

fn usage_thought_tokens(usage: &serde_json::Value) -> u64 {
    usage["thought_tokens"].as_u64()
        .or_else(|| usage["reasoning_tokens"].as_u64())
        .or_else(|| usage["completion_tokens_details"]["reasoning_tokens"].as_u64())
        .or_else(|| usage["output_tokens_details"]["reasoning_tokens"].as_u64())
        .unwrap_or(0)
}

fn assistant_content_text(message: &serde_json::Value) -> String {
    if let Some(s) = message["content"].as_str() {
        return s.trim().to_string();
    }
    if let Some(parts) = message["content"].as_array() {
        let mut out = String::new();
        for part in parts {
            if let Some(s) = part["text"].as_str() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(s);
                continue;
            }
            if let Some(s) = part["content"].as_str() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(s);
            }
        }
        return out.trim().to_string();
    }
    String::new()
}

fn llm_decide(state: &HostState, req: &GuestRequest) -> Result<LlmDecision> {
    // Build provider-compatible tool definitions
    let tools_json_str = build_tools_json(&state.enabled_tools);
    let tools_value: serde_json::Value = serde_json::from_str(&tools_json_str)
        .unwrap_or(serde_json::json!([]));

    // For resumed sessions, preserve exact YAML ordering and role alternation.
    // For brand-new sessions, keep legacy seed behavior.
    let messages: Vec<serde_json::Value> = if !state.seed_messages.is_empty() {
        build_llm_messages_from_seed(state)
    } else {
        let system = state.system_prompt.as_deref().unwrap_or("");
        let initial_user = &req.prompt;
        let mut m: Vec<serde_json::Value> = vec![
            serde_json::json!({"role": "system", "content": system}),
            serde_json::json!({"role": "user", "content": initial_user}),
        ];
        for entry in &req.history {
            let assistant: serde_json::Value = serde_json::from_str(&entry.assistant_msg_json)
                .unwrap_or_else(|_| serde_json::json!({"role": "assistant", "content": ""}));
            m.push(assistant);
            let summary = format_tool_result(&entry.result_json);
            m.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": entry.tool_call_id,
                "name": entry.tool_name,
                "content": summary,
            }));
        }
        for feedback in &req.validation_feedback {
            m.push(serde_json::json!({
                "role": "user",
                "content": feedback,
            }));
        }
        m
    };

    let request_model = if state.provider == ModelProvider::Copilot {
        normalize_copilot_model_id(&state.model_name)
    } else {
        state.model_name.clone()
    };

    let mut body = serde_json::json!({
        "model": request_model,
        "temperature": 0.1,
        "messages": messages,
    });
    if tools_value.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
        body["tools"] = tools_value;
        body["tool_choice"] = serde_json::json!("auto");
    }

    let (chat_url, provider_label) = match state.provider {
        ModelProvider::Xai => (
            format!("{}/v1/chat/completions", state.provider_api_url.trim_end_matches('/')),
            "xAI",
        ),
        ModelProvider::Copilot => (
            format!("{}/chat/completions", state.provider_api_url.trim_end_matches('/')),
            "Copilot",
        ),
    };

    let mut last_timeout_err: Option<reqwest::Error> = None;
    let mut resp_opt = None;
    let req_started = Instant::now();
    for attempt in 1..=3 {
        let mut req_builder = state
            .client
            .post(&chat_url)
            .bearer_auth(&state.api_key)
            .json(&body);
        if state.provider == ModelProvider::Copilot {
            // VS Code-style Copilot headers
            req_builder = req_builder
                .header("Editor-Version", COPILOT_EDITOR_VERSION)
                .header("Editor-Plugin-Version", COPILOT_EDITOR_PLUGIN_VERSION)
                .header("User-Agent", COPILOT_USER_AGENT)
                .header("Copilot-Integration-Id", "vscode-chat")
                .header("OpenAI-Intent", "conversation-panel");
        }
        match req_builder.send() {
            Ok(resp) => {
                resp_opt = Some(resp);
                break;
            }
            Err(err) if err.is_timeout() && attempt < 3 => {
                let backoff_ms = 500_u64 * (1_u64 << (attempt - 1));
                eprintln!(
                    "[HOST] {provider_label} request timed out (attempt {attempt}/3); retrying in {backoff_ms}ms"
                );
                std::thread::sleep(Duration::from_millis(backoff_ms));
                last_timeout_err = Some(err);
            }
            Err(err) if err.is_timeout() => {
                last_timeout_err = Some(err);
                break;
            }
            Err(err) => {
                return Err(anyhow!(err).context(format!("request to {provider_label} failed")));
            }
        }
    }
    let resp = match resp_opt {
        Some(v) => v,
        None => {
            let err = last_timeout_err
                .map(anyhow::Error::new)
                .unwrap_or_else(|| anyhow!("request timed out"));
            return Err(err.context(format!("request to {provider_label} failed after 3 timeout retries")));
        }
    };

    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("(unknown)")
        .to_string();
    let body_text = resp
        .text()
        .with_context(|| format!("failed to read {provider_label} response body"))?;
    if !status.is_success() {
        return Err(anyhow!(
            "{provider_label} API error {status} (content-type: {content_type}): {body}",
            body = body_text.chars().take(600).collect::<String>()
        ));
    }
    let payload: serde_json::Value = serde_json::from_str(&body_text).with_context(|| {
        format!(
            "failed to parse {provider_label} response JSON (content-type: {content_type}, body-prefix: {prefix})",
            prefix = body_text.chars().take(220).collect::<String>().replace('\n', "\\n")
        )
    })?;

    let perf_meta = {
        let u = &payload["usage"];
        let prompt_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
        let completion_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
        let thought_tokens = usage_thought_tokens(u);
        let total_tokens = u["total_tokens"].as_u64().unwrap_or(prompt_tokens + completion_tokens);
        let elapsed_s = req_started.elapsed().as_secs_f64();
        let output_tokens = completion_tokens + thought_tokens;
        let tok_per_sec = output_tokens as f64 / elapsed_s.max(0.001);
        let context_window = state.context_window;
        let context_pct = context_window.map(|window| total_tokens * 100 / window.max(1));
        serde_json::json!({
            "duration_s": elapsed_s,
            "step": req.step,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "thought_tokens": thought_tokens,
            "output_tokens": output_tokens,
            "total_tokens": total_tokens,
            "tok_per_sec": tok_per_sec,
            "context_window": context_window,
            "context_pct": context_pct,
        })
    };

    let message = &payload["choices"][0]["message"];

    // Native tool calling: provider may return one or multiple tool calls.
    if let Some(tool_calls) = message["tool_calls"].as_array() {
        if !tool_calls.is_empty() {
            let mut invocations: Vec<ToolInvocation> = Vec::new();
            for tc in tool_calls {
                let tool_call_id = tc["id"].as_str().unwrap_or("").to_string();
                if tool_call_id.is_empty() {
                    continue;
                }
                let tool_name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                if tool_name.is_empty() {
                    continue;
                }
                let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                let args: serde_json::Value = serde_json::from_str(args_str)
                    .unwrap_or_else(|_| serde_json::json!({}));
                invocations.push(ToolInvocation {
                    tool: tool_name,
                    tool_call_id,
                    args,
                });
            }

            if !invocations.is_empty() {
                // Serialize the full assistant message for replay in the next turn.
                let assistant_msg_json = serde_json::to_string(message)
                    .unwrap_or_else(|_| "{}".to_string());
                return Ok(LlmDecision::ToolCall {
                    tool_calls: invocations,
                    assistant_msg_json,
                    perf: Some(perf_meta),
                });
            }
        }
    }

    // No tool_calls → model is done; return its text as the final answer
    let content = assistant_content_text(message);
    Ok(LlmDecision::Final {
        answer: content,
        thought: None,
        perf: Some(perf_meta),
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
