//! Stdio MCP shim: forwards newline-delimited JSON-RPC from an MCP client
//! (Claude Desktop, Cursor, ...) to the running Culvert app over loopback
//! HTTP.
//!
//! It is deliberately tiny and stateless: the port and bearer token live in
//! the same `mcp.json` the app writes, so rotating the token in the UI is
//! enough — no client config edits, no secrets baked into the executable or
//! the client config.

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

#[derive(serde::Deserialize)]
struct Config {
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    token: String,
}

fn default_port() -> u16 {
    47822
}

fn config_file() -> PathBuf {
    dirs::config_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("culvert")
        .join("mcp.json")
}

fn main() {
    let path = config_file();
    let cfg = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Config>(s.trim_start_matches('\u{feff}')).ok())
    {
        Some(cfg) if cfg.token.trim().len() >= 16 => cfg,
        _ => {
            eprintln!(
                "culvert-mcp: no usable config at {} — open Culvert, Tools > MCP server, and press Start.",
                path.display()
            );
            std::process::exit(2);
        }
    };

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match forward(&cfg, line) {
            Ok(Some(body)) => {
                let _ = writeln!(stdout, "{body}");
                let _ = stdout.flush();
            }
            // Notifications get a 202 with no body.
            Ok(None) => {}
            Err(err) => {
                // Answer requests with a JSON-RPC error so the client shows a
                // useful message instead of hanging.
                let id = serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| v.get("id").cloned());
                if let Some(id) = id {
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32000,
                            "message": format!(
                                "Culvert MCP unavailable: {err}. Start Culvert and enable Tools > MCP server."
                            )
                        }
                    });
                    let _ = writeln!(stdout, "{reply}");
                    let _ = stdout.flush();
                }
            }
        }
    }
}

fn forward(cfg: &Config, body: &str) -> Result<Option<String>, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", cfg.port))
        .map_err(|e| format!("cannot reach 127.0.0.1:{} ({e})", cfg.port))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let request = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Content-Type: application/json\r\n\
         Authorization: Bearer {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{}",
        cfg.token,
        body.len(),
        body
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw);
    let (head, rest) = text.split_once("\r\n\r\n").ok_or("no HTTP response")?;
    let status = head.lines().next().unwrap_or("");
    if !(status.contains(" 200") || status.contains(" 202")) {
        return Err(format!("server said {status}: {}", rest.trim()));
    }
    Ok(if rest.trim().is_empty() {
        None
    } else {
        Some(rest.to_string())
    })
}
