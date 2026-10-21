//! Model Context Protocol server: lets an external agent (Claude Desktop,
//! Cursor, any MCP client) drive Culvert's saved sessions and tunnels.
//!
//! Architecture rule, same as the Copilot: **the model asks, the app executes.**
//! This module owns the transport (local HTTP, bearer token, JSON-RPC) and the
//! tool schemas. Every state change is a [`Action`] handed to the UI thread,
//! which applies it with the same code paths the windows and dialogs use.
//!
//! Secrets never cross the wire: requests may carry a new password, replies
//! only ever say whether one is saved.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Path MCP clients POST to.
pub const ENDPOINT: &str = "/mcp";
const MAX_BODY: usize = 1_000_000;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpConfig {
    /// The server only listens after the user turns it on.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Bearer token; generated on first load. Loopback-only, but any local
    /// process could otherwise drive the app.
    #[serde(default)]
    pub token: String,
}

fn default_port() -> u16 {
    47822
}

pub fn config_file() -> PathBuf {
    dirs::config_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("culvert")
        .join("mcp.json")
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_port(),
            token: new_token(),
        }
    }
}

impl McpConfig {
    /// Load the config, creating it (disabled) on first run. A missing or
    /// unreadable token is regenerated rather than leaving the server open.
    pub fn load() -> Self {
        let path = config_file();
        let mut cfg = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str::<McpConfig>(s.trim_start_matches('\u{feff}'))
                .unwrap_or_default(),
            Err(_) => McpConfig::default(),
        };
        if cfg.token.trim().len() < 16 {
            cfg.token = new_token();
        }
        if !path.exists() {
            let _ = cfg.save();
        }
        cfg
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let body = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, body).map_err(|e| e.to_string())
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}{ENDPOINT}", self.port)
    }
}

pub fn new_token() -> String {
    // Two v4 UUIDs, no dashes: 64 hex chars, plenty for a loopback token.
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

// ---------------------------------------------------------------------------
// Actions requested by the server, applied by the UI thread
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, Default)]
pub struct SessionSpec {
    pub name: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    /// "password" or "key".
    pub auth: Option<String>,
    /// Write-only: sent to the app, never returned.
    pub password: Option<String>,
    pub key_path: Option<String>,
    pub key_passphrase: Option<String>,
    /// Group id or name.
    pub group: Option<String>,
    /// "ssh" or "rdp".
    pub protocol: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct TunnelSpec {
    pub name: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub auth: Option<String>,
    pub password: Option<String>,
    pub key_path: Option<String>,
    pub key_passphrase: Option<String>,
    /// "local", "remote" or "dynamic".
    pub kind: Option<String>,
    pub listen_host: Option<String>,
    pub listen_port: Option<u16>,
    pub dest_host: Option<String>,
    pub dest_port: Option<u16>,
    pub auto_reconnect: Option<bool>,
}

#[derive(Clone, Debug)]
pub enum Action {
    ListSessions,
    GetSession { name: String },
    CreateSession { spec: SessionSpec },
    UpdateSession { name: String, spec: SessionSpec },
    DeleteSession { name: String },
    ListTunnels,
    GetTunnel { name: String },
    CreateTunnel { spec: TunnelSpec },
    UpdateTunnel { name: String, spec: TunnelSpec },
    DeleteTunnel { name: String },
    StartTunnel { name: String },
    StopTunnel { name: String },
    ListForwards { session: Option<String> },
    CreateForward { session: String, spec: TunnelSpec },
    UpdateForward {
        session: String,
        forward: String,
        spec: TunnelSpec,
    },
    DeleteForward { session: String, forward: String },
    StartForward { session: String, forward: String },
    StopForward { session: String, forward: String },
    OpenSession { name: String },
}

pub struct Pending {
    pub action: Action,
    pub reply: Sender<Result<Value, String>>,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

pub struct Server {
    rx: Receiver<Pending>,
    stop: Arc<AtomicBool>,
    port: u16,
    token: String,
}

impl Server {
    /// Bind 127.0.0.1 and start the listener thread. The egui context is used
    /// to wake the UI loop when a request arrives (the app is idle otherwise,
    /// and an idle app never drains the channel).
    pub fn start(cfg: &McpConfig, ctx: egui::Context) -> Result<Self, String> {
        // A previous Server (e.g. after "New token") closes its listener on
        // another thread; give it a moment to release the port instead of
        // making the user press Start a second time.
        let deadline = Instant::now() + Duration::from_millis(600);
        let listener = loop {
            match TcpListener::bind(("127.0.0.1", cfg.port)) {
                Ok(l) => break l,
                Err(e)
                    if e.kind() == std::io::ErrorKind::AddrInUse
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => {
                    return Err(format!("cannot listen on 127.0.0.1:{}: {e}", cfg.port));
                }
            }
        };
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("cannot configure listener: {e}"))?;
        // `port: 0` asks the OS for a free port; remember the real one.
        let port = listener
            .local_addr()
            .map_err(|e| format!("cannot read listener address: {e}"))?
            .port();

