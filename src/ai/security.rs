//! AI security layer: command risk classification, sensitive-path detection
//! and the redaction engine that runs before any payload is sent to an
//! external model.
//!
//! The classifier decides between three risk buckets (safe / needs approval /
//! high risk). The redactor scrubs secrets (keys, tokens, passwords, PEM
//! blocks, connection strings) from text with [`redact_text`], while
//! [`scan_text`] reports what would be redacted so the UI can gate sending.

use regex::Regex;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Command risk classification
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Risk {
    /// Read-only, executes immediately when auto-approve is on.
    Safe,
    /// State-changing; requires the user to press Execute.
    Approval,
    /// Destructive; always asks, rendered with a red warning.
    High,
}

impl Risk {
    pub fn label(&self) -> &'static str {
        match self {
            Risk::Safe => "SAFE",
            Risk::Approval => "NEEDS APPROVAL",
            Risk::High => "HIGH RISK",
        }
    }
}

pub struct Verdict {
    pub risk: Risk,
    pub reason: String,
}

fn v(risk: Risk, reason: &str) -> Verdict {
    Verdict {
        risk,
        reason: reason.to_string(),
    }
}

/// Commands that are always read-only and safe to auto-run.
fn safe_base(base: &str) -> bool {
    matches!(
        base,
        "ls" | "ll" | "la" | "cat" | "tac" | "head" | "tail" | "zcat" | "zless" | "zgrep"
            | "grep" | "egrep" | "fgrep" | "rgrep" | "find" | "du" | "df" | "free" | "ps"
            | "top" | "htop" | "uptime" | "who" | "w" | "whoami" | "id" | "groups" | "uname"
            | "hostname" | "hostnamectl" | "date" | "cal" | "wc" | "file" | "stat" | "readlink"
            | "realpath" | "findmnt" | "lsblk" | "blkid" | "netstat" | "ss" | "ip" | "lsof"
            | "sockstat" | "arp" | "route" | "traceroute" | "ping" | "dig" | "host" | "nslookup"
            | "getent" | "env" | "printenv" | "sort" | "uniq" | "cut" | "tr" | "jq" | "dmesg"
            | "journalctl" | "arch" | "lsattr" | "nproc" | "nvidia-smi" | "which" | "whereis"
            | "type" | "man" | "apropos" | "history" | "swapon"
    )
}

