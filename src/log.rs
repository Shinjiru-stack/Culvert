//! Shared activity log — written by tunnel tasks, terminal sessions and the
//! UI, shown in the "Activity" modal.

use chrono::Local;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub enum LogLevel {
    Info,
    Success,
    Error,
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub time: String,
    pub level: LogLevel,
    pub session: String,
    pub message: String,
}

impl LogEntry {
    pub fn new(level: LogLevel, session: &str, message: &str) -> Self {
        Self {
            time: Local::now().format("%H:%M:%S").to_string(),
            level,
            session: session.to_string(),
            message: message.to_string(),
        }
    }
}

pub type SharedLogs = Arc<Mutex<Vec<LogEntry>>>;

pub fn push_log(logs: &SharedLogs, entry: LogEntry) {
    if let Ok(mut l) = logs.lock() {
        l.push(entry);
        if l.len() > 2000 {
            let excess = l.len() - 2000;
            l.drain(0..excess);
        }
    }
}