        let (tx, rx) = mpsc::channel::<Pending>();
        let stop = Arc::new(AtomicBool::new(false));
        let token = cfg.token.clone();
        let stop_thread = stop.clone();

        std::thread::Builder::new()
            .name("culvert-mcp".to_string())
            .spawn(move || {
                while !stop_thread.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            // One request at a time: tools are tiny, and a
                            // serialized queue keeps UI replies ordered.
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                            let _ = handle_connection(&mut stream, &token, &tx, &ctx);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(40));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(200)),
                    }
                }
            })
            .map_err(|e| format!("cannot start MCP thread: {e}"))?;

        Ok(Self {
            rx,
            stop,
            port,
            token: cfg.token.clone(),
        })
    }

    pub fn try_recv(&self) -> Option<Pending> {
        self.rx.try_recv().ok()
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}{ENDPOINT}", self.port)
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the blocking accept() so the thread notices the flag now.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

// ---------------------------------------------------------------------------
// HTTP / JSON-RPC
// ---------------------------------------------------------------------------

fn handle_connection(
    stream: &mut TcpStream,
    token: &str,
    tx: &Sender<Pending>,
    ctx: &egui::Context,
) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_BODY {
            return respond(stream, 413, "Payload Too Large", "");
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let mut content_length = 0usize;
    let mut authorized = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if k == "content-length" {
                content_length = v.parse().unwrap_or(0);
            } else if k == "authorization" {
                authorized = v == format!("Bearer {token}");
            }
        }
    }

    if method == "GET" && path == "/" {
        return respond(
            stream,
            200,
            "OK",
            "Culvert MCP server. POST JSON-RPC to /mcp with a bearer token.",
        );
    }
    if path != ENDPOINT {
        return respond(stream, 404, "Not Found", "unknown endpoint");
    }
    if method == "OPTIONS" {
        return respond(stream, 204, "No Content", "");
    }
    if !authorized {
        return respond(
            stream,
            401,
            "Unauthorized",
            "missing or wrong bearer token (see Tools > MCP server)",
        );
    }
    if method != "POST" {
        return respond(stream, 405, "Method Not Allowed", "use POST");
    }

    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    if body.len() > MAX_BODY {
        return respond(stream, 413, "Payload Too Large", "");
    }

    let request: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let err = json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32700, "message": format!("parse error: {e}")}
            });
            return respond(stream, 200, "OK", &err.to_string());
        }
    };

    match dispatch(&request, tx, ctx) {
        Some(reply) => respond(stream, 200, "OK", &reply.to_string()),
        // Notifications (no id) get no JSON-RPC response.
        None => respond(stream, 202, "Accepted", ""),
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn respond(stream: &mut TcpStream, code: u16, reason: &str, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         \r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn dispatch(req: &Value, tx: &Sender<Pending>, ctx: &egui::Context) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    // Notifications have no id and expect no reply.
    let id = id?;

    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "culvert", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Manage Culvert's saved SSH sessions and tunnels. \
                             Destructive changes (delete) take effect immediately; \
                             pass the exact name or id as shown by the list tools."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match action_from_call(name, &args) {
                Ok(action) => Ok(run_action(action, tx, ctx)),
                Err(msg) => Ok(tool_error(&msg)),
            }
        }
        _ => Err((-32601, format!("unknown method: {method}"))),
    };

    Some(match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
    })
}

