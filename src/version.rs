//! Build-stamped version identity, shared by the window title, sidebar brand
//! row and status bar so every display shows the same version string.
//!
//! The displayed version advances on every build: the patch number is the
//! monotonic build counter (git commit count), so `v0.3.17` for commit 17,
//! `v0.3.18` after the next commit, and so on. The static `0.3.0` in
//! Cargo.toml is never shown verbatim again.

/// Monotonic build number — increments with every new build of changed code.
pub fn build_num() -> &'static str {
    env!("CULVERT_BUILD_NUM")
}

/// Short commit hash this build was cut from.
pub fn build_commit() -> &'static str {
    env!("CULVERT_BUILD_COMMIT")
}

/// UTC compile timestamp (`YYYY-MM-DD HH:MM`).
pub fn build_time() -> &'static str {
    env!("CULVERT_BUILD_TIME")
}

/// Display version string, e.g. `0.3.17` — major/minor come from Cargo.toml,
/// patch is the per-build counter so the number never repeats a stalled
/// `0.3.0`.
pub fn version() -> String {
    let sem = env!("CARGO_PKG_VERSION");
    let mut sem = sem.split('.');
    let major = sem.next().unwrap_or("0");
    let minor = sem.next().unwrap_or("3");
    format!("{}.{}.{}", major, minor, build_num())
}

/// Compact tag used in the title bar, sidebar and status bar.
pub fn tag() -> String {
    format!("v{}", version())
}

/// Full identity for the About/profile dialog, e.g.
/// `v0.3.17 (9d27564 · 2026-10-18 13:05 UTC)`.
pub fn display() -> String {
    format!("v{} ({} · {} UTC)", version(), build_commit(), build_time())
}
