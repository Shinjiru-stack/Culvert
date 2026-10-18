//! Build-stamped version identity, shared by the window title, sidebar brand
//! row and status bar so every display shows the same version string.

/// Semantic version from Cargo.toml (`0.3.0`).
pub fn semver() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

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

/// Compact tag used in the title bar, sidebar and status bar.
/// Changes on every build, e.g. `v0.3.0-b142`.
pub fn tag() -> String {
    format!("v{}-b{}", semver(), build_num())
}

/// Full identity for the About/profile dialog, e.g.
/// `v0.3.0 (build 142 · 9d27564 · 2026-10-18 09:12 UTC)`.
pub fn display() -> String {
    format!(
        "v{} (build {} · {} · {} UTC)",
        semver(),
        build_num(),
        build_commit(),
        build_time()
    )
}

