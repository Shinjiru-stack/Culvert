//! AI tool registry.
//!
//! Every tool wraps a capability Culvert already has: SSH exec channels
//! (`ssh` layer, same as the terminal monitor strip), SFTP operations
//! (`russh-sftp`, same as the file browser), app state reads (from the UI
//! context snapshot) and UI actions (executed by `app.rs` through its
//! existing paths — open session, tunnel start/stop, dock navigation).
//!
//! `prepare` validates a model's tool call, resolves names against the
//! context and classifies the risk; the engine drives approval, and
//! [`Executor`] runs the remote work.

use crate::ai::security::{classify_command, sensitive_path, Risk};
use crate::ai::{CtxTunnel, UiAction, UiContext};
use crate::log::SharedLogs;
use crate::session::{AuthType, SshSession};
use crate::ssh;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

fn s(params: Value) -> Value {
    params
}

/// The full registry. Descriptions double as the model's manual — keep them
/// action-oriented and honest about defaults.
pub fn defs() -> Vec<crate::ai::provider::ToolDef> {
    use crate::ai::provider::ToolDef;
    vec![
        ToolDef {
            name: "get_active_session",
            description: "Return the session the user is currently looking at (name, user@host, group).",
            parameters: s(json!({ "type": "object", "properties": {} })),
        },
        ToolDef {
            name: "get_available_profiles",
            description: "List every saved SSH profile the user can connect to.",
            parameters: s(json!({ "type": "object", "properties": {} })),
        },
        ToolDef {
            name: "list_connections",
            description: "List saved profiles together with which ones have an open shell window right now.",
            parameters: s(json!({ "type": "object", "properties": {} })),
        },
        ToolDef {
            name: "get_current_directory",
            description: "Return the directory the docked file browser is showing (plus the session it belongs to).",
            parameters: s(json!({ "type": "object", "properties": {} })),
        },
        ToolDef {
            name: "get_selected_file",
            description: "Return the file(s) currently selected in the docked file browser, if any.",
            parameters: s(json!({ "type": "object", "properties": {} })),
        },
        ToolDef {
            name: "list_tunnels",
            description: "List every configured tunnel (local/remote/dynamic) with its running state.",
            parameters: s(json!({ "type": "object", "properties": {} })),
        },
        ToolDef {
            name: "connect_server",
            description: "Open a shell window to a saved profile (like double-clicking it in the sidebar).",
            parameters: s(json!({
                "type": "object",
                "properties": { "profile": { "type": "string", "description": "Profile name (see get_available_profiles)." } },
                "required": ["profile"]
            })),
        },
        ToolDef {
            name: "disconnect_server",
            description: "Close the open shell window for a profile.",
            parameters: s(json!({
                "type": "object",
                "properties": { "profile": { "type": "string", "description": "Profile name." } },
                "required": ["profile"]
            })),
        },
        ToolDef {
            name: "start_tunnel",
            description: "Start a configured tunnel (from list_tunnels).",
            parameters: s(json!({
                "type": "object",
                "properties": { "tunnel": { "type": "string", "description": "Tunnel key or a description that uniquely identifies it (host:port, session name)." } },
                "required": ["tunnel"]
            })),
        },
        ToolDef {
            name: "stop_tunnel",
            description: "Stop a running tunnel (from list_tunnels).",
            parameters: s(json!({
                "type": "object",
                "properties": { "tunnel": { "type": "string", "description": "Tunnel key or a description that uniquely identifies it." } },
                "required": ["tunnel"]
            })),
        },
        ToolDef {
            name: "execute_command",
            description: "Run a shell command on a server over SSH and return its output. Read-only commands run immediately; state-changing and destructive commands need the user's approval first. `sudo` works: when the profile has a saved password Culvert feeds it to `sudo -S` (passwordless sudo also works) — do not avoid sudo for that reason. Avoid interactive or streaming commands (top, tail -f, watch, less); use bounded forms (`cmd | tail -n 200`).",
            parameters: s(json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "description": "Profile name or host. Omit to use the active session." },
                    "command": { "type": "string", "description": "The shell command line." }
                },
                "required": ["command"]
            })),
        },
        ToolDef {
            name: "get_system_info",
            description: "One-shot server health probe: user, hostname, CPU load, memory, root-disk usage and uptime.",
            parameters: s(json!({
                "type": "object",
                "properties": { "server": { "type": "string", "description": "Profile name or host. Omit for the active session." } }
            })),
        },
        ToolDef {
            name: "get_disk_usage",
            description: "Run df -h for a path (defaults to /) to see filesystem usage.",
            parameters: s(json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "description": "Profile name or host." },
                    "path": { "type": "string", "description": "Server path to inspect, e.g. /var/log." }
                }
            })),
        },
        ToolDef {
            name: "search_files",
            description: "Find files under a directory by name, size or age (built on `find`). Examples: larger_than_mb for big files, newer_than_days + name for fresh logs.",
            parameters: s(json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "description": "Profile name or host." },
                    "path": { "type": "string", "description": "Directory to search, e.g. /var/log." },
                    "name": { "type": "string", "description": "Glob pattern, e.g. '*.log' or 'nginx*'. Case-insensitive." },
                    "larger_than_mb": { "type": "number", "description": "Only files larger than this many MB." },
                    "newer_than_days": { "type": "number", "description": "Only files modified within this many days." },
                    "max_results": { "type": "number", "description": "Cap (default 200)." }
                },
                "required": ["path"]
            })),
        },
        ToolDef {
            name: "list_directory",
            description: "List a remote directory over SFTP (folders first; sizes included).",
            parameters: s(json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "description": "Profile name or host." },
                    "path": { "type": "string", "description": "Directory path." }
                },
                "required": ["path"]
            })),
        },
        ToolDef {
            name: "read_file",
            description: "Read a remote text file (up to 256 KB). Sensitive files (keys, .env, kubeconfig, …) go through the security gate before the model sees them.",
            parameters: s(json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "description": "Profile name or host." },
                    "path": { "type": "string", "description": "File path." }
                },
                "required": ["path"]
            })),
        },
        ToolDef {
            name: "download_file",
            description: "Download a remote file to this computer (defaults to the Downloads folder).",
            parameters: s(json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "description": "Profile name or host." },
                    "remote_path": { "type": "string", "description": "File path on the server." },
                    "local_path": { "type": "string", "description": "Optional destination path on this computer." }
                },
                "required": ["remote_path"]
            })),
        },
        ToolDef {
            name: "upload_file",
            description: "Upload a local file to the server.",
            parameters: s(json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "description": "Profile name or host." },
                    "local_path": { "type": "string", "description": "File path on this computer." },
                    "remote_path": { "type": "string", "description": "Destination path on the server." }
                },
                "required": ["local_path", "remote_path"]
            })),
        },
        ToolDef {
            name: "navigate_file_browser",
            description: "Point the docked file browser at a directory on the active session (the sidebar Files tool follows).",
            parameters: s(json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "description": "Profile name or host." },
                    "path": { "type": "string", "description": "Directory path." }
                },
                "required": ["path"]
            })),
        },
        ToolDef {
            name: "open_file_in_editor",
            description: "Open a remote file in the user's local editor (download → edit → auto re-upload on save).",
            parameters: s(json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string", "description": "Profile name or host." },
                    "path": { "type": "string", "description": "File path." }
                },
                "required": ["path"]
            })),
        },
    ]
}

