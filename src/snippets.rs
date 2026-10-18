//! Saved command snippets (macros): one-click commands you can fire into any
//! open shell from the terminal quick-bar — like MobaXterm's macros. Stored
//! locally in `snippets.json` next to the other app data.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

fn config_base() -> PathBuf {
    dirs::config_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn snippets_file() -> PathBuf {
    config_base().join("culvert").join("snippets.json")
}

/// Strip a leading UTF-8 BOM — same treatment as the other data files.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snippet {
    pub id: String,
    pub name: String,
    pub command: String,
}

impl Snippet {
    pub fn new(name: &str, command: &str) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            command: command.to_string(),
        }
    }
}

pub fn load_snippets() -> Vec<Snippet> {
    match fs::read_to_string(snippets_file()) {
        Ok(s) => serde_json::from_str::<Vec<Snippet>>(strip_bom(&s)).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save_snippets(snippets: &[Snippet]) -> anyhow::Result<PathBuf> {
    save_snippets_to(snippets, snippets_file())
}

fn save_snippets_to(snippets: &[Snippet], path: PathBuf) -> anyhow::Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(snippets)?;
    fs::write(&path, json)?;
    Ok(path)
}

/// Parse the quick-connect target: `user@host` or `user@host:port`
/// (port defaults to 22). Returns `(user, host, port)`.
pub fn parse_host_target(s: &str) -> Result<(String, String, u16), String> {
    let s = s.trim();
    let Some((user, rest)) = s.rsplit_once('@') else {
        return Err("Use the form user@host or user@host:port".to_string());
    };
    let user = user.trim();
    if user.is_empty() {
        return Err("Add a username: user@host".to_string());
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => {
            let port = p
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("'{}' is not a valid port (1–65535)", p.trim()))?;
            if port == 0 {
                return Err("Port must be between 1 and 65535".to_string());
            }
            (h.trim(), port)
        }
        None => (rest.trim(), 22_u16),
    };
    if host.is_empty() {
        return Err("Add a host: user@host".to_string());
    }
    if host.contains(['@', ' ']) {
        return Err("Host looks wrong — use user@host:port".to_string());
    }
    Ok((user.to_string(), host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_quick_connect_targets() {
        assert_eq!(
            parse_host_target("root@10.0.0.5"),
            Ok(("root".into(), "10.0.0.5".into(), 22))
        );
        assert_eq!(
            parse_host_target("  deploy@server.example.com:2200  "),
            Ok(("deploy".into(), "server.example.com".into(), 2200))
        );
        assert_eq!(
            parse_host_target("linuxio@127.0.0.1:2222"),
            Ok(("linuxio".into(), "127.0.0.1".into(), 2222))
        );
        // Errors.
        assert!(parse_host_target("10.0.0.5").is_err());
        assert!(parse_host_target("@10.0.0.5").is_err());
        assert!(parse_host_target("root@").is_err());
        assert!(parse_host_target("root@host:0").is_err());
        assert!(parse_host_target("root@host:abc").is_err());
    }

    #[test]
    fn snippets_round_trip() {
        // Round-trips through a throwaway file, never the real config dir.
        let dir = std::env::temp_dir().join(format!("culvert-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("snippets.json");
        let snips = vec![Snippet::new("uptime", "uptime\n")];
        save_snippets_to(&snips, path.clone()).expect("save");
        let loaded: Vec<Snippet> =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded[0].command, "uptime\n");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(dir);
    }
}