/// Top-level permission to run for now.
fn classify_segment(segment: &str) -> Option<Verdict> {
    let mut tokens = segment.split_whitespace().peekable();
    let mut base = tokens.next()?;
    // `/usr/bin/rm -rf` → `rm`
    base = base.rsplit('/').next().unwrap_or(base);
    let args: Vec<String> = tokens.map(|t| t.trim_matches('"').trim_matches('\'').to_string()).collect();
    let second = args.first().map(|s| s.as_str()).unwrap_or("");
    let joined = args.join(" ");

    // sudo/doas: classify the rest of the segment instead.
    if base == "sudo" || base == "doas" || base == "command" || base == "nohup" {
        return classify_segment(&joined);
    }

    // Writing through a redirection (`> file`, `>> file`).
    if let Some(cap) = redirect_re().captures(segment) {
        let target = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        if target != "/dev/null" {
            return Some(v(Risk::Approval, "writes to a file"));
        }
    }

    if base == "rm" || base == "rmdir" || base == "shred" || base == "unlink" || base == "find" && joined.contains("-delete") {
        return Some(v(Risk::High, "deletes files"));
    }
    if matches!(base, "dd" | "fdisk" | "sfdisk" | "parted" | "wipefs" | "gdisk") {
        return Some(v(Risk::High, "raw disk operation"));
    }
    if matches!(base, "shutdown" | "reboot" | "poweroff" | "halt") {
        return Some(v(Risk::High, "powers the server down"));
    }
    if base == "init" && (second == "0" || second == "6") {
        return Some(v(Risk::High, "powers the server down"));
    }
    if matches!(base, "userdel" | "groupdel" | "passwd" | "chpasswd" | "visudo" | "su") {
        return Some(v(Risk::High, "changes user accounts"));
    }
    if base == "truncate" {
        return Some(v(Risk::High, "empties a file"));
    }

    match base {
        "systemctl" => {
            return Some(match second {
                "restart" | "stop" | "start" | "disable" | "enable" | "mask" | "unmask" => {
                    v(Risk::Approval, "manages services")
                }
                _ => v(Risk::Safe, "read-only service query"),
            });
        }
        "service" => {
            // `service <name> <action>` — the action sits in third place.
            let changes = args.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "restart" | "stop" | "start" | "reload" | "force-reload" | "condrestart"
                )
            });
            return Some(if changes {
                v(Risk::Approval, "manages services")
            } else {
                v(Risk::Safe, "read-only service query")
            });
        }
        "docker" | "podman" => {
            return Some(match second {
                "rm" | "rmi" | "kill" | "system" | "prune" => {
                    v(Risk::High, "deletes Docker objects")
                }
                "volume" if joined.contains("rm") => v(Risk::High, "deletes Docker volumes"),
                "restart" | "stop" | "start" | "down" | "up" | "create" | "run" | "pull"
                | "push" | "exec" | "compose" | "volume" => v(Risk::Approval, "changes containers"),
                _ => v(Risk::Safe, "read-only Docker query"),
            });
        }
        "docker-compose" => {
            return Some(match second {
                "up" | "down" | "restart" | "stop" | "start" | "build" => {
                    v(Risk::Approval, "changes containers")
                }
                _ => v(Risk::Safe, "read-only Docker query"),
            });
        }
        "kubectl" | "oc" => {
            return Some(match second {
                "delete" | "drain" => v(Risk::High, "deletes cluster objects"),
                "rollout" | "scale" | "apply" | "create" | "patch" | "edit" | "cordon"
                | "uncordon" | "exec" | "cp" | "run" | "expose" | "set" => {
                    v(Risk::Approval, "changes cluster state")
                }
                _ => v(Risk::Safe, "read-only cluster query"),
            });
        }
        "helm" => {
            return Some(match second {
                "install" | "upgrade" | "uninstall" | "rollback" => {
                    v(Risk::Approval, "changes cluster state")
                }
                _ => v(Risk::Safe, "read-only cluster query"),
            });
        }
        "apt" | "apt-get" | "yum" | "dnf" | "apk" | "pacman" | "zypper" => {
            return Some(match second {
                "install" | "remove" | "purge" | "upgrade" | "update" | "autoremove" => {
                    v(Risk::Approval, "manages packages")
                }
                _ => v(Risk::Safe, "read-only package query"),
            });
        }
        "pip" | "pip3" | "npm" | "yarn" | "pnpm" | "gem" | "cargo" | "go" | "composer" => {
            return Some(match second {
                "install" | "add" | "remove" | "uninstall" | "update" | "upgrade" | "global" => {
                    v(Risk::Approval, "installs software")
                }
                _ => v(Risk::Safe, "read-only tooling query"),
            });
        }
        "mount" | "umount" | "swapon" | "swapoff" => {
            return Some(v(Risk::Approval, "changes mounts"));
        }
        "iptables" | "nft" | "ufw" | "firewall-cmd" => {
            return Some(v(Risk::Approval, "changes firewall rules"));
        }
        "kill" | "pkill" | "killall" => {
            return Some(v(Risk::Approval, "kills processes"));
        }
        "chmod" | "chown" | "chgrp" | "chattr" | "setfacl" | "setenforce" => {
            return Some(v(Risk::Approval, "changes permissions"));
        }
        "crontab" => {
            return Some(if joined.contains("-e") || joined.contains("-r") {
                v(Risk::Approval, "changes scheduled jobs")
            } else {
                v(Risk::Safe, "read-only cron query")
            });
        }
        "sed" => {
            return Some(if joined.contains("-i") {
                v(Risk::Approval, "edits file in place")
            } else {
                v(Risk::Safe, "read-only text filter")
            });
        }
        "tee" => {
            return Some(v(Risk::Approval, "writes to a file"));
        }
        "awk" | "gawk" | "perl" | "ruby" | "python" | "python3" | "node" => {
            return Some(v(Risk::Approval, "runs a script interpreter"));
        }
        "sysctl" if joined.contains("-w") || joined.contains('=') => {
            return Some(v(Risk::Approval, "changes kernel parameters"));
        }
        "curl" | "wget" | "scp" | "sftp" | "rsync" | "nc" | "ncat" | "telnet" => {
            return Some(v(Risk::Approval, "transfers data over the network"));
        }
        "git" => {
            return Some(match second {
                "push" if joined.contains("--force") || joined.contains("-f") => {
                    v(Risk::High, "force-pushes history")
                }
                "reset" if joined.contains("--hard") => v(Risk::High, "discards local changes"),
                "push" | "reset" | "clean" | "pull" | "fetch" | "checkout" | "clone" => {
                    v(Risk::Approval, "changes the repository")
                }
                _ => v(Risk::Safe, "read-only git query"),
            });
        }
        "mysql" | "mariadb" | "psql" | "sqlite3" | "mongo" | "mongosh" => {
            return Some(if joined.to_lowercase().contains("drop ")
                || joined.to_lowercase().contains("delete ")
                || joined.to_lowercase().contains("truncate ")
            {
                v(Risk::High, "destructive database statement")
            } else {
                v(Risk::Approval, "database access")
            });
        }
        _ => {}
    }

    if safe_base(base) {
        Some(v(Risk::Safe, "read-only command"))
    } else {
        Some(v(Risk::Approval, "not in the auto-execute list"))
    }
}

