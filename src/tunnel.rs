//! SSH port-forward engine.
//!
//! Every tunnel is its own SSH connection + forward, exactly like the
//! MobaXterm tunnel window: start/stop individually, no commands, fully
//! driven from the UI. Tunnels with auto-reconnect enabled are supervised:
//! when the SSH connection drops, the task reconnects with backoff until
//! the user stops it.

use crate::log::{push_log, LogEntry, LogLevel, SharedLogs};
use crate::session::{ForwardType, PortForward, SshSession};
use crate::ssh::{self, SharedSshHandle};
use russh::client;
use russh::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Key identifying a running tunnel: session id + forward id.
pub fn forward_key(session_id: &str, forward_id: &str) -> String {
    format!("{session_id}::{forward_id}")
}

/// Standalone tunnels run under a synthetic session whose id is
/// `tunnel-<tunnel id>` (see `StandaloneTunnel::to_ssh_session`). Use this
/// wherever a running key is computed from a `tunnels.json` id.
pub fn standalone_key(tunnel_id: &str, forward_id: &str) -> String {
    forward_key(&format!("tunnel-{tunnel_id}"), forward_id)
}

/// First reconnect delay; doubles up to `MAX_RECONNECT_DELAY` per attempt
/// and resets whenever a connection served at least once.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(15);

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
    errors: SharedErrors,
    key: String,
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
                        let errors2 = errors.clone();
                        let key2 = key.clone();
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
                                Ok(ch) => {
                                    // The server allowed this forward: drop any
                                    // earlier per-connection failure.
                                    clear_tunnel_error(&errors2, &key2);
                                    bridge(sock, ch, logs2, label2, desc).await
                                }
                                Err(e) => {
                                    let message =
                                        format!("direct-tcpip to {dh}:{dest_port} failed: {e}");
                                    push_log(
                                        &logs2,
                                        LogEntry::new(LogLevel::Error, &label2, &message),
                                    );
                                    // Keep this visible on the tunnel row:
                                    // "Running" only means listener + SSH are
                                    // up; this is a forwarding denial.
                                    record_tunnel_error(&errors2, &key2, &message);
                                }
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

/// Outcome of one connect + serve cycle.
/// `established` reports whether the forward actually came up before the
/// connection ended — used to distinguish "could not connect" retries from
/// genuine drops.
#[derive(PartialEq, Eq)]
enum CycleExit {
    /// The user stopped the tunnel (cancellation token fired).
    Cancelled,
    /// Connection lost / setup failed; `established=true` after real service.
    Lost { established: bool },
}

/// Remember the why of a failed start so the tunnel row can show it without
/// making the user open the Activity log.
fn record_tunnel_error(errors: &SharedErrors, key: &str, message: &str) {
    if let Ok(mut errors) = errors.lock() {
        errors.insert(key.to_string(), message.to_string());
    }
}

fn clear_tunnel_error(errors: &SharedErrors, key: &str) {
    if let Ok(mut errors) = errors.lock() {
        errors.remove(key);
    }
}

/// First attempt uses the listener pre-bound by `start` (so bind errors reach
/// the caller synchronously). Reconnect attempts bind a fresh listener: the
/// previous one was closed with the failed attempt.
async fn forward_listener(
    prepared: Option<std::net::TcpListener>,
    bind: &str,
) -> std::io::Result<TcpListener> {
    match prepared {
        Some(listener) => TcpListener::from_std(listener),
        None => TcpListener::bind(bind).await,
    }
}