// ---------------------------------------------------------------------------
// Prepared calls
// ---------------------------------------------------------------------------

/// A validated tool call: resolved targets, human summary, risk verdict and
/// the concrete work to run.
pub struct Call {
    pub tool: String,
    pub summary: String,
    pub risk: Risk,
    pub reason: String,
    /// Editable command preview (only execute_command and friends).
    pub command: Option<String>,
    /// Set when the *source* is sensitive regardless of content (read_file).
    pub sensitive: Option<String>,
    pub prepared: Prepared,
}

pub enum Prepared {
    State(StateQuery),
    Ssh {
        session_id: String,
        label: String,
        command: String,
    },
    ListDir {
        session_id: String,
        label: String,
        path: String,
    },
    ReadFile {
        session_id: String,
        label: String,
        path: String,
    },
    Download {
        session_id: String,
        label: String,
        remote: String,
        local: String,
    },
    Upload {
        session_id: String,
        label: String,
        local: String,
        remote: String,
    },
    Action {
        action: UiAction,
        label: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StateQuery {
    ActiveSession,
    Profiles,
    Connections,
    CurrentDirectory,
    SelectedFile,
    Tunnels,
}

// ---------------------------------------------------------------------------
// Preparation (validate + resolve + classify)
// ---------------------------------------------------------------------------

fn arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .unwrap_or("")
}

fn safe_shell(s: &str) -> bool {
    !s.chars()
        .any(|c| matches!(c, ';' | '&' | '|' | '$' | '`' | '"' | '\'' | '\n' | '\r' | '<' | '>'))
}

/// Resolve the target server: explicit name/host wins, otherwise the active
/// session, otherwise a single configured profile.
fn resolve_server(ctx: &UiContext, args: &Value) -> Result<(String, String), String> {
    let want = arg(args, "server");
    let pick = |id: &str| -> Option<(String, String)> {
        ctx.profiles
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.id.clone(), p.name.clone()))
    };
    if want.is_empty() || want == "active" || want == "current" {
        if let Some(id) = &ctx.active_session_id {
            if let Some(hit) = pick(id) {
                return Ok(hit);
            }
        }
        if ctx.profiles.len() == 1 {
            let p = &ctx.profiles[0];
            return Ok((p.id.clone(), p.name.clone()));
        }
        let names: Vec<&str> = ctx.profiles.iter().map(|p| p.name.as_str()).collect();
        return Err(format!(
            "no active session — name a server explicitly. Available: {}",
            names.join(", ")
        ));
    }
    let low = want.to_lowercase();
    if let Some(p) = ctx.profiles.iter().find(|p| {
        p.name.to_lowercase() == low
            || p.host.to_lowercase() == low
            || p.id == want
            || p.name.to_lowercase().starts_with(&low)
    }) {
        return Ok((p.id.clone(), p.name.clone()));
    }
    let names: Vec<&str> = ctx.profiles.iter().map(|p| p.name.as_str()).collect();
    Err(format!(
        "no saved profile matches '{want}'. Available: {}",
        names.join(", ")
    ))
}