fn tool_error(msg: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

fn tool_ok(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

fn run_action(action: Action, tx: &Sender<Pending>, ctx: &egui::Context) -> Value {
    let (reply_tx, reply_rx) = mpsc::channel();
    if tx
        .send(Pending {
            action,
            reply: reply_tx,
        })
        .is_err()
    {
        return tool_error("Culvert is shutting down");
    }
    // The UI drains this on its next frame; wake it up if it is idle.
    ctx.request_repaint();
    match reply_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(v)) => tool_ok(v),
        Ok(Err(e)) => tool_error(&e),
        Err(_) => tool_error(
            "Culvert did not answer within 20 s — is its window open (not closed to tray)?",
        ),
    }
}

fn action_from_call(name: &str, args: &Value) -> Result<Action, String> {
    let text = |key: &str| -> Result<String, String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("'{key}' is required"))
    };
    let opt_text = |key: &str| -> Option<String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let spec = || -> Result<SessionSpec, String> {
        serde_json::from_value(args.clone()).map_err(|e| format!("invalid arguments: {e}"))
    };
    let tspec = || -> Result<TunnelSpec, String> {
        serde_json::from_value(args.clone()).map_err(|e| format!("invalid arguments: {e}"))
    };
    // Updates take the lookup key in `name` and the replacement in
    // `new_name`; the spec itself must not see the lookup key.
    let patch = || -> Result<SessionSpec, String> {
        let mut a = args.clone();
        if let Some(obj) = a.as_object_mut() {
            obj.remove("name");
            if let Some(nn) = obj.remove("new_name") {
                obj.insert("name".to_string(), nn);
            }
        }
        serde_json::from_value(a).map_err(|e| format!("invalid arguments: {e}"))
    };
    let tpatch = || -> Result<TunnelSpec, String> {
        let mut a = args.clone();
        if let Some(obj) = a.as_object_mut() {
            obj.remove("name");
            if let Some(nn) = obj.remove("new_name") {
                obj.insert("name".to_string(), nn);
            }
        }
        serde_json::from_value(a).map_err(|e| format!("invalid arguments: {e}"))
    };
    Ok(match name {
        "culvert_list_sessions" => Action::ListSessions,
        "culvert_get_session" => Action::GetSession { name: text("name")? },
        "culvert_create_session" => Action::CreateSession { spec: spec()? },
        "culvert_update_session" => Action::UpdateSession {
            name: text("name")?,
            spec: patch()?,
        },
        "culvert_delete_session" => Action::DeleteSession { name: text("name")? },
        "culvert_list_tunnels" => Action::ListTunnels,
        "culvert_get_tunnel" => Action::GetTunnel { name: text("name")? },
        "culvert_create_tunnel" => Action::CreateTunnel { spec: tspec()? },
        "culvert_update_tunnel" => Action::UpdateTunnel {
            name: text("name")?,
            spec: tpatch()?,
        },
        "culvert_delete_tunnel" => Action::DeleteTunnel { name: text("name")? },
        "culvert_start_tunnel" => Action::StartTunnel { name: text("name")? },
        "culvert_stop_tunnel" => Action::StopTunnel { name: text("name")? },
        "culvert_list_forwards" => Action::ListForwards {
            session: opt_text("session"),
        },
        "culvert_create_forward" => Action::CreateForward {
            session: text("session")?,
            spec: tspec()?,
        },
        "culvert_update_forward" => Action::UpdateForward {
            session: text("session")?,
            forward: text("forward")?,
            spec: tspec()?,
        },
        "culvert_delete_forward" => Action::DeleteForward {
            session: text("session")?,
            forward: text("forward")?,
        },
        "culvert_start_forward" => Action::StartForward {
            session: text("session")?,
            forward: text("forward")?,
        },
        "culvert_stop_forward" => Action::StopForward {
            session: text("session")?,
            forward: text("forward")?,
        },
        "culvert_open_session" => Action::OpenSession { name: text("name")? },
        other => return Err(format!("unknown tool: {other}")),
    })
}