/// Runs ONE tunnel attempt until it stops or the connection drops: connect,
/// set up the forward, serve, wait. `prepared` is a locally pre-bound
/// std listener (Local/Dynamic) so bind errors are reported synchronously to
/// the UI on the first start; reconnect attempts re-bind inside this task.
async fn run_once(
    session: &SshSession,
    forward: &PortForward,
    prepared: Option<std::net::TcpListener>,
    logs: SharedLogs,
    cancel: CancellationToken,
    phase: &Arc<AtomicU8>,
    errors: &SharedErrors,
    key: &str,
) -> CycleExit {
    let label = session.name.clone();
    let (fwd_tx, fwd_rx) = tokio::sync::mpsc::unbounded_channel::<Channel<client::Msg>>();

    let handle: SharedSshHandle = match ssh::connect(session, logs.clone(), fwd_tx).await {
        Ok(h) => Arc::new(h),
        Err(e) => {
            let message = format!("Tunnel not started: {e:#}");
            push_log(&logs, LogEntry::new(LogLevel::Error, &label, &message));
            record_tunnel_error(errors, key, &message);
            return CycleExit::Lost { established: false };
        }
    };

    let mut bg: Vec<JoinHandle<()>> = Vec::new();
    // Remote forward bookkeeping for clean release on stop.
    let mut remote_release: Option<(String, u32)> = None;
    // (target, bind_desc) for the remote dispatcher, spawned after setup.
    let mut remote_dispatch: Option<(String, String)> = None;

    match forward.fwd_type {
        ForwardType::Local => {
            let bind = format!("{}:{}", forward.listen_host, forward.listen_port);
            let listener = match forward_listener(prepared, &bind).await {
                Ok(listener) => listener,
                Err(e) => {
                    let message = format!("Cannot listen on {bind}: {e} (port in use?)");
                    push_log(&logs, LogEntry::new(LogLevel::Error, &label, &message));
                    record_tunnel_error(errors, key, &message);
                    let _ = handle
                        .disconnect(Disconnect::ByApplication, "listener setup failed", "")
                        .await;
                    return CycleExit::Lost { established: false };
                }
            };
            push_log(
                &logs,
                LogEntry::new(
                    LogLevel::Success,
                    &label,
                    &format!(
                        "Local forward ON: {bind} -> {}:{}",
                        forward.dest_host, forward.dest_port
                    ),
                ),
            );
            let (h, l, lb) = (handle.clone(), logs.clone(), label.clone());
            let (dh, dp) = (forward.dest_host.clone(), forward.dest_port as u32);
            let c = cancel.clone();
            let errs = errors.clone();
            let k = key.to_string();
            bg.push(tokio::spawn(async move {
                serve_local(listener, h, dh, dp, l, lb, bind, c, errs, k).await;
            }));
        }
        ForwardType::Dynamic => {
            let bind = format!("{}:{}", forward.listen_host, forward.listen_port);
            let listener = match forward_listener(prepared, &bind).await {
                Ok(listener) => listener,
                Err(e) => {
                    let message = format!("Cannot listen on {bind}: {e} (port in use?)");
                    push_log(&logs, LogEntry::new(LogLevel::Error, &label, &message));
                    record_tunnel_error(errors, key, &message);
                    let _ = handle
                        .disconnect(Disconnect::ByApplication, "listener setup failed", "")
                        .await;
                    return CycleExit::Lost { established: false };
                }
            };
            push_log(
                &logs,
                LogEntry::new(
                    LogLevel::Success,
                    &label,
                    &format!("Dynamic SOCKS5 ON: {bind}"),
                ),
            );
            let (h, l, lb) = (handle.clone(), logs.clone(), label.clone());
            let c = cancel.clone();
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
                    // Dispatcher spec; spawned below (it owns `fwd_rx`).
                    remote_dispatch = Some((target, bind_desc));
                }
                Err(e) => {
                    let message = format!(
                        "remote forward {bind_host}:{} denied: {e}",
                        forward.listen_port
                    );
                    push_log(&logs, LogEntry::new(LogLevel::Error, &label, &message));
                    record_tunnel_error(errors, key, &message);
                    let _ = handle
                        .disconnect(Disconnect::ByApplication, "forward denied", "")
                        .await;
                    return CycleExit::Lost { established: false };
                }
            }
        }
    }

    // SSH is authenticated and the forward is live: the row can finally say
    // "Running" (until now it was "Connecting…").
    phase.store(PHASE_RUNNING, Ordering::Relaxed);
    clear_tunnel_error(errors, key);

    // Wait for the user to stop, or the SSH connection to die. When the
    // russh connection task ends the handler (and its fwd_tx) is dropped.
    //  - Local/Dynamic: our unused `rx` then yields `None` -> drop detected.
    //  - Remote: the dispatcher owns `rx`; its JoinHandle completing signals
    //    the same thing.
    let exit = {
        // Dispatcher: bridges every server-initiated connection for a remote
        // forward to the local target. Lives outside `bg` — its completion is
        // our death signal, not something to abort.
        let mut rx_slot: Option<tokio::sync::mpsc::UnboundedReceiver<Channel<client::Msg>>> =
            Some(fwd_rx);
        let remote_dispatcher: Option<JoinHandle<()>> = remote_dispatch.map(|(target, bind_desc)| {
            let (l, lb, c) = (logs.clone(), label.clone(), cancel.clone());
            let mut rx = rx_slot.take().expect("remote dispatcher owns the rx");
            tokio::spawn(async move {
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
            })
        });
        tokio::select! {
            biased;
            _ = cancel.cancelled() => CycleExit::Cancelled,
            lost = death_signal(remote_dispatcher, rx_slot) => match lost {
                true => CycleExit::Lost { established: true },
                false => CycleExit::Cancelled,
            },
        }
    };

    for t in bg {
        t.abort();
    }
    if let Some((host, port)) = remote_release {
        let _ = handle.cancel_tcpip_forward(host, port).await;
    }
    match exit {
        CycleExit::Cancelled => {
            let _ = handle
                .disconnect(Disconnect::ByApplication, "tunnel stopped", "")
                .await;
        }
        CycleExit::Lost { .. } => {
            // The handle is most likely dead already; try a graceful
            // disconnect anyway, it is a no-op when the connection is gone.
            let _ = handle
                .disconnect(Disconnect::ByApplication, "connection dropped", "")
                .await;
        }
    }
    exit
}