fn resolve_tunnel<'a>(ctx: &'a UiContext, want: &str) -> Result<&'a CtxTunnel, String> {
    let w = want.trim().to_lowercase();
    if let Some(t) = ctx.tunnels.iter().find(|t| t.key == want) {
        return Ok(t);
    }
    if let Some(t) = ctx.tunnels.iter().find(|t| t.summary.to_lowercase() == w) {
        return Ok(t);
    }
    let hits: Vec<&CtxTunnel> = ctx
        .tunnels
        .iter()
        .filter(|t| {
            t.summary.to_lowercase().contains(&w) || t.session_label.to_lowercase().contains(&w)
        })
        .collect();
    match hits.len() {
        1 => Ok(hits[0]),
        0 => Err(format!(
            "no tunnel matches '{want}'. Configured: {}",
            if ctx.tunnels.is_empty() {
                "(none)".to_string()
            } else {
                ctx.tunnels
                    .iter()
                    .map(|t| t.summary.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ")
            }
        )),
        _ => Err(format!(
            "'{want}' matches {} tunnels — use an exact key (see list_tunnels)",
            hits.len()
        )),
    }
}

fn state_call(tool: &str, q: StateQuery, summary: &str) -> Call {
    Call {
        tool: tool.to_string(),
        summary: summary.to_string(),
        risk: Risk::Safe,
        reason: "reads app state".to_string(),
        command: None,
        sensitive: None,
        prepared: Prepared::State(q),
    }
}

