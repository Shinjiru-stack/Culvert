//! SSH port-forward engine.
//!
//! Every tunnel is its own SSH connection + forward, exactly like the
//! MobaXterm tunnel window: start/stop individually, no commands, fully
//! driven from the UI.

use crate::log::{push_log, LogEntry, LogLevel, SharedLogs};
use crate::session::{ForwardType, PortForward, SshSession};
use crate::ssh::{self, SharedSshHandle};
use russh::client;
use russh::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Key identifying a running tunnel: session id + forward id.
pub fn forward_key(session_id: &str, forward_id: &str) -> String {
    format!("{session_id}::{forward_id}")
}

async fn bridge(
    tcp: TcpStream,
    channel: Channel<client::Msg>,
    logs: SharedLogs,
    label: String,
    desc: String,
) {
    let mut stream = channel.into_stream();
    let mut tcp = tcp;
    match tokio::io::copy_bidirectional(&mut tcp, &mut stream).await {
        Ok((a, b)) => {
            push_log(
                &logs,
                LogEntry::new(
                    LogLevel::Info,
                    &label,
                    &format!("Closed {desc} (client→server {a}B / server→client {b}B)"),
                ),
            );
        }
        Err(e) => {
            push_log(
                &logs,
                LogEntry::new(LogLevel::Error, &label, &format!("{desc} I/O error: {e}")),
            );
        }
    }
    let _ = tcp.shutdown().await;
}

async fn serve_local(
    listener: TcpListener,
    handle: SharedSshHandle,
    dest_host: String,
    dest_port: u32,
    logs: SharedLogs,
    label: String,
    bind: String,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                push_log(&logs, LogEntry::new(LogLevel::Info, &label, &format!("Stopped listening on {bind}")));
                break;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((sock, peer)) => {
                        let h = handle.clone();
                        let logs2 = logs.clone();
                        let label2 = label.clone();
                        let dh = dest_host.clone();
                        let desc = format!("local {bind} -> {dh}:{dest_port} [{peer}]");
                        push_log(&logs, LogEntry::new(LogLevel::Info, &label, &format!("Incoming {peer} -> {dh}:{dest_port}")));
                        tokio::spawn(async move {
                            match h
                                .channel_open_direct_tcpip(
                                    dh.clone(),
                                    dest_port,
                                    "127.0.0.1",
                                    peer.port() as u32,
                                )
                                .await
                            {
                                Ok(ch) => bridge(sock, ch, logs2, label2, desc).await,
                                Err(e) => push_log(
                                    &logs2,
                                    LogEntry::new(LogLevel::Error, &label2, &format!("direct-tcpip to {dh}:{dest_port} failed: {e}")),
                                ),
                            }
                        });
                    }
                    Err(e) => {
                        push_log(&logs, LogEntry::new(LogLevel::Error, &label, &format!("accept error on {bind}: {e}")));
                    }
                }
            }
        }
    }
}

/// Minimal SOCKS5 (CONNECT, no-auth) for Dynamic (-D) forwarding.
async fn serve_dynamic(
    listener: TcpListener,
    handle: SharedSshHandle,
    logs: SharedLogs,
    label: String,
    bind: String,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                push_log(&logs, LogEntry::new(LogLevel::Info, &label, &format!("Stopped SOCKS on {bind}")));
                break;
            }
            accepted = listener.accept() => {
                let (sock, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        push_log(&logs, LogEntry::new(LogLevel::Error, &label, &format!("SOCKS accept error: {e}")));
                        continue;
                    }
                };
                let h = handle.clone();
                let logs2 = logs.clone();
                let label2 = label.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_socks5(sock, h).await {
                        push_log(&logs2, LogEntry::new(LogLevel::Error, &label2, &format!("SOCKS client {peer}: {e:#}")));
                    }
                });
            }
        }
    }
}

