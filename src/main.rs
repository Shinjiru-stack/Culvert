#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod filebrowser;
mod log;
mod session;
mod ssh;
mod terminal;
mod theme;
mod tunnel;
mod version;

use app::TunnelApp;
use log::{LogEntry, LogLevel, SharedLogs};
use std::sync::{Arc, Mutex};

fn main() -> eframe::Result<()> {
    // Tokio runtime lives for the whole GUI lifetime so tunnels keep forwarding
    // while the window is open.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("culvert-rt")
        .build()
        .expect("cannot start tokio runtime");
    let rt_handle = rt.handle().clone();
    // Leak the runtime intentionally: eframe::run_native blocks until exit,
    // and background tasks need the runtime alive that whole time.
    std::mem::forget(rt);

    let logs: SharedLogs = Arc::new(Mutex::new(vec![LogEntry::new(
        LogLevel::Info,
        "app",
        "Culvert ready. Double-click a saved session for a shell; manage tunnels on the Tunnels tab.",
    )]));

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1300.0, 840.0])
            .with_min_inner_size([1020.0, 660.0])
            .with_title(format!("Culvert {}", version::tag()))
            .with_app_id("culvert")
            .with_icon(Arc::new(theme::app_icon(256))),
        ..Default::default()
    };

    let logs2 = logs.clone();
    eframe::run_native(
        "culvert",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(TunnelApp::new(cc, rt_handle.clone(), logs2.clone())))
        }),
    )
}