/// Validate a model tool call against the live context.
pub fn prepare(tool: &str, args: &Value, ctx: &UiContext) -> Result<Call, String> {
    match tool {
        "get_active_session" => Ok(state_call(tool, StateQuery::ActiveSession, "read the active session")),
        "get_available_profiles" => Ok(state_call(tool, StateQuery::Profiles, "list saved profiles")),
        "list_connections" => Ok(state_call(tool, StateQuery::Connections, "list connections")),
        "get_current_directory" => Ok(state_call(tool, StateQuery::CurrentDirectory, "read the current directory")),
        "get_selected_file" => Ok(state_call(tool, StateQuery::SelectedFile, "read the selected file")),
        "list_tunnels" => Ok(state_call(tool, StateQuery::Tunnels, "list tunnels")),

        "connect_server" | "disconnect_server" => {
            let want = arg(args, "profile");
            if want.is_empty() {
                return Err("missing 'profile'".into());
            }
            let (id, name) = resolve_server(ctx, &json!({ "server": want }))?;
            let (action, verb) = if tool == "connect_server" {
                (UiAction::OpenSession { session_id: id }, "open a shell to")
            } else {
                (UiAction::CloseSession { session_id: id }, "close the shell for")
            };
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("{verb} {name}"),
                risk: Risk::Approval,
                reason: "changes open sessions".into(),
                command: None,
                sensitive: None,
                prepared: Prepared::Action {
                    action,
                    label: format!("{verb} {name}"),
                },
            })
        }

        "start_tunnel" | "stop_tunnel" => {
            let want = arg(args, "tunnel");
            if want.is_empty() {
                return Err("missing 'tunnel'".into());
            }
            let t = resolve_tunnel(ctx, want)?;
            let (action, verb) = if tool == "start_tunnel" {
                (
                    UiAction::StartTunnel {
                        key: t.key.clone(),
                    },
                    "start tunnel",
                )
            } else {
                (
                    UiAction::StopTunnel {
                        key: t.key.clone(),
                    },
                    "stop tunnel",
                )
            };
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("{verb}: {} (via {})", t.summary, t.session_label),
                risk: Risk::Approval,
                reason: "changes tunnel state".into(),
                command: None,
                sensitive: None,
                prepared: Prepared::Action {
                    action,
                    label: format!("{verb} {}", t.summary),
                },
            })
        }

        "execute_command" => {
            let command = arg(args, "command").to_string();
            if command.is_empty() {
                return Err("missing 'command'".into());
            }
            let (id, label) = resolve_server(ctx, args)?;
            let verdict = classify_command(&command);
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("run on {label}: {command}"),
                risk: verdict.risk,
                reason: verdict.reason,
                command: Some(command.clone()),
                sensitive: None,
                prepared: Prepared::Ssh {
                    session_id: id,
                    label,
                    command,
                },
            })
        }

        "get_system_info" => {
            let (id, label) = resolve_server(ctx, args)?;
            let script = "echo '== user'; whoami; echo '== host'; hostname; echo '== cpus'; nproc 2>/dev/null; echo '== load'; cat /proc/loadavg 2>/dev/null; echo '== memory'; free -h 2>/dev/null || head -3 /proc/meminfo; echo '== disk'; df -h /; echo '== uptime'; uptime";
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("read system health on {label}"),
                risk: Risk::Safe,
                reason: "fixed read-only probe".into(),
                command: Some(script.to_string()),
                sensitive: None,
                prepared: Prepared::Ssh {
                    session_id: id,
                    label,
                    command: script.to_string(),
                },
            })
        }

        "get_disk_usage" => {
            let (id, label) = resolve_server(ctx, args)?;
            let path = {
                let p = arg(args, "path");
                if p.is_empty() { "/".to_string() } else { p.to_string() }
            };
            if !safe_shell(&path) {
                return Err("path contains shell metacharacters".into());
            }
            let command = format!("df -h '{path}'");
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("check disk usage of {path} on {label}"),
                risk: Risk::Safe,
                reason: "read-only command".into(),
                command: Some(command.clone()),
                sensitive: None,
                prepared: Prepared::Ssh {
                    session_id: id,
                    label,
                    command,
                },
            })
        }

        "search_files" => {
            let (id, label) = resolve_server(ctx, args)?;
            let path = arg(args, "path").to_string();
            if path.is_empty() {
                return Err("missing 'path'".into());
            }
            if !safe_shell(&path) {
                return Err("path contains shell metacharacters".into());
            }
            let name = arg(args, "name").to_string();
            if !name.is_empty() && !safe_shell(&name) {
                return Err("name pattern contains shell metacharacters".into());
            }
            let mut cmd = format!("find '{path}' -type f");
            if !name.is_empty() {
                cmd.push_str(&format!(" -iname '{name}'"));
            }
            if let Some(mb) = args.get("larger_than_mb").and_then(|v| v.as_f64()) {
                if mb > 0.0 {
                    cmd.push_str(&format!(" -size +{}M", mb.ceil() as u64));
                }
            }
            if let Some(d) = args.get("newer_than_days").and_then(|v| v.as_f64()) {
                if d > 0.0 {
                    cmd.push_str(&format!(" -mtime -{}", d.ceil() as u64));
                }
            }
            let max = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(200)
                .clamp(1, 1000);
            let full = format!("{cmd} 2>/dev/null | head -n {max}");
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("search files on {label}: {cmd}"),
                risk: Risk::Safe,
                reason: "read-only search".into(),
                command: Some(cmd),
                sensitive: None,
                prepared: Prepared::Ssh {
                    session_id: id,
                    label,
                    command: full,
                },
            })
        }

        "list_directory" => {
            let (id, label) = resolve_server(ctx, args)?;
            let path = arg(args, "path").to_string();
            if path.is_empty() {
                return Err("missing 'path'".into());
            }
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("list {path} on {label}"),
                risk: Risk::Safe,
                reason: "reads a directory listing".into(),
                command: None,
                sensitive: None,
                prepared: Prepared::ListDir {
                    session_id: id,
                    label,
                    path,
                },
            })
        }

        "read_file" => {
            let (id, label) = resolve_server(ctx, args)?;
            let path = arg(args, "path").to_string();
            if path.is_empty() {
                return Err("missing 'path'".into());
            }
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("read {path} on {label}"),
                risk: Risk::Safe,
                reason: "reads a file (content is security-filtered)".into(),
                command: None,
                sensitive: sensitive_path(&path),
                prepared: Prepared::ReadFile {
                    session_id: id,
                    label,
                    path,
                },
            })
        }

        "download_file" => {
            let (id, label) = resolve_server(ctx, args)?;
            let remote = arg(args, "remote_path").to_string();
            if remote.is_empty() {
                return Err("missing 'remote_path'".into());
            }
            let local = {
                let given = arg(args, "local_path").to_string();
                if given.is_empty() {
                    let base = remote.trim_end_matches('/').rsplit('/').next().unwrap_or("download.bin");
                    let dir = dirs::download_dir()
                        .or_else(dirs::home_dir)
                        .unwrap_or_else(std::env::temp_dir);
                    dir.join(base).display().to_string()
                } else {
                    given
                }
            };
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("download {remote} from {label} to {local}"),
                risk: Risk::Approval,
                reason: "writes a file on this computer".into(),
                command: None,
                sensitive: None,
                prepared: Prepared::Download {
                    session_id: id,
                    label,
                    remote,
                    local,
                },
            })
        }

        "upload_file" => {
            let (id, label) = resolve_server(ctx, args)?;
            let local = arg(args, "local_path").to_string();
            let remote = arg(args, "remote_path").to_string();
            if local.is_empty() || remote.is_empty() {
                return Err("missing 'local_path' or 'remote_path'".into());
            }
            if !std::path::Path::new(&local).is_file() {
                return Err(format!("local file does not exist: {local}"));
            }
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("upload {local} to {remote} on {label}"),
                risk: Risk::Approval,
                reason: "writes to the server".into(),
                command: None,
                sensitive: None,
                prepared: Prepared::Upload {
                    session_id: id,
                    label,
                    local,
                    remote,
                },
            })
        }

        "navigate_file_browser" => {
            let (id, label) = resolve_server(ctx, args)?;
            let path = arg(args, "path").to_string();
            if path.is_empty() {
                return Err("missing 'path'".into());
            }
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("show {path} in the file browser ({label})"),
                risk: Risk::Safe,
                reason: "moves the file browser".into(),
                command: None,
                sensitive: None,
                prepared: Prepared::Action {
                    action: UiAction::DockNavigate {
                        session_id: id,
                        path: path.clone(),
                    },
                    label: format!("file browser → {path}"),
                },
            })
        }

        "open_file_in_editor" => {
            let (id, label) = resolve_server(ctx, args)?;
            let path = arg(args, "path").to_string();
            if path.is_empty() {
                return Err("missing 'path'".into());
            }
            Ok(Call {
                tool: tool.to_string(),
                summary: format!("open {path} ({label}) in the local editor"),
                risk: Risk::Safe,
                reason: "downloads a copy and opens the editor".into(),
                command: None,
                sensitive: None,
                prepared: Prepared::Action {
                    action: UiAction::OpenFile {
                        session_id: id,
                        path: path.clone(),
                    },
                    label: format!("open {path} in editor"),
                },
            })
        }

        other => Err(format!("unknown tool '{other}'")),
    }
}

