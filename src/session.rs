use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum AuthType {
    Password,
    Key,
}

impl Default for AuthType {
    fn default() -> Self {
        AuthType::Password
    }
}

impl AuthType {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthType::Password => "Password",
            AuthType::Key => "Private Key",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ForwardType {
    /// Local forwarding:  -L [listen_host:]listen_port:dest_host:dest_port
    Local,
    /// Remote forwarding: -R [listen_host:]listen_port:dest_host:dest_port
    Remote,
    /// Dynamic (SOCKS5): -D [listen_host:]listen_port
    Dynamic,
}

impl Default for ForwardType {
    fn default() -> Self {
        ForwardType::Local
    }
}

impl ForwardType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ForwardType::Local => "Local (-L)",
            ForwardType::Remote => "Remote (-R)",
            ForwardType::Dynamic => "Dynamic (-D)",
        }
    }

    #[allow(dead_code)]
    pub fn short(&self) -> &'static str {
        match self {
            ForwardType::Local => "L",
            ForwardType::Remote => "R",
            ForwardType::Dynamic => "D",
        }
    }
}

fn gen_forward_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortForward {
    /// Stable id so each tunnel can be started/stopped on its own.
    #[serde(default = "gen_forward_id")]
    pub id: String,
    pub fwd_type: ForwardType,
    pub listen_host: String,
    pub listen_port: u16,
    pub dest_host: String,
    pub dest_port: u16,
    /// Bring the tunnel back automatically (with backoff) after the SSH
    /// connection drops.
    #[serde(default)]
    pub auto_reconnect: bool,
}

impl Default for PortForward {
    fn default() -> Self {
        Self {
            id: gen_forward_id(),
            fwd_type: ForwardType::Local,
            listen_host: "127.0.0.1".to_string(),
            listen_port: 8080,
            dest_host: "127.0.0.1".to_string(),
            dest_port: 80,
            auto_reconnect: false,
        }
    }
}

impl PortForward {
    pub fn validate(&self) -> Result<(), String> {
        if self.listen_port == 0 {
            return Err("Listen port must be > 0".to_string());
        }
        match self.fwd_type {
            ForwardType::Dynamic => Ok(()),
            _ => {
                if self.dest_host.trim().is_empty() {
                    return Err("Destination host is required".to_string());
                }
                if self.dest_port == 0 {
                    return Err("Destination port must be > 0".to_string());
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SshSession {
    pub id: String,
    pub name: String,
    /// Optional group id (see `SessionGroup`) to segregate sessions by
    /// environment, customer, team… whatever suites the user.
    #[serde(default)]
    pub group: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_type: AuthType,
    /// NOTE: stored in plain JSON on local disk for reuse convenience,
    /// same tradeoff MobaXterm makes for saved sessions. Protect your user profile.
    pub password: String,
    pub key_path: String,
    pub key_passphrase: String,
    pub forwards: Vec<PortForward>,
}

impl Default for SshSession {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: "New session".to_string(),
            group: None,
            host: "".to_string(),
            port: 22,
            username: "".to_string(),
            auth_type: AuthType::Password,
            password: String::new(),
            key_path: String::new(),
            key_passphrase: String::new(),
            forwards: Vec::new(),
        }
    }
}

/// A named bucket that sessions can belong to (Production, Staging,
/// Home lab…). Stored next to the sessions so the order, color and empty
/// groups survive restarts.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionGroup {
    #[serde(default = "gen_group_id")]
    pub id: String,
    pub name: String,
    /// Optional accent color as RGB bytes for the sidebar dot.
    #[serde(default)]
    pub color: Option<[u8; 3]>,
    /// Sidebar section expanded? Persisted so the UI looks the same after
    /// a restart.
    #[serde(default = "default_true")]
    pub expanded: bool,
}

fn gen_group_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn default_true() -> bool {
    true
}

impl SshSession {
    /// Connection-level validation. Tunnels live on the tunnels page and are
    /// validated individually, so a session without any forward is valid —
    /// it can simply be used for a shell.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Session name is required".to_string());
        }
        if self.host.trim().is_empty() {
            return Err("SSH host is required".to_string());
        }
        if self.port == 0 {
            return Err("SSH port must be > 0".to_string());
        }
        if self.username.trim().is_empty() {
            return Err("Username is required".to_string());
        }
        match self.auth_type {
            AuthType::Password => {
                if self.password.is_empty() {
                    return Err("Password is required for password auth".to_string());
                }
            }
            AuthType::Key => {
                if self.key_path.trim().is_empty() {
                    return Err("Private key file is required for key auth".to_string());
                }
            }
        }
        Ok(())
    }

