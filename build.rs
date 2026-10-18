//! Build stamping: every compiled binary gets a unique release identity so
//! the title bar EXACTLY echoes the version that was compiled, instead of a
//! stale Cargo.toml version number.
//!
//! Emitted as rustc-env values:
//! * CULVERT_BUILD_NUM    — monotonic number (git commit count; increments
//!   with every new commit = every new build of changed code)
//! * CULVERT_BUILD_COMMIT — short commit hash the build was cut from
//! * CULVERT_BUILD_TIME   — UTC "YYYY-MM-DD HH:MM" of the actual compile
//!
//! No new dependencies: the date is formatted by hand (Howard Hinnant's
//! civil-from-days algorithm) and git is queried through Command.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let (count, commit) = git_identity();
    println!("cargo:rustc-env=CULVERT_BUILD_NUM={}", count);
    println!("cargo:rustc-env=CULVERT_BUILD_COMMIT={}", commit);
    println!("cargo:rustc-env=CULVERT_BUILD_TIME={}", utc_stamp());
}

/// Commits on HEAD; falls back to hours-since-epoch when git is unavailable
/// so the number still advances (and stays roughly sortable) per rebuild.
fn git_identity() -> (u64, String) {
    let run = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let count = run(&["rev-list", "--count", "HEAD"])
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() / 3600).unwrap_or(0));
    let commit = run(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "nogit".into());
    (count, commit)
}

/// UTC wall clock at compile time, "YYYY-MM-DD HH:MM" — no chrono needed.
fn utc_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

/// Howard Hinnant's civil_from_days: days since 1970-01-01 → (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era [0, 146095]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year of era
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