async fn handle_socks5(mut sock: TcpStream, handle: SharedSshHandle) -> anyhow::Result<()> {
    let mut ver = [0u8; 2];
    sock.read_exact(&mut ver).await?;
    if ver[0] != 0x05 {
        anyhow::bail!("unsupported SOCKS version {}", ver[0]);
    }
    let mut methods = vec![0u8; ver[1] as usize];
    sock.read_exact(&mut methods).await?;
    sock.write_all(&[0x05, 0x00]).await?;

    let mut hdr = [0u8; 4];
    sock.read_exact(&mut hdr).await?;
    if hdr[0] != 0x05 || hdr[1] != 0x01 {
        sock.write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await?;
        anyhow::bail!("only SOCKS5 CONNECT supported");
    }
    let dest_host = match hdr[3] {
        0x01 => {
            let mut ip = [0u8; 4];
            sock.read_exact(&mut ip).await?;
            format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
        }
        0x03 => {
            let mut l = [0u8; 1];
            sock.read_exact(&mut l).await?;
            let mut d = vec![0u8; l[0] as usize];
            sock.read_exact(&mut d).await?;
            String::from_utf8_lossy(&d).to_string()
        }
        0x04 => {
            let mut ip = [0u8; 16];
            sock.read_exact(&mut ip).await?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        t => anyhow::bail!("unsupported ATYP {t}"),
    };
    let mut pb = [0u8; 2];
    sock.read_exact(&mut pb).await?;
    let dest_port = u16::from_be_bytes(pb) as u32;
    let origin_port = sock.peer_addr().map(|p| p.port() as u32).unwrap_or(0);

    match handle
        .channel_open_direct_tcpip(dest_host.clone(), dest_port, "127.0.0.1", origin_port)
        .await
    {
        Ok(ch) => {
            sock.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await?;
            let mut stream = ch.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
            Ok(())
        }
        Err(e) => {
            let _ = sock.write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
            anyhow::bail!("remote refused {dest_host}:{dest_port}: {e}");
        }
    }
}

/// Runs ONE tunnel until cancelled: connect, set up the forward, the
/// teardown. `prepared` is a locally pre-bound listener (Local/Dynamic)
/// so bind errors are reported synchronously to the UI.
async fn run_tunnel_task(
    session: SshSession,
    forward: PortForward,
    prepared: Option<std::net::TcpListener>,
    logs: SharedLogs,
    cancel: CancellationToken,
) {
    let label = session.name.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Channel<client::Msg>>();

    let handle = match ssh::connect(&session, logs.clone(), tx).await {
        Ok(h) => Arc::new(h),
        Err(e) => {
            push_log(
                &logs,
                LogEntry::new(LogLevel::Error, &label, &format!("Tunnel not started: {e:#}")),
            );
            return;
        }
    };

    let mut bg: Vec<JoinHandle<()>> = Vec::new();
    // Remote forward bookkeeping for clean release on stop.
    let mut remote_release: Option<(String, u32)> = None;

    match forward.fwd_type {
        ForwardType::Local => {
            let Some(l) = prepared else { return };
            let listener = match TcpListener::from_std(l) {
                Ok(l) => l,
                Err(e) => {
                    push_log(&logs, LogEntry::new(LogLevel::Error, &label, &format!("Listener setup failed: {e}")));
                    return;
                }
            };
            let bind = format!("{}:{}", forward.listen_host, forward.listen_port);
            push_log(
                &logs,
                LogEntry::new(
                    LogLevel::Success,
                    &label,
                    &format!("Local forward ON: {bind} -> {}:{}", forward.dest_host, forward.dest_port),
                ),
            );
            let (h, l, lb, c) = (handle.clone(), logs.clone(), label.clone(), cancel.clone());
            let (dh, dp) = (forward.dest_host.clone(), forward.dest_port as u32);
            bg.push(tokio::spawn(async move {
                serve_local(listener, h, dh, dp, l, lb, bind, c).await;
            }));
        }
        ForwardType::Dynamic => {
            let Some(l) = prepared else { return };
            let listener = match TcpListener::from_std(l) {
                Ok(l) => l,
                Err(e) => {
                    push_log(&logs, LogEntry::new(LogLevel::Error, &label, &format!("Listener setup failed: {e}")));
                    return;
                }
            };
            let bind = format!("{}:{}", forward.listen_host, forward.listen_port);
            push_log(
                &logs,
                LogEntry::new(
                    LogLevel::Success,
                    &label,
                    &format!("Dynamic SOCKS5 ON: {bind}"),
                ),
            );
            let (h, l, lb, c) = (handle.clone(), logs.clone(), label.clone(), cancel.clone());
            bg.push(tokio::spawn(async move {
                serve_dynamic(listener, h, l, lb, bind, c).await;
            }));
        }
        ForwardType::Remote => {
            let bind_host = if forward.listen_host.trim().is_empty() {
                "0.0.0.0".to_string()
            } else {
                forward.listen_host.clone()
            };
            match handle
                .tcpip_forward(bind_host.clone(), forward.listen_port as u32)
                .await
            {
                Ok(bound) => {
                    let actual = if forward.listen_port == 0 {
                        bound
                    } else {
                        forward.listen_port as u32
                    };
                    let bind_desc = format!("{bind_host}:{actual}");
                    let target = format!("{}:{}", forward.dest_host, forward.dest_port);
                    push_log(
                        &logs,
                        LogEntry::new(
                            LogLevel::Success,
                            &label,
                            &format!("Remote forward ON: {bind_desc} (on server) -> {target}"),
                        ),
                    );
                    remote_release = Some((bind_host, actual));

                    // Dispatcher: every server-initiated connection for this
                    // forward is bridged to the local target.
                    let (l, lb, c) = (logs.clone(), label.clone(), cancel.clone());
                    bg.push(tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                _ = c.cancelled() => break,
                                ch_opt = rx.recv() => {
                                    let Some(channel) = ch_opt else { break };
                                    match TcpStream::connect(target.clone()).await {
                                        Ok(tcp) => {
                                            bridge(tcp, channel, l.clone(), lb.clone(),
                                                format!("remote {bind_desc} -> {target}")).await;
                                        }
                                        Err(e) => {
                                            push_log(&l, LogEntry::new(LogLevel::Error, &lb,
                                                &format!("local target {target} unreachable: {e}")));
                                            let _ = channel.close().await;
                                        }
                                    }
                                }
                            }
                        }
                    }));
                }
                Err(e) => {
                    push_log(
                        &logs,
                        LogEntry::new(
                            LogLevel::Error,
                            &label,
                            &format!("remote forward {bind_host}:{} denied: {e}", forward.listen_port),
                        ),
                    );
                    return;
                }
            }
        }
    }

    cancel.cancelled().await;
    for t in bg {
        t.abort();
    }
    if let Some((host, port)) = remote_release {
        let _ = handle.cancel_tcpip_forward(host, port).await;
    }
    let _ = handle
        .disconnect(Disconnect::ByApplication, "tunnel stopped", "")
        .await;
    push_log(
        &logs,
        LogEntry::new(LogLevel::Info, &label, "Tunnel stopped"),
    );
}