    /// `user@host:port` as shown throughout the UI.
    pub fn connection_label(&self) -> String {
        format!(
            "{}@{}:{}",
            self.username.trim(),
            self.host.trim(),
            self.port
        )
    }
}

fn config_base() -> PathBuf {
    dirs::config_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn sessions_file() -> PathBuf {
    config_base().join("culvert").join("sessions.json")
}

/// Location used before the app was renamed to Culvert, kept so existing
/// installs keep their sessions via a one-time import.
fn legacy_sessions_file() -> PathBuf {
    config_base().join("ssh-tunnel-manager").join("sessions.json")
}

pub fn load_sessions() -> Vec<SshSession> {
    let path = sessions_file();
    if !path.exists() {
        // First launch after the rename: import and persist old sessions.
        if let Ok(s) = fs::read_to_string(legacy_sessions_file()) {
            if let Ok(sessions) = serde_json::from_str::<Vec<SshSession>>(&s) {
                let _ = save_sessions(&sessions);
                return sessions;
            }
        }
        return Vec::new();
    }
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str::<Vec<SshSession>>(&s).unwrap_or_else(|_| Vec::new()),
        Err(_) => Vec::new(),
    }
}

pub fn save_sessions(sessions: &[SshSession]) -> anyhow::Result<PathBuf> {
    let path = sessions_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(sessions)?;
    fs::write(&path, json)?;
    Ok(path)
}

pub fn groups_file() -> PathBuf {
    config_base().join("culvert").join("groups.json")
}

pub fn load_groups() -> Vec<SessionGroup> {
    match fs::read_to_string(groups_file()) {
        Ok(s) => serde_json::from_str::<Vec<SessionGroup>>(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save_groups(groups: &[SessionGroup]) -> anyhow::Result<PathBuf> {
    let path = groups_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(groups)?;
    fs::write(&path, json)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_validation_requires_fields() {
        let s = SshSession::default();
        assert!(s.validate().is_err());
    }

    #[test]
    fn session_validation_ok() {
        let mut s = SshSession::default();
        s.name = "demo".into();
        s.host = "example.com".into();
        s.username = "root".into();
        s.password = "secret".into();
        // No forwards required: shell-only sessions are valid.
        assert!(s.validate().is_ok());
        s.forwards.push(PortForward::default());
        assert!(s.validate().is_ok());
    }

    #[test]
    fn forward_gets_stable_id() {
        let f = PortForward::default();
        assert!(!f.id.is_empty());
        let json = serde_json::to_string(&f).unwrap();
        let back: PortForward = serde_json::from_str(&json).unwrap();
        assert_eq!(f.id, back.id);
    }

    #[test]
    fn legacy_forward_json_without_id_still_loads() {
        let legacy = r#"{"fwd_type":"Local","listen_host":"127.0.0.1","listen_port":8080,"dest_host":"127.0.0.1","dest_port":80}"#;
        let f: PortForward = serde_json::from_str(legacy).unwrap();
        assert!(!f.id.is_empty());
        // Tunnels recorded before auto-reconnect existed stay opt-in.
        assert!(!f.auto_reconnect);
    }

    #[test]
    fn legacy_session_json_without_group_still_loads() {
        let legacy = r#"[{"id":"x","name":"n","host":"h","port":22,"username":"u","auth_type":"Password","password":"p","key_path":"","key_passphrase":"","forwards":[]}]"#;
        let sessions: Vec<SshSession> = serde_json::from_str(legacy).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].group, None);
    }
}