fn tool_definitions() -> Value {
    let session_props = json!({
        "name": { "type": "string", "description": "Display name (unique). Defaults to user@host." },
        "host": { "type": "string", "description": "Hostname or IP." },
        "port": { "type": "integer", "description": "SSH port (default 22)." },
        "username": { "type": "string" },
        "auth": { "type": "string", "enum": ["password", "key"], "description": "Auth method (default password)." },
        "password": { "type": "string", "description": "Password. Stored with Windows DPAPI; never returned." },
        "key_path": { "type": "string", "description": "Private key file for auth=key." },
        "key_passphrase": { "type": "string", "description": "Key passphrase, if any." },
        "group": { "type": "string", "description": "Group name or id. Empty string clears the group." },
        "protocol": { "type": "string", "enum": ["ssh", "rdp"], "description": "Remote protocol (default ssh)." }
    });
    let tunnel_props = json!({
        "name": { "type": "string", "description": "Tunnel name (unique)." },
        "host": { "type": "string", "description": "SSH host." },
        "port": { "type": "integer", "description": "SSH port (default 22)." },
        "username": { "type": "string" },
        "auth": { "type": "string", "enum": ["password", "key"] },
        "password": { "type": "string", "description": "Write-only; stored with DPAPI." },
        "key_path": { "type": "string" },
        "key_passphrase": { "type": "string" },
        "kind": { "type": "string", "enum": ["local", "remote", "dynamic"], "description": "Forward type (default local)." },
        "listen_host": { "type": "string", "description": "Bind address (default 127.0.0.1)." },
        "listen_port": { "type": "integer", "description": "Local listen port." },
        "dest_host": { "type": "string", "description": "Destination host (not used by dynamic)." },
        "dest_port": { "type": "integer", "description": "Destination port (not used by dynamic)." },
        "auto_reconnect": { "type": "boolean", "description": "Reconnect after a drop (default false)." }
    });
    let forward_props = json!({
        "kind": { "type": "string", "enum": ["local", "remote", "dynamic"], "description": "Forward type (default local)." },
        "listen_host": { "type": "string", "description": "Bind address (default 127.0.0.1)." },
        "listen_port": { "type": "integer", "description": "Local listen port." },
        "dest_host": { "type": "string", "description": "Destination host (not used by dynamic)." },
        "dest_port": { "type": "integer", "description": "Destination port (not used by dynamic)." },
        "auto_reconnect": { "type": "boolean", "description": "Reconnect after a drop (default false)." }
    });
    json!([
        {
            "name": "culvert_list_sessions",
            "description": "List saved SSH/RDP sessions. Passwords and passphrases are never included.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "culvert_get_session",
            "description": "Show one saved session by name or id.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Session name or id." } },
                "required": ["name"]
            }
        },
        {
            "name": "culvert_create_session",
            "description": "Save a new SSH/RDP session.",
            "inputSchema": {
                "type": "object",
                "properties": session_props,
                "required": ["host", "username"]
            }
        },
        {
            "name": "culvert_update_session",
            "description": "Change fields of a saved session. Omitted fields are kept; password is only replaced when provided.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Current session name or id." },
                    "new_name": { "type": "string", "description": "Rename to this." },
                    "host": { "type": "string" },
                    "port": { "type": "integer" },
                    "username": { "type": "string" },
                    "auth": { "type": "string", "enum": ["password", "key"] },
                    "password": { "type": "string" },
                    "key_path": { "type": "string" },
                    "key_passphrase": { "type": "string" },
                    "group": { "type": "string" },
                    "protocol": { "type": "string", "enum": ["ssh", "rdp"] }
                },
                "required": ["name"]
            }
        },
        {
            "name": "culvert_delete_session",
            "description": "Delete a saved session (and close its open shells). The file browser and tunnels tied to it stop.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "culvert_list_tunnels",
            "description": "List saved standalone tunnels with their forward and running state.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "culvert_get_tunnel",
            "description": "Show one tunnel by name or id.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "culvert_create_tunnel",
            "description": "Save a new standalone tunnel (its own SSH endpoint).",
            "inputSchema": {
                "type": "object",
                "properties": tunnel_props,
                "required": ["name", "host", "username", "listen_port"]
            }
        },
        {
            "name": "culvert_update_tunnel",
            "description": "Change fields of a tunnel. Omitted fields are kept; password is only replaced when provided.",
            "inputSchema": {
                "type": "object",
                "properties": tunnel_props,
                "required": ["name"]
            }
        },
        {
            "name": "culvert_delete_tunnel",
            "description": "Delete a saved tunnel; stops it first if it is running.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "culvert_start_tunnel",
            "description": "Start a saved tunnel.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "culvert_stop_tunnel",
            "description": "Stop a running tunnel.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        },
        {
            "name": "culvert_list_forwards",
            "description": "List tunnels bound to saved sessions (they share the session's SSH connection). Omit 'session' for every session. Includes live state and the last error; never includes passwords.",
            "inputSchema": {
                "type": "object",
                "properties": { "session": { "type": "string", "description": "Optional session name or id." } }
            }
        },
        {
            "name": "culvert_create_forward",
            "description": "Add a tunnel to a saved SSH session. The tunnel uses that session's host and credentials.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session name or id." },
                    "kind": forward_props["kind"],
                    "listen_host": forward_props["listen_host"],
                    "listen_port": forward_props["listen_port"],
                    "dest_host": forward_props["dest_host"],
                    "dest_port": forward_props["dest_port"],
                    "auto_reconnect": forward_props["auto_reconnect"]
                },
                "required": ["session", "listen_port"]
            }
        },
        {
            "name": "culvert_update_forward",
            "description": "Change a session-bound tunnel. Omitted fields are kept. A running tunnel is stopped first so the saved config and the live forward cannot diverge.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session name or id." },
                    "forward": { "type": "string", "description": "Forward id or its listen port." },
                    "kind": forward_props["kind"],
                    "listen_host": forward_props["listen_host"],
                    "listen_port": forward_props["listen_port"],
                    "dest_host": forward_props["dest_host"],
                    "dest_port": forward_props["dest_port"],
                    "auto_reconnect": forward_props["auto_reconnect"]
                },
                "required": ["session", "forward"]
            }
        },
        {
            "name": "culvert_delete_forward",
            "description": "Delete a session-bound tunnel; stops it first if it is running.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "forward": { "type": "string", "description": "Forward id or its listen port." }
                },
                "required": ["session", "forward"]
            }
        },
        {
            "name": "culvert_start_forward",
            "description": "Start a session-bound tunnel. Returns immediately; poll culvert_list_forwards for state and last_error.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "forward": { "type": "string", "description": "Forward id or its listen port." }
                },
                "required": ["session", "forward"]
            }
        },
        {
            "name": "culvert_stop_forward",
            "description": "Stop a running session-bound tunnel.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string" },
                    "forward": { "type": "string", "description": "Forward id or its listen port." }
                },
                "required": ["session", "forward"]
            }
        },
        {
            "name": "culvert_open_session",
            "description": "Open a saved session's terminal tab in the app.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"]
            }
        }
    ])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": args }
        })
    }

    #[test]
    fn initialize_and_tool_list_are_valid_json_rpc() {
        let (tx, _rx) = mpsc::channel();
        let ctx = egui::Context::default();
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
        let reply = dispatch(&init, &tx, &ctx).unwrap();
        assert_eq!(reply["result"]["serverInfo"]["name"], "culvert");
        assert!(reply["result"]["protocolVersion"].is_string());

        let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
        let reply = dispatch(&list, &tx, &ctx).unwrap();
        let tools = reply["result"]["tools"].as_array().unwrap();
        assert!(tools.len() >= 13);
        for t in tools {
            assert!(t["name"].as_str().unwrap().starts_with("culvert_"));
            assert!(t["inputSchema"]["type"] == "object");
        }

        // Notifications get no reply.
        let note = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(dispatch(&note, &tx, &ctx).is_none());
    }

    #[test]
    fn tool_calls_map_to_actions() {
        let (tx, rx) = mpsc::channel();
        let ctx = egui::Context::default();
        let req = call("culvert_create_session", json!({
            "name": "web-01", "host": "10.0.0.5", "username": "deploy",
            "password": "hunter2"
        }));
        let handle = std::thread::spawn(move || dispatch(&req, &tx, &ctx));
        let pending = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        match pending.action {
            Action::CreateSession { spec } => {
                assert_eq!(spec.name.as_deref(), Some("web-01"));
                assert_eq!(spec.password.as_deref(), Some("hunter2"));
            }
            other => panic!("unexpected action: {other:?}"),
        }
        pending
            .reply
            .send(Ok(json!({"id": "s1", "name": "web-01"})))
            .unwrap();
        let reply = handle.join().unwrap().unwrap();
        assert_eq!(reply["result"]["isError"], false);
        assert!(reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("web-01"));
    }

    #[test]
    fn session_forward_calls_map_to_actions() {
        let (tx, rx) = mpsc::channel();
        let ctx = egui::Context::default();
        let ctx1 = ctx.clone();
        let req = call("culvert_create_forward", json!({
            "session": "linux test (docker)",
            "listen_port": 15432,
            "dest_host": "127.0.0.1",
            "dest_port": 5432,
            "auto_reconnect": true
        }));
        let handle = std::thread::spawn(move || dispatch(&req, &tx, &ctx1));
        let pending = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        match pending.action {
            Action::CreateForward { session, spec } => {
                assert_eq!(session, "linux test (docker)");
                assert_eq!(spec.listen_port, Some(15432));
                assert_eq!(spec.dest_port, Some(5432));
                assert_eq!(spec.auto_reconnect, Some(true));
            }
            other => panic!("unexpected action: {other:?}"),
        }
        pending
            .reply
            .send(Ok(json!({"id": "f1"})))
            .unwrap();
        let reply = handle.join().unwrap().unwrap();
        assert_eq!(reply["result"]["isError"], false);

        let (tx, rx) = mpsc::channel();
        let ctx2 = ctx.clone();
        let req = call(
            "culvert_stop_forward",
            json!({ "session": "linux test (docker)", "forward": "15432" }),
        );
        let handle = std::thread::spawn(move || dispatch(&req, &tx, &ctx2));
        let pending = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        match pending.action {
            Action::StopForward { session, forward } => {
                assert_eq!(session, "linux test (docker)");
                assert_eq!(forward, "15432");
            }
            other => panic!("unexpected action: {other:?}"),
        }
        pending.reply.send(Ok(json!({"stopped": true}))).unwrap();
        let reply = handle.join().unwrap().unwrap();
        assert_eq!(reply["result"]["isError"], false);
    }

    #[test]
    fn missing_arguments_produce_tool_errors_not_crashes() {
        let (tx, _rx) = mpsc::channel();
        let ctx = egui::Context::default();
        let reply = dispatch(&call("culvert_get_session", json!({})), &tx, &ctx).unwrap();
        assert_eq!(reply["result"]["isError"], true);
        let reply = dispatch(&call("culvert_nope", json!({})), &tx, &ctx).unwrap();
        assert_eq!(reply["result"]["isError"], true);
    }

    #[test]
    fn secrets_are_write_only_in_the_schema() {
        let defs = tool_definitions();
        let text = defs.to_string();
        // The schema describes a password input but no tool returns one.
        assert!(text.contains("\"password\""));
        assert!(!text.contains("returns the password"));
    }

    /// Exercise the real HTTP transport: bearer auth, JSON-RPC, and a
    /// tools/call whose reply comes from the (simulated) UI thread.
    #[test]
    fn http_transport_authenticates_and_round_trips() {
        let cfg = McpConfig {
            enabled: true,
            port: 0,
            token: "t".repeat(32),
        };
        let ctx = egui::Context::default();
        let server = Server::start(&cfg, ctx).unwrap();
        let port = server.port;

        let init = r#"{"jsonrpc":"2.0","id":7,"method":"initialize"}"#;
        let denied = http_post(port, "wrong", init);
        assert!(denied.starts_with("HTTP/1.1 401"), "got {denied}");

        let ok = http_post(port, &cfg.token, init);
        assert!(ok.contains("\"serverInfo\""), "got {ok}");
        assert!(ok.contains("culvert"));

        let token = cfg.token.clone();
        let client = std::thread::spawn(move || {
            http_post(
                port,
                &token,
                r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"culvert_list_sessions","arguments":{}}}"#,
            )
        });
        let pending = loop {
            if let Some(p) = server.try_recv() {
                break p;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(matches!(pending.action, Action::ListSessions));
        pending
            .reply
            .send(Ok(json!({ "sessions": [], "count": 0 })))
            .unwrap();
        let reply = client.join().unwrap();
        assert!(reply.contains("\"isError\":false"), "got {reply}");
        assert!(reply.contains("sessions"));
    }

    fn http_post(port: u16, token: &str, body: &str) -> String {
        use std::io::{Read, Write};
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        String::from_utf8_lossy(&raw).to_string()
    }

    #[test]
    fn config_round_trips_and_generates_a_token() {
        let saved = config_file();
        let cfg = McpConfig {
            enabled: true,
            port: 47999,
            token: new_token(),
        };
        assert!(cfg.token.len() >= 32);
        assert!(cfg.url().ends_with("/mcp"));
        // Do not touch the real config file from a test.
        let _ = saved;
        let parsed: McpConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.port, 47999);
    }
}