// ---------------------------------------------------------------------------
// Manager: tracks running tunnels from the UI thread
// ---------------------------------------------------------------------------

struct RunningTunnel {
    cancel: CancellationToken,
}

pub struct TunnelManager {
    rt: tokio::runtime::Handle,
    logs: SharedLogs,
    running: HashMap<String, RunningTunnel>,
}

impl TunnelManager {
    pub fn new(rt: tokio::runtime::Handle, logs: SharedLogs) -> Self {
        Self {
            rt,
            logs,
            running: HashMap::new(),
        }
    }

    pub fn is_running(&self, key: &str) -> bool {
        self.running.contains_key(key)
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// True when any tunnel of this session is currently running.
    pub fn session_has_active(&self, session_id: &str) -> bool {
        let prefix = format!("{session_id}::");
        self.running.keys().any(|k| k.starts_with(&prefix))
    }

    /// Start one tunnel. Local/dynamic listeners are bound synchronously so
    /// errors like "port in use" are reported to the caller immediately.
    pub fn start(&mut self, session: &SshSession, forward: &PortForward) -> Result<(), String> {
        let key = forward_key(&session.id, &forward.id);
        if self.running.contains_key(&key) {
            return Ok(());
        }
        session.validate()?;
        forward.validate()?;

        let prepared = match forward.fwd_type {
            ForwardType::Local | ForwardType::Dynamic => {
                let bind = format!("{}:{}", forward.listen_host, forward.listen_port);
                let l = std::net::TcpListener::bind(&bind)
                    .map_err(|e| format!("Cannot listen on {bind}: {e} (port in use?)"))?;
                l.set_nonblocking(true).map_err(|e| e.to_string())?;
                Some(l)
            }
            ForwardType::Remote => None,
        };

        let cancel = CancellationToken::new();
        // Detached task: dropping the JoinHandle keeps it running; the
        // cancellation token is what drives shutdown.
        let _task = self.rt.spawn(run_tunnel_task(
            session.clone(),
            forward.clone(),
            prepared,
            self.logs.clone(),
            cancel.clone(),
        ));
        self.running.insert(key, RunningTunnel { cancel });
        Ok(())
    }

    pub fn stop(&mut self, key: &str) {
        if let Some(r) = self.running.remove(key) {
            // Graceful: the task observes the cancellation and tears the
            // SSH connection down itself.
            r.cancel.cancel();
        }
    }

    /// Stop every tunnel belonging to a session (e.g. before deletion).
    pub fn stop_for_session(&mut self, session_id: &str) {
        let prefix = format!("{session_id}::");
        let keys: Vec<String> = self
            .running
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        for k in keys {
            self.stop(&k);
        }
    }
}