/// Resolves `true` exactly once when the SSH connection dies.
/// For remote forwards the dispatcher task confirms it; otherwise the
/// forwarded-channel queue closes when the russh handler is dropped.
async fn death_signal(
    dispatcher: Option<JoinHandle<()>>,
    rx: Option<tokio::sync::mpsc::UnboundedReceiver<Channel<client::Msg>>>,
) -> bool {
    let Some(dispatcher) = dispatcher else {
        let Some(mut rx) = rx else {
            // Unreachable: either a dispatcher or the channel receiver is
            // always provided by the caller.
            return true;
        };
        return loop {
            // No forwarded channels are expected for Local/Dynamic tunnels;
            // ignore strays and keep waiting for channel close.
            match rx.recv().await {
                Some(_) => continue,
                None => break true,
            }
        };
    };
    dispatcher.await.map(|_| true).unwrap_or(true)
}

#[allow(dead_code)]

/// Supervises one tunnel for as long as the user wants it: runs
/// connect-and-serve cycles, re-binding local listeners and re-registering
/// remote forwards until the user stops — or, without auto-reconnect, until
/// the first connection drop.
async fn run_tunnel_task(
    session: SshSession,
    forward: PortForward,
    prepared: Option<std::net::TcpListener>,
    logs: SharedLogs,
    cancel: CancellationToken,
    key: String,
    auto: Arc<AtomicBool>,
    phase: Arc<AtomicU8>,
    errors: SharedErrors,
    shared_running: SharedRunningMap,
    instance: u64,
) {
    let label = session.name.clone();
    let mut prepared = prepared;
    let mut delay = RECONNECT_DELAY;

    loop {
        let exit = run_once(
            &session,
            &forward,
            prepared.take(),
            logs.clone(),
            cancel.clone(),
            &phase,
            &errors,
            &key,
        )
        .await;
        match exit {
            CycleExit::Cancelled => {
                push_log(&logs, LogEntry::new(LogLevel::Info, &label, "Tunnel stopped"));
                break;
            }
            CycleExit::Lost { established } => {
                let auto_on = auto.load(Ordering::Relaxed);
                if established {
                    push_log(
                        &logs,
                        LogEntry::new(LogLevel::Error, &label, "SSH connection dropped"),
                    );
                    // A fresh connection served nicely once; retry soon again.
                    delay = RECONNECT_DELAY;
                }
                if !auto_on {
                    push_log(
                        &logs,
                        LogEntry::new(LogLevel::Error, &label, "Tunnel stopped after connection loss"),
                    );
                    break;
                }
                phase.store(PHASE_RECONNECTING, Ordering::Relaxed);
                push_log(
                    &logs,
                    LogEntry::new(
                        LogLevel::Info,
                        &label,
                        &format!(
                            "Auto-reconnect in {delay:?}{} — listener is re-bound when the link is back",
                            if established { "" } else { " (retry)" }
                        ),
                    ),
                );
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        push_log(&logs, LogEntry::new(LogLevel::Info, &label, "Tunnel stopped"));
                        break;
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
                delay = std::cmp::min(delay * 2, MAX_RECONNECT_DELAY);
            }
        }
    }

    // Mark the tunnel as ended: the UI flips back to "stopped" even if the
    // user never pressed Stop. The instance check keeps this from clobbering
    // a freshly started replacement tunnel on the same key.
    if let Ok(mut map) = shared_running.lock() {
        if map
            .get(&key)
            .map(|t| t.instance == instance)
            .unwrap_or(false)
        {
            map.remove(&key);
        }
    }
}

