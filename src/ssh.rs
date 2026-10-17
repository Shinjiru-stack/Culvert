//! Shared SSH plumbing: the russh client handler and an authenticated
//! `connect()` used by both the tunnel engine and terminal sessions.

use crate::log::{push_log, LogEntry, LogLevel, SharedLogs};
use crate::session::{AuthType, SshSession};
use russh::keys::{PublicKeyOrCertificate, load_secret_key};
use russh::*;
use std::sync::Arc;

/// Handler for every SSH connection (shells and tunnels alike).
/// Accepts unknown host keys (MobaXterm-style) and routes server-initiated
/// `forwarded-tcpip` channels (remote `-R` forwards) into a queue that the
/// owning task drains. Shell-only connections just leave the queue empty.
pub struct ClientHandler {
    tx: tokio::sync::mpsc::UnboundedSender<Channel<client::Msg>>,
}

impl ClientHandler {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<Channel<client::Msg>>) -> Self {
        Self { tx }
    }
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // Accept unknown host keys (MobaXterm-style). Harden by pinning
        // known_hosts here for production use.
        Ok(true)
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        _connected_address: &str,
        _connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let _ = self.tx.send(channel);
        reply.accept().await;
        Ok(())
    }
}

pub type SshHandle = client::Handle<ClientHandler>;
pub type SharedSshHandle = Arc<SshHandle>;

/// Connect and authenticate. `forwarded_tx` receives server-initiated
/// forwarded-tcpip channels; pass a throwaway queue for shell-only use.
pub async fn connect(
    session: &SshSession,
    logs: SharedLogs,
    forwarded_tx: tokio::sync::mpsc::UnboundedSender<Channel<client::Msg>>,
) -> anyhow::Result<SshHandle> {
    let label = session.name.clone();
    let addr_str = format!("{}:{}", session.host.trim(), session.port);
    let user = session.username.trim().to_string();

    push_log(
        &logs,
        LogEntry::new(
            LogLevel::Info,
            &label,
            &format!("Connecting to {addr_str} as {user} …"),
        ),
    );

    let mut config = client::Config::default();
    // Detect dead connections (dropped WiFi, sleeping laptop, killed VPN)
    // instead of hanging forever: after 25s of server silence send a
    // keepalive and give up after 3 unanswered ones.
    config.keepalive_interval = Some(std::time::Duration::from_secs(25));
    config.keepalive_max = 3;
    let config = Arc::new(config);
    let mut handle = client::connect(
        config,
        (session.host.trim(), session.port),
        ClientHandler::new(forwarded_tx),
    )
    .await
    .map_err(|e| anyhow::anyhow!("SSH connect to {addr_str} failed: {e}"))?;

    push_log(
        &logs,
        LogEntry::new(LogLevel::Info, &label, "TCP connected, authenticating …"),
    );

    match session.auth_type {
        AuthType::Password => {
            let res = handle
                .authenticate_password(user.clone(), session.password.clone())
                .await
                .map_err(|e| anyhow::anyhow!("password auth error: {e}"))?;
            if !res.success() {
                anyhow::bail!("password authentication rejected for user '{user}'");
            }
        }
        AuthType::Key => {
            let key_path = session.key_path.trim();
            let passphrase = if session.key_passphrase.is_empty() {
                None
            } else {
                Some(session.key_passphrase.as_str())
            };
            let key = load_secret_key(key_path, passphrase)
                .map_err(|e| anyhow::anyhow!("cannot load private key '{key_path}': {e}"))?;
            let hash_alg = handle.best_supported_rsa_hash().await?.flatten();
            let key_with_alg = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
            let res = handle
                .authenticate_publickey(user.clone(), key_with_alg)
                .await
                .map_err(|e| anyhow::anyhow!("publickey auth error: {e}"))?;
            if !res.success() {
                anyhow::bail!("publickey authentication rejected for user '{user}'");
            }
        }
    }

    push_log(
        &logs,
        LogEntry::new(LogLevel::Success, &label, &format!("Authenticated as {user}")),
    );
    Ok(handle)
}