// ---------------------------------------------------------------------------
// State answers (no remote work)
// ---------------------------------------------------------------------------

pub fn state_answer(q: StateQuery, ctx: &UiContext) -> String {
    match q {
        StateQuery::ActiveSession => match ctx
            .active_session_id
            .as_ref()
            .and_then(|id| ctx.profiles_summary.iter().find(|p| &p.id == id))
        {
            Some(p) => format!(
                "Active session: {} ({}@{}:{}){}",
                p.name,
                p.username,
                p.host,
                p.protocol,
                p.group
                    .as_ref()
                    .map(|g| format!(" [group {g}]"))
                    .unwrap_or_default()
            ),
            None => "No active session — figure out the target from the profile list.".to_string(),
        },
        StateQuery::Profiles => {
            if ctx.profiles_summary.is_empty() {
                return "No saved profiles.".to_string();
            }
            let mut out = String::from("Saved profiles:\n");
            for p in &ctx.profiles_summary {
                out.push_str(&format!(
                    "- {} ({}@{}){} [{}]\n",
                    p.name,
                    p.username,
                    p.host,
                    p.group
                        .as_ref()
                        .map(|g| format!(" group={g}"))
                        .unwrap_or_default(),
                    p.protocol
                ));
            }
            out
        }
        StateQuery::Connections => {
            let mut out = String::from("Profiles and window state:\n");
            for p in &ctx.profiles_summary {
                let open = ctx.open_session_ids.contains(&p.id);
                out.push_str(&format!(
                    "- {} ({}@{}) — {}\n",
                    p.name,
                    p.username,
                    p.host,
                    if open { "window open" } else { "no window" }
                ));
            }
            if ctx.profiles_summary.is_empty() {
                out.push_str("(none)\n");
            }
            out
        }
        StateQuery::CurrentDirectory => match &ctx.dock {
            Some(d) => {
                let session = ctx
                    .profiles_summary
                    .iter()
                    .find(|p| p.id == d.session_id)
                    .map(|p| p.name.as_str())
                    .unwrap_or("?");
                format!("File browser shows {session}: {}", d.cwd)
            }
            None => "The file browser is not showing a server right now.".to_string(),
        },
        StateQuery::SelectedFile => match ctx.dock.as_ref().and_then(|d| d.selected.as_ref()) {
            Some(f) => format!("Selected in the file browser: {f}"),
            None => "Nothing is selected in the file browser.".to_string(),
        },
        StateQuery::Tunnels => {
            if ctx.tunnels.is_empty() {
                return "No tunnels are configured.".to_string();
            }
            let mut out = String::from("Tunnels:\n");
            for t in &ctx.tunnels {
                out.push_str(&format!(
                    "- [{}] {} — via '{}' (key: {}{})\n",
                    if t.running { "running" } else { "stopped" },
                    t.summary,
                    t.session_label,
                    t.key,
                    if t.auto_reconnect { ", auto-reconnect" } else { "" }
                ));
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Executor (remote work over the reused ssh/sftp layers)
// ---------------------------------------------------------------------------

pub struct Executor {
    conns: HashMap<String, ssh::SshHandle>,
    sftps: HashMap<String, SftpSession>,
}

impl Executor {
    pub fn new() -> Self {
        Self {
            conns: HashMap::new(),
            sftps: HashMap::new(),
        }
    }

    /// Run a prepared call. UI actions and state queries never reach here.
    pub async fn run(
        &mut self,
        profile: &SshSession,
        prepared: &Prepared,
        logs: &SharedLogs,
    ) -> Result<String, String> {
        match prepared {
            Prepared::State(_) => Err("internal: state answered locally".into()),
            Prepared::Action { .. } => Err("internal: actions run on the UI thread".into()),
            Prepared::Ssh { command, .. } => self.command(profile, command, logs).await,
            Prepared::ListDir { path, .. } => self.list_dir(profile, path, logs).await,
            Prepared::ReadFile { path, .. } => self.read_file(profile, path, logs).await,
            Prepared::Download { remote, local, .. } => {
                self.download(profile, remote, local, logs).await
            }
            Prepared::Upload { local, remote, .. } => {
                self.upload(profile, local, remote, logs).await
            }
        }
    }

    async fn connect_handle(
        &self,
        profile: &SshSession,
        logs: &SharedLogs,
    ) -> Result<ssh::SshHandle, String> {
        let (ftx, _frx) = tokio::sync::mpsc::unbounded_channel();
        ssh::connect(profile, logs.clone(), ftx)
            .await
            .map_err(|e| format!("cannot connect to {}: {e}", profile.name))
    }

    async fn command(
        &mut self,
        profile: &SshSession,
        command: &str,
        logs: &SharedLogs,
    ) -> Result<String, String> {
        // PTY-less exec channels cannot answer sudo's password prompt;
        // rewrite + feed the saved password when there is one.
        let (cmd, sudo_pw) = prepare_exec(profile, command);
        let id = profile.id.clone();
        if !self.conns.contains_key(&id) {
            let h = self.connect_handle(profile, logs).await?;
            self.conns.insert(id.clone(), h);
        }
        let first = {
            let h = self.conns.get(&id).unwrap();
            run_exec(h, &cmd, sudo_pw.clone()).await
        };
        let out = match first {
            Ok(out) => Ok(out),
            Err(e) => {
                // Stale connection: reconnect once before giving up.
                self.conns.remove(&id);
                let h = self.connect_handle(profile, logs).await?;
                self.conns.insert(id.clone(), h);
                let h = self.conns.get(&id).unwrap();
                run_exec(h, &cmd, sudo_pw.clone())
                    .await
                    .map_err(|e2| format!("{e2} (after reconnect; first error: {e})"))
            }
        };
        out.map(|text| sudo_hint(&text, sudo_pw.is_some()))
    }

    async fn sftp(
        &mut self,
        profile: &SshSession,
        logs: &SharedLogs,
    ) -> Result<&SftpSession, String> {
        let id = profile.id.clone();
        if !self.sftps.contains_key(&id) {
            let handle = self.connect_handle(profile, logs).await?;
            let ch = handle
                .channel_open_session()
                .await
                .map_err(|e| format!("channel: {e}"))?;
            ch.request_subsystem(true, "sftp")
                .await
                .map_err(|e| format!("server has no SFTP subsystem: {e}"))?;
            let sftp = SftpSession::new(ch.into_stream())
                .await
                .map_err(|e| format!("SFTP init failed: {e}"))?;
            self.sftps.insert(id.clone(), sftp);
        }
        Ok(self.sftps.get(&id).unwrap())
    }

    async fn list_dir(
        &mut self,
        profile: &SshSession,
        path: &str,
        logs: &SharedLogs,
    ) -> Result<String, String> {
        let sftp = self.sftp(profile, logs).await?;
        let canon = sftp.canonicalize(path).await.unwrap_or_else(|_| path.to_string());
        let mut rows: Vec<(bool, String, u64)> = Vec::new();
        for entry in sftp
            .read_dir(&canon)
            .await
            .map_err(|e| format!("cannot list {path}: {e}"))?
        {
            let md = entry.metadata();
            rows.push((md.is_dir(), entry.file_name().to_string(), md.size.unwrap_or(0)));
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));
        let total = rows.len();
        let mut out = format!("{canon} — {total} entries\n");
        for (is_dir, name, size) in rows.iter().take(300) {
            if *is_dir {
                out.push_str(&format!("{name}/\n"));
            } else {
                out.push_str(&format!("{name}  {}\n", crate::filebrowser::human_size(*size)));
            }
        }
        if total > 300 {
            out.push_str(&format!("… {} more\n", total - 300));
        }
        Ok(out)
    }

    async fn read_file(
        &mut self,
        profile: &SshSession,
        path: &str,
        logs: &SharedLogs,
    ) -> Result<String, String> {
        const CAP: u64 = 256 * 1024;
        let sftp = self.sftp(profile, logs).await?;
        if let Ok(md) = sftp.metadata(path).await {
            if md.size.unwrap_or(0) > CAP {
                return Err(format!(
                    "file is {} — too large to read (cap 256 KB); use search_files or execute_command with grep/head",
                    crate::filebrowser::human_size(md.size.unwrap_or(0))
                ));
            }
        }
        let mut f = sftp
            .open(path)
            .await
            .map_err(|e| format!("cannot open {path}: {e}"))?;
        let mut buf: Vec<u8> = Vec::new();
        f.read_to_end(&mut buf)
            .await
            .map_err(|e| format!("read failed: {e}"))?;
        if buf.len() as u64 > CAP {
            buf.truncate(CAP as usize);
        }
        let text = String::from_utf8_lossy(&buf);
        Ok(format!("### {path} ({} bytes)\n\n{}", buf.len(), text))
    }

    async fn download(
        &mut self,
        profile: &SshSession,
        remote: &str,
        local: &str,
        logs: &SharedLogs,
    ) -> Result<String, String> {
        let sftp = self.sftp(profile, logs).await?;
        let mut rf = sftp
            .open(remote)
            .await
            .map_err(|e| format!("cannot open {remote}: {e}"))?;
        if let Some(dir) = std::path::Path::new(local).parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
        }
        let mut lf = tokio::fs::File::create(local)
            .await
            .map_err(|e| format!("cannot create {local}: {e}"))?;
        let n = tokio::io::copy(&mut rf, &mut lf)
            .await
            .map_err(|e| format!("copy failed: {e}"))?;
        Ok(format!(
            "Downloaded {remote} → {local} ({})",
            crate::filebrowser::human_size(n)
        ))
    }

    async fn upload(
        &mut self,
        profile: &SshSession,
        local: &str,
        remote: &str,
        logs: &SharedLogs,
    ) -> Result<String, String> {
        let sftp = self.sftp(profile, logs).await?;
        let mut lf = tokio::fs::File::open(local)
            .await
            .map_err(|e| format!("cannot open {local}: {e}"))?;
        let mut rf = sftp
            .open_with_flags(
                remote,
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await
            .map_err(|e| format!("cannot write {remote}: {e}"))?;
        let n = tokio::io::copy(&mut lf, &mut rf)
            .await
            .map_err(|e| format!("copy failed: {e}"))?;
        rf.flush().await.ok();
        Ok(format!(
            "Uploaded {local} → {remote} ({})",
            crate::filebrowser::human_size(n)
        ))
    }
}

/// `sudo` cannot prompt on a PTY-less exec channel ("a terminal is required
/// to read the password"). When the profile carries a saved password, the
/// first `sudo` invocation is rewritten to `sudo -S` and that password goes
/// to the channel's stdin — the same thing `ssh -t` plus typing does. The
/// password never appears on the command line, in the audit log or to the
/// model. Returns the (possibly rewritten) command plus the stdin data.
fn prepare_exec(profile: &SshSession, command: &str) -> (String, Option<String>) {
    if profile.auth_type != AuthType::Password || profile.password.is_empty() {
        return (command.to_string(), None);
    }
    let Some(at) = find_sudo(command) else {
        return (command.to_string(), None);
    };
    let rewritten = if command[at..].starts_with("sudo -S") {
        command.to_string()
    } else {
        let end = at + "sudo".len();
        format!("{} -S -p ''{}", &command[..end], &command[end..])
    };
    (rewritten, Some(format!("{}\n", profile.password)))
}

/// Byte offset of the first real `sudo` invocation: at the start of the
/// command or after a separator, on a word boundary, and *outside* quotes —
/// a mention inside a string is not an invocation, and rewriting inside
/// single quotes would break the quoting outright.
fn find_sudo(command: &str) -> Option<usize> {
    let bytes = command.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => {
                if b == b'\'' || b == b'"' {
                    quote = Some(b);
                } else if command[i..].starts_with("sudo") {
                    let before_ok = i == 0
                        || matches!(
                            bytes[i - 1],
                            b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'(' | b'`' | b'{'
                        );
                    let end = i + 4;
                    let after_ok = end >= bytes.len() || matches!(bytes[end], b' ' | b'\t');
                    if before_ok && after_ok {
                        return Some(i);
                    }
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }
    None
}

/// Next-step note appended to output when sudo could not authenticate, so
/// both the model and the user know what to do (the model otherwise retries
/// blind or gives up).
fn sudo_hint(out: &str, had_password: bool) -> String {
    if out.contains("a terminal is required to read the password")
        || out.contains("a password is required")
    {
        format!(
            "{out}\n[hint] sudo could not prompt for a password on this exec channel. Culvert feeds the saved session password to `sudo -S` automatically — save this profile with Password auth (Auth: Password), or configure passwordless sudo for this user."
        )
    } else if had_password
        && (out.contains("Sorry, try again") || out.contains("incorrect password attempt"))
    {
        format!(
            "{out}\n[hint] the saved SSH password was rejected by sudo on this host — it differs from the login password, or this user is not a sudoer."
        )
    } else if out.contains("is not in the sudoers file") {
        format!(
            "{out}\n[hint] this account has no sudo rights on the host at all (not a sudoer). Another admin account, or adding the user to sudoers, is required — installing software as this user is not possible."
        )
    } else {
        out.to_string()
    }
}

/// One exec channel, full output, 45 s cap (streaming commands never finish).
async fn run_exec(
    handle: &ssh::SshHandle,
    command: &str,
    stdin_data: Option<String>,
) -> Result<String, String> {
    let fut = async {
        let mut ch = handle
            .channel_open_session()
            .await
            .map_err(|e| format!("channel: {e}"))?;
        ch.exec(false, command)
            .await
            .map_err(|e| format!("exec: {e}"))?;
        if let Some(data) = stdin_data {
            // `sudo -S` reads the password here; EOF afterwards makes a
            // wrong password fail fast instead of hanging.
            ch.data_bytes(data.into_bytes())
                .await
                .map_err(|e| format!("stdin: {e}"))?;
            ch.eof().await.map_err(|e| format!("stdin close: {e}"))?;
        }
        let mut out: Vec<u8> = Vec::new();
        let mut exit: Option<u32> = None;
        while let Some(msg) = ch.wait().await {
            match msg {
                russh::ChannelMsg::Data { data } => out.extend_from_slice(&data),
                russh::ChannelMsg::ExtendedData { data, .. } => out.extend_from_slice(&data),
                russh::ChannelMsg::ExitStatus { exit_status } => exit = Some(exit_status),
                russh::ChannelMsg::Eof | russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        let _ = ch.close().await;
        Ok::<(Vec<u8>, Option<u32>), String>((out, exit))
    };
    match tokio::time::timeout(std::time::Duration::from_secs(45), fut).await {
        Err(_) => Err(
            "command timed out after 45 s — avoid streaming commands (tail -f, watch, top); use bounded forms like `| tail -n 200`"
                .to_string(),
        ),
        Ok(Err(e)) => Err(e),
        Ok(Ok((out, exit))) => {
            let mut text = String::from_utf8_lossy(&out).trim().to_string();
            if text.chars().count() > 15_000 {
                let cut: String = text.chars().take(15_000).collect();
                text = format!("{cut}\n… (output truncated by Culvert at 15000 chars)");
            }
            if text.is_empty() {
                text = "(no output)".to_string();
            }
            if let Some(code) = exit {
                if code != 0 {
                    text = format!("{text}\n(exit code {code})");
                }
            }
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn password_profile() -> SshSession {
        SshSession {
            auth_type: AuthType::Password,
            password: "hunter2".into(),
            ..SshSession::default()
        }
    }

    #[test]
    fn sudo_gets_stdin_flags_and_password() {
        let p = password_profile();
        let (cmd, pw) = prepare_exec(&p, "sudo apt install docker.io -y");
        assert_eq!(cmd, "sudo -S -p '' apt install docker.io -y");
        assert_eq!(pw.as_deref(), Some("hunter2\n"));

        let (cmd, pw) = prepare_exec(&p, "echo hi && sudo systemctl restart nginx");
        assert_eq!(cmd, "echo hi && sudo -S -p '' systemctl restart nginx");
        assert!(pw.is_some());

        // Already interactive-ready: keep the command, still feed stdin.
        let (cmd, pw) = prepare_exec(&p, "sudo -S whoami");
        assert_eq!(cmd, "sudo -S whoami");
        assert!(pw.is_some());
    }

    #[test]
    fn non_sudo_and_quoted_mentions_are_untouched() {
        let p = password_profile();
        let (cmd, pw) = prepare_exec(&p, "df -h && free -m");
        assert_eq!(cmd, "df -h && free -m");
        assert!(pw.is_none());

        // A quoted mention or a longer word is not an invocation.
        let (cmd, pw) = prepare_exec(&p, "echo 'install with sudo apt'");
        assert_eq!(cmd, "echo 'install with sudo apt'");
        assert!(pw.is_none());
        let (cmd, _) = prepare_exec(&p, "cat /etc/sudoers");
        assert_eq!(cmd, "cat /etc/sudoers");
    }

    #[test]
    fn key_auth_profiles_have_no_password_to_feed() {
        let p = SshSession {
            auth_type: AuthType::Key,
            key_path: "C:\\keys\\id_ed25519".into(),
            ..SshSession::default()
        };
        let (cmd, pw) = prepare_exec(&p, "sudo whoami");
        assert_eq!(cmd, "sudo whoami");
        assert!(pw.is_none());
    }

    #[test]
    fn sudo_failures_gain_actionable_hints() {
        let no_tty = "sudo: a terminal is required to read the password; either use ssh's -t option or configure an askpass helper";
        let hinted = sudo_hint(no_tty, false);
        assert!(hinted.contains("[hint]"));
        assert!(hinted.contains("Password auth"));

        let wrong = "Sorry, try again.\nsudo: 3 incorrect password attempts";
        let hinted = sudo_hint(wrong, true);
        assert!(hinted.contains("rejected by sudo"));

        let not_sudoer = "linuxio is not in the sudoers file.\nThis incident has been reported.";
        let hinted = sudo_hint(not_sudoer, true);
        assert!(hinted.contains("no sudo rights"));

        // Ordinary output passes through untouched.
        assert_eq!(sudo_hint("root\n", true), "root\n");
    }

    /// Live probe: the exact production path (prepare_exec + stdin feed)
    /// against the docker test server. Run with:
    /// `cargo test -- --ignored --nocapture sudo_exec`
    ///
    /// The demo container's `linuxio` is intentionally not a sudoer, so the
    /// assertion checks the mechanism (the password is fed — sudo no longer
    /// reports a missing terminal) rather than root access.
    #[test]
    #[ignore = "needs the local docker ssh test server on 127.0.0.1:2222"]
    fn sudo_exec_feeds_saved_password() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let session = SshSession {
                name: "probe".into(),
                host: "127.0.0.1".into(),
                port: 2222,
                username: "linuxio".into(),
                auth_type: AuthType::Password,
                password: "test123".into(),
                ..SshSession::default()
            };
            let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
            let mut ex = Executor::new();
            let out = ex
                .command(&session, "sudo whoami", &logs)
                .await
                .expect("exec failed");
            println!("sudo whoami -> {out:?}");
            assert!(
                !out.contains("a terminal is required") && !out.contains("a password is required"),
                "the saved password must reach sudo's stdin: {out}"
            );
            if out.contains("is not in the sudoers file") {
                println!("note: linuxio is not a sudoer on this container (expected)");
            } else {
                assert!(out.contains("root"), "sudo must authenticate: {out}");
            }
        });
    }
}