// ---------------------------------------------------------------------------
// Manager: tracks running tunnels from the UI thread
// ---------------------------------------------------------------------------

/// User-visible lifecycle of one started tunnel. The entry exists as soon as
/// Start is pressed, but it only becomes `Running` once SSH is authenticated
/// and the forward is actually in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunnelPhase {
    /// SSH connecting / forward being set up (or first retry pending).
    Starting,
    /// Forward is live.
    Running,
    /// The link dropped and auto-reconnect is waiting to try again.
    Reconnecting,
}

const PHASE_STARTING: u8 = 0;
const PHASE_RUNNING: u8 = 1;
const PHASE_RECONNECTING: u8 = 2;

fn phase_from_u8(value: u8) -> TunnelPhase {
    match value {
        PHASE_RUNNING => TunnelPhase::Running,
        PHASE_RECONNECTING => TunnelPhase::Reconnecting,
        _ => TunnelPhase::Starting,
    }
}

struct RunningTunnel {
    cancel: CancellationToken,
    /// Live auto-reconnect switch (mirrors `forward.auto_reconnect`; can be
    /// flipped while the tunnel is running).
    auto: Arc<AtomicBool>,
    /// Lifecycle state, written by the supervisor task and read by the UI.
    phase: Arc<AtomicU8>,
    /// Guards against a dying supervisor removing a newer tunnel's entry.
    instance: u64,
}

type SharedRunningMap = Arc<Mutex<HashMap<String, RunningTunnel>>>;
/// Last failure per tunnel key. Kept after the supervisor exits so the row
/// can explain *why* a Start attempt ended immediately, instead of silently
/// flipping back to "Stopped" with the error only in the Activity log.
type SharedErrors = Arc<Mutex<HashMap<String, String>>>;

pub struct TunnelManager {
    rt: tokio::runtime::Handle,
    logs: SharedLogs,
    running: SharedRunningMap,
    errors: SharedErrors,
    instances: AtomicU64,
}

impl TunnelManager {
    pub fn new(rt: tokio::runtime::Handle, logs: SharedLogs) -> Self {
        Self {
            rt,
            logs,
            running: Arc::new(Mutex::new(HashMap::new())),
            errors: Arc::new(Mutex::new(HashMap::new())),
            instances: AtomicU64::new(1),
        }
    }

    pub fn is_running(&self, key: &str) -> bool {
        self.running.lock().map(|m| m.contains_key(key)).unwrap_or(false)
    }