fn redirect_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|\s)>>?\s*(\S+)").unwrap())
}

/// Classify a full shell command line. The whole line is broken into
/// `;`/`|`/`&`/newline segments and the worst verdict wins.
pub fn classify_command(cmd: &str) -> Verdict {
    let low = cmd.to_lowercase();
    // Destructive SQL / shell phrases anywhere in the line.
    for phrase in [
        "drop database",
        "drop table",
        "delete from",
        "truncate table",
        "mkfs.",
        ":(){",
        "rm -rf /",
        "rm -fr /",
        "> /dev/sd",
    ] {
        if low.contains(phrase) {
            return v(Risk::High, "destructive operation");
        }
    }
    let mut worst = v(Risk::Safe, "read-only command");
    for segment in cmd.split([';', '|', '&', '\n']) {
        let seg = segment.trim();
        if seg.is_empty() {
            continue;
        }
        if let Some(verdict) = classify_segment(seg) {
            if verdict.risk == Risk::High {
                return verdict;
            }
            if verdict.risk == Risk::Approval {
                worst = verdict;
            }
        }
    }
    worst
}

// ---------------------------------------------------------------------------
// Sensitive path detection
// ---------------------------------------------------------------------------

/// Why a path looks sensitive, or `None` when it is ordinary.
pub fn sensitive_path(path: &str) -> Option<String> {
    let name = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim()
        .to_lowercase();
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    let is_pub = name.ends_with(".pub");

    if matches!(ext.as_str(), "pem" | "key" | "p12" | "pfx" | "jks" | "keystore" | "ppk") {
        return Some(format!("credential material (*.{ext})"));
    }
    if matches!(ext.as_str(), "crt" | "cer") {
        return Some("certificate".to_string());
    }
    if !is_pub
        && (name == "id_rsa"
            || name == "id_dsa"
            || name == "id_ecdsa"
            || name == "id_ed25519"
            || name.starts_with("id_rsa.")
            || name.starts_with("id_ed25519."))
    {
        return Some("SSH private key".to_string());
    }
    if name == ".env" || name.starts_with(".env") {
        return Some("environment file (often holds secrets)".to_string());
    }
    if name == "kubeconfig"
        || name.ends_with("kubeconfig")
        || name.contains("kubeconfig")
        || path.contains("/.kube/")
        || path.ends_with(".kube/config")
    {
        return Some("kubeconfig".to_string());
    }
    if name == ".netrc" || name == "_netrc" {
        return Some("netrc credentials".to_string());
    }
    if name == "credentials" || name == "config.json" && path.to_lowercase().contains(".docker") {
        return Some("cloud/registry credentials".to_string());
    }
    if name.ends_with(".tfstate") {
        return Some("terraform state (holds secrets)".to_string());
    }
    if name.contains("secret") && (ext == "yaml" || ext == "yml" || ext == "json" || ext == "env") {
        return Some("secrets file".to_string());
    }
    if ext == "ovpn" {
        return Some("OpenVPN profile (holds keys)".to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// Redaction engine
// ---------------------------------------------------------------------------

pub struct Hit {
    pub kind: String,
    pub count: usize,
    /// First match, masked for display.
    pub sample: String,
}

struct Pattern {
    re: Regex,
    kind: &'static str,
    replacement: &'static str,
}

fn mask(s: &str) -> String {
    let cut: String = s.chars().take(48).collect();
    if s.chars().count() > 48 {
        format!("{cut}…")
    } else {
        cut
    }
}

fn patterns() -> &'static Vec<Pattern> {
    static PATS: OnceLock<Vec<Pattern>> = OnceLock::new();
    PATS.get_or_init(|| {
        let mut out = Vec::new();
        let mut add = |re: &str, kind: &'static str, replacement: &'static str| {
            out.push(Pattern {
                re: Regex::new(re).unwrap(),
                kind,
                replacement,
            });
        };
        add(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", "private key", "[REDACTED PRIVATE KEY]");
        add(r"(?s)-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----", "certificate", "[REDACTED CERTIFICATE]");
        add(r#"(?i)(password|passwd|pwd)(\s*[=:]\s*)("[^"]{1,256}"|'[^']{1,256}'|[^\s]{1,128})"#, "password", "$1$2[REDACTED]");
        add(r#"(?i)(api[_-]?key|apikey|access[_-]?key|secret[_-]?key|client[_-]?secret|auth[_-]?token|access[_-]?token|refresh[_-]?token|session[_-]?token|private[_-]?key|passphrase)(\s*[=:]\s*)("[^"]{1,256}"|'[^']{1,256}'|[^\s]{1,256})"#, "credential", "$1$2[REDACTED]");
        add(r"(?i)(secret)(\s*[=:]\s*)([^\s]{4,256})", "secret", "$1$2[REDACTED]");
        add(r"(?i)(token)(\s*[=:]\s*)([^\s]{8,512})", "token", "$1$2[REDACTED]");
        add(r"AKIA[0-9A-Z]{16}", "AWS access key", "[REDACTED AWS KEY]");
        add(r"(?i)(aws_secret_access_key)(\s*[=:]\s*)([^\s]+)", "AWS secret", "$1$2[REDACTED]");
        add(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b", "JWT", "[REDACTED JWT]");
        add(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]{16,}", "bearer token", "$1[REDACTED]");
        add(r"(?i)\b(postgres(?:ql)?|mysql|mariadb|mongodb(?:\+srv)?|redis|amqp|rabbitmq|mssql|ftp)://([^:@\s/]+):([^@\s]+)@", "connection string", "$1://$2:[REDACTED]@");
        add(r"(?i)\b(hvs|hvb)\.[A-Za-z0-9_-]{16,}", "Vault token", "[REDACTED VAULT TOKEN]");
        add(r"(?i)AccountKey=[A-Za-z0-9+/=]{16,}", "Azure storage key", "AccountKey=[REDACTED]");
        add(r"(?i)(client-key-data|client-certificate-data)(\s*:\s*)([A-Za-z0-9+/=]{16,})", "kubeconfig data", "$1$2[REDACTED]");
        add(r"(?i)(client-key-data|client-certificate-data):\s*[A-Za-z0-9+/=]{16,}", "kubeconfig data", "[REDACTED]");
        add(r"\bghp_[A-Za-z0-9]{20,}\b", "GitHub token", "[REDACTED TOKEN]");
        add(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b", "GitHub token", "[REDACTED TOKEN]");
        add(r"\bxox[bap]-[A-Za-z0-9-]{10,}\b", "Slack token", "[REDACTED TOKEN]");
        add(r"\bsk-[A-Za-z0-9_-]{20,}\b", "API key", "[REDACTED API KEY]");
        add(r"\b(sshpass)(\s+-p\s*)(\S+)", "sshpass password", "$1$2[REDACTED]");
        out
    })
}

/// What the redactor would mask in this text. Empty = nothing sensitive.
pub fn scan_text(text: &str) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    for p in patterns() {
        let matches: Vec<_> = p.re.find_iter(text).collect();
        if matches.is_empty() {
            continue;
        }
        let sample = mask(matches[0].as_str());
        // Merge with an existing hit of the same kind.
        if let Some(h) = hits.iter_mut().find(|h| h.kind == p.kind) {
            h.count += matches.len();
        } else {
            hits.push(Hit {
                kind: p.kind.to_string(),
                count: matches.len(),
                sample,
            });
        }
    }
    hits
}

/// Replace every sensitive finding with `[REDACTED …]` markers.
/// Returns the scrubbed text and how many substitutions ran.
pub fn redact_text(text: &str) -> (String, usize) {
    let mut cur = text.to_string();
    let mut n = 0usize;
    for p in patterns() {
        if !p.re.is_match(&cur) {
            continue;
        }
        let replaced = p.re.replace_all(&cur, p.replacement);
        n += p.re.find_iter(&cur).count();
        cur = replaced.into_owned();
    }
    (cur, n)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_commands_pass() {
        for cmd in [
            "ls -la /var/log",
            "df -h /",
            "grep -i error /var/log/nginx/error.log | tail -50",
            "docker ps -a",
            "kubectl get pods",
            "systemctl status nginx",
            "cat /etc/nginx/nginx.conf",
            "journalctl -u nginx --since '1 hour ago' -n 100",
        ] {
            assert_eq!(classify_command(cmd).risk, Risk::Safe, "{cmd}");
        }
    }

    #[test]
    fn service_changes_need_approval() {
        for cmd in [
            "systemctl restart nginx",
            "service nginx restart",
            "docker restart web",
            "kubectl rollout restart deployment/web",
            "apt-get install nginx",
            "chmod 777 /tmp/x",
            "sudo systemctl stop sshd",
        ] {
            assert_eq!(classify_command(cmd).risk, Risk::Approval, "{cmd}");
        }
    }

    #[test]
    fn destructive_commands_are_high() {
        for cmd in [
            "rm -rf /var/log",
            "rm file.txt",
            "shutdown -h now",
            "reboot",
            "mkfs.ext4 /dev/sda1",
            "userdel ghost",
            "mysql -e 'drop database prod'",
            "dd if=/dev/zero of=/dev/sda",
            "git push --force origin main",
        ] {
            assert_eq!(classify_command(cmd).risk, Risk::High, "{cmd}");
        }
    }

    #[test]
    fn unknown_commands_need_approval() {
        assert_eq!(classify_command("frobnicate --all").risk, Risk::Approval);
        assert_eq!(classify_command("curl https://example.com").risk, Risk::Approval);
    }

    #[test]
    fn redirects_need_approval_but_devnull_does_not() {
        assert_eq!(classify_command("echo hi > /tmp/x").risk, Risk::Approval);
        assert_eq!(classify_command("grep x /etc/hosts > /dev/null").risk, Risk::Safe);
    }

    #[test]
    fn redacts_password_lines() {
        let (out, n) = redact_text("password=hunter2\nuser: admin");
        assert!(n >= 1);
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_bearer_and_connection_strings() {
        let (out, _) = redact_text("Authorization: Bearer abcdefghijklmnop12345\nDB=postgres://user:s3cret@db:5432/app");
        assert!(!out.contains("abcdefghijklmnop12345"), "{out}");
        assert!(!out.contains("s3cret"), "{out}");
    }

    #[test]
    fn redacts_private_key_blocks() {
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nabc123\n-----END OPENSSH PRIVATE KEY-----";
        let (out, n) = redact_text(key);
        assert_eq!(n, 1);
        assert!(!out.contains("abc123"));
    }

    #[test]
    fn detects_sensitive_paths() {
        assert!(sensitive_path("/home/u/.ssh/id_rsa").is_some());
        assert!(sensitive_path("/srv/app/.env.production").is_some());
        assert!(sensitive_path("/etc/ssl/private/site.pem").is_some());
        assert!(sensitive_path("~/.kube/config").is_some());
        assert!(sensitive_path("/var/log/nginx/access.log").is_none());
        assert!(sensitive_path("/etc/nginx/nginx.conf").is_none());
        assert!(sensitive_path("/home/u/.ssh/id_rsa.pub").is_none());
    }
}
