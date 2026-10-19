//! What Culvert has learned about a host: its OS and the package manager it
//! really has.
//!
//! A fresh chat starts with no history, so a model asked to "install htop"
//! guesses — usually apt, even on the Alpine boxes where it cannot exist. The
//! probe hints ([`super::tools`]) already correct that *after* a failure; this
//! book makes the first guess right by remembering what the probes found and
//! feeding it back into the system prompt. Facts are a hint, not gospel: a
//! host can be rebuilt, and a stale note is corrected by the next probe.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HostFacts {
    /// Distribution as reported by `/etc/os-release` (e.g. "Alpine Linux v3.24").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Package manager binary the host has (e.g. "apk").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pm: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostBook {
    #[serde(default)]
    pub hosts: BTreeMap<String, HostFacts>,
}

impl HostBook {
    pub fn file() -> PathBuf {
        crate::ai::config::ai_config_file().with_file_name("hosts.json")
    }

    pub fn load() -> Self {
        std::fs::read_to_string(Self::file())
            .ok()
            .and_then(|raw| serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = Self::file();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }

    pub fn facts(&self, host: &str) -> Option<&HostFacts> {
        self.hosts.get(host).filter(|f| f.os.is_some() || f.pm.is_some())
    }

    /// What the model should be told about this host, if anything.
    pub fn note(&self, host: &str) -> Option<String> {
        let f = self.facts(host)?;
        Some(match (&f.os, &f.pm) {
            (Some(os), Some(pm)) => format!("{os} · package manager {pm}"),
            (Some(os), None) => format!("{os} (package manager unknown)"),
            (None, Some(pm)) => format!("package manager {pm}"),
            (None, None) => return None,
        })
    }

    /// Record the package manager a probe found. `label` is the hint label
    /// ("apk (Alpine)"): its parenthesised part is a coarse OS family, kept
    /// only when `/etc/os-release` has not been read yet.
    pub fn learn_pm(&mut self, host: &str, label: &str) -> bool {
        let (pm, family) = match label.split_once(" (") {
            Some((pm, rest)) => (pm.trim().to_string(), rest.trim_end_matches(')').to_string()),
            None => (label.trim().to_string(), String::new()),
        };
        if pm.is_empty() {
            return false;
        }
        let mut changed = false;
        {
            let f = self.hosts.entry(host.to_string()).or_default();
            if f.pm.as_deref() != Some(pm.as_str()) {
                f.pm = Some(pm);
                changed = true;
            }
            if f.os.is_none() && !family.is_empty() {
                f.os = Some(family);
                changed = true;
            }
        }
        // Drop empty entries so hosts.json stays readable.
        if self.hosts.get(host).is_some_and(|f| f.os.is_none() && f.pm.is_none()) {
            self.hosts.remove(host);
        }
        changed
    }

    /// Learn from `cat /etc/os-release` output (or anything containing it).
    pub fn learn_os_release(&mut self, host: &str, text: &str) -> bool {
        let Some(name) = os_release_value(text, "PRETTY_NAME") else {
            return false;
        };
        let pm = os_release_value(text, "ID")
            .and_then(|id| pm_for_os_id(&id))
            .map(str::to_string);
        let mut changed = false;
        let f = self.hosts.entry(host.to_string()).or_default();
        if f.os.as_deref() != Some(name.as_str()) {
            f.os = Some(name);
            changed = true;
        }
        if let Some(pm) = pm {
            if f.pm.as_deref() != Some(pm.as_str()) {
                f.pm = Some(pm);
                changed = true;
            }
        }
        changed
    }

    /// Learn from any tool result: our package-manager hints name the manager
    /// ("Detected package manager(s): apk (Alpine)"), and a listing may contain
    /// os-release text.
    pub fn learn_from_result(&mut self, host: &str, result: &str) -> bool {
        let mut changed = false;
        if self.learn_os_release(host, result) {
            changed = true;
        }
        if let Some(list) = result
            .split("Detected package manager(s):")
            .nth(1)
            .and_then(|rest| rest.split('.').next())
        {
            if let Some(first) = list.split(',').next() {
                if self.learn_pm(host, first.trim()) {
                    changed = true;
                }
            }
        }
        changed
    }
}

/// `PRETTY_NAME="Alpine Linux v3.24"` → `Alpine Linux v3.24`.
fn os_release_value(text: &str, key: &str) -> Option<String> {
    let line = text.lines().find(|l| {
        l.trim_start()
            .starts_with(&format!("{key}="))
    })?;
    let value = line.split_once('=')?.1.trim();
    let value = value.trim_matches('"').trim_matches('\'').trim();
    (!value.is_empty() && !value.contains('$')).then(|| value.to_string())
}

/// Which manager a distribution id implies.
fn pm_for_os_id(id: &str) -> Option<&'static str> {
    let id = id.trim_matches('"').trim().to_ascii_lowercase();
    Some(match id.as_str() {
        "alpine" => "apk",
        "debian" | "ubuntu" | "linuxmint" | "pop" | "raspbian" => "apt-get",
        "fedora" | "rhel" | "rocky" | "almalinux" | "ol" => "dnf",
        "centos" => "yum",
        "opensuse" | "opensuse-leap" | "opensuse-tumbleweed" | "sles" => "zypper",
        "arch" | "manjaro" | "endeavouros" => "pacman",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_the_package_manager_from_a_hint_label() {
        let mut book = HostBook::default();
        assert!(book.learn_pm("10.0.0.5", "apk (Alpine)"));
        assert_eq!(book.facts("10.0.0.5").unwrap().pm.as_deref(), Some("apk"));
        assert_eq!(book.facts("10.0.0.5").unwrap().os.as_deref(), Some("Alpine"));
        // Re-learning the same fact is not a change (no pointless disk write).
        assert!(!book.learn_pm("10.0.0.5", "apk (Alpine)"));
        assert_eq!(
            book.note("10.0.0.5").unwrap(),
            "Alpine · package manager apk"
        );
        assert!(book.note("other-host").is_none());
    }

    #[test]
    fn learns_from_tool_results_and_os_release() {
        let mut book = HostBook::default();
        let hint = "[exit 1]\nsudo: apt-get: command not found\n[hint] `apt-get` is not installed on this host. Detected package manager(s): apk (Alpine). Retry with `sudo apk add <package>` instead of guessing.";
        assert!(book.learn_from_result("10.0.0.5", hint));
        assert_eq!(book.facts("10.0.0.5").unwrap().pm.as_deref(), Some("apk"));

        let os_release = "NAME=\"Alpine Linux\"\nID=alpine\nVERSION_ID=3.24.1\nPRETTY_NAME=\"Alpine Linux v3.24\"\n";
        assert!(book.learn_os_release("10.0.0.5", os_release));
        let f = book.facts("10.0.0.5").unwrap();
        assert_eq!(f.os.as_deref(), Some("Alpine Linux v3.24"));
        assert_eq!(f.pm.as_deref(), Some("apk"));

        // Something without os-release or our hint leaves the book alone.
        assert!(!book.learn_from_result("10.0.0.5", "total 4\ndrwxr-xr-x 2 root root"));
        assert!(!book.learn_from_result("fresh-host", "hello"));
        assert!(book.facts("fresh-host").is_none());
    }

    #[test]
    fn maps_distribution_ids_to_package_managers() {
        assert_eq!(pm_for_os_id("alpine"), Some("apk"));
        assert_eq!(pm_for_os_id("\"ubuntu\""), Some("apt-get"));
        assert_eq!(pm_for_os_id("rocky"), Some("dnf"));
        assert_eq!(pm_for_os_id("centos"), Some("yum"));
        assert_eq!(pm_for_os_id("sles"), Some("zypper"));
        assert_eq!(pm_for_os_id("manjaro"), Some("pacman"));
        assert_eq!(pm_for_os_id("gentoo"), None);
        // Variables and empty values are not names to remember.
        assert_eq!(os_release_value("PRETTY_NAME=$PRETTY_NAME\n", "PRETTY_NAME"), None);
        assert_eq!(os_release_value("ID=alpine\n", "PRETTY_NAME"), None);
    }
}