    /// Current lifecycle state, or `None` when no supervisor is running.
    pub fn phase(&self, key: &str) -> Option<TunnelPhase> {
        let map = self.running.lock().ok()?;
        map.get(key)
            .map(|t| phase_from_u8(t.phase.load(Ordering::Relaxed)))
    }

    /// Last connection/setup failure for this key, until the next Start.
    pub fn last_error(&self, key: &str) -> Option<String> {
        self.errors.lock().ok().and_then(|m| m.get(key).cloned())
    }

    /// True while any tunnel is still connecting or waiting to reconnect, so
    /// the UI can repaint promptly when its state changes.
    pub fn any_pending(&self) -> bool {
        self.running
            .lock()
            .map(|m| {
                m.values()
                    .any(|t| t.phase.load(Ordering::Relaxed) != PHASE_RUNNING)
            })
            .unwrap_or(false)
    }

    pub fn running_count(&self) -> usize {
        self.running.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// True when any tunnel of this session is currently running.
    pub fn session_has_active(&self, session_id: &str) -> bool {
        let prefix = format!("{session_id}::");
        match self.running.lock() {
            Ok(m) => m.keys().any(|k| k.starts_with(&prefix)),
            Err(_) => false,
        }
    }

    /// Start one tunnel. Local/dynamic listeners are bound synchronously so
    /// errors like "port in use" are reported to the caller immediately.
    pub fn start(&mut self, session: &SshSession, forward: &PortForward) -> Result<(), String> {
        let key = forward_key(&session.id, &forward.id);
        if self.is_running(&key) {
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
        let auto = Arc::new(AtomicBool::new(forward.auto_reconnect));
        let phase = Arc::new(AtomicU8::new(PHASE_STARTING));
        if let Ok(mut errors) = self.errors.lock() {
            errors.remove(&key);
        }
        let instance = self.instances.fetch_add(1, Ordering::Relaxed);
        self.running.lock().map_err(|_| "internal error".to_string())?.insert(
            key.clone(),
            RunningTunnel {
                cancel: cancel.clone(),
                auto: auto.clone(),
                phase: phase.clone(),
                instance,
            },
        );
        // Detached task: it owns the supervisor loop and cleans up its own
        // manager entry when finished (guarded by the instance id).
        let _task = self.rt.spawn(run_tunnel_task(
            session.clone(),
            forward.clone(),
            prepared,
            self.logs.clone(),
            cancel,
            key,
            auto,
            phase,
            self.errors.clone(),
            self.running.clone(),
            instance,
        ));
        Ok(())
    }

    pub fn stop(&mut self, key: &str) {
        // Remove first (so the row flips to "stopped" immediately), then
        // cancel; the task's guarded cleanup sees the entry already gone.
        let entry = self.running.lock().ok().and_then(|mut m| m.remove(key));
        if let Some(r) = entry {
            r.cancel.cancel();
        }
        if let Ok(mut errors) = self.errors.lock() {
            errors.remove(key);
        }
    }

    /// Drop a saved tunnel's remembered failure (after delete).
    pub fn forget_error(&mut self, key: &str) {
        if let Ok(mut errors) = self.errors.lock() {
            errors.remove(key);
        }
    }

    /// Flip the live auto-reconnect switch of a running tunnel.
    pub fn set_auto_reconnect(&self, key: &str, on: bool) {
        if let Ok(m) = self.running.lock() {
            if let Some(t) = m.get(key) {
                t.auto.store(on, Ordering::Relaxed);
            }
        }
    }

    /// Stop every tunnel belonging to a session (e.g. before deletion).
    pub fn stop_for_session(&mut self, session_id: &str) {
        let prefix = format!("{session_id}::");
        let keys: Vec<String> = match self.running.lock() {
            Ok(m) => m
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .cloned()
                .collect(),
            Err(_) => Vec::new(),
        };
        for k in keys {
            self.stop(&k);
        }
    }
}
