//! Windows DPAPI protection for the secret fields in Culvert's config files.
//!
//! Saved passwords, key passphrases and the AI provider API key are encrypted
//! with `CryptProtectData` (the same per-user store Windows uses for its own
//! credentials). The ciphertext is bound to the current Windows account (and
//! machine) — another user, or the same file on another computer, cannot
//! decrypt it. Everything else in the JSON stays readable: only the fields
//! listed in [`SECRET_FIELDS`] are sealed, as `dpapi:v1:<hex>`.
//!
//! Legacy plaintext values keep working: they pass through unchanged on load,
//! and the loaders immediately rewrite the file in the encrypted form.

use serde_json::Value;

/// Marker prefix for DPAPI-sealed values.
const PREFIX: &str = "dpapi:v1:";

/// JSON fields that hold secrets, wherever they appear in the document.
pub const SECRET_FIELDS: &[&str] = &["password", "key_passphrase", "api_key"];

/// True when a stored value is already sealed.
pub fn is_protected(s: &str) -> bool {
    s.starts_with(PREFIX)
}

/// Seal `plain` for the current user. Empty values and already-sealed values
/// are returned untouched; if the OS call fails the input is stored as-is
/// (never silently lose a credential the user typed).
pub fn protect(plain: &str) -> String {
    if plain.is_empty() || is_protected(plain) {
        return plain.to_string();
    }
    match imp::protect(plain.as_bytes()) {
        Some(blob) => format!("{PREFIX}{}", hex(&blob)),
        None => plain.to_string(),
    }
}

/// Open a stored value. Plaintext (legacy) values pass through, so old
/// config files keep working until their next save.
pub fn unprotect(stored: &str) -> String {
    let Some(encoded) = stored.strip_prefix(PREFIX) else {
        return stored.to_string();
    };
    let Some(bytes) = unhex(encoded) else {
        return String::new();
    };
    imp::unprotect(&bytes)
        .and_then(|plain| String::from_utf8(plain).ok())
        .unwrap_or_default()
}

/// Seal every [`SECRET_FIELDS`] string in a JSON document.
pub fn protect_value(value: &mut Value) {
    walk(value, &mut |v| {
        if let Value::String(s) = v {
            *s = protect(s);
        }
    });
}

/// Open every secret field. Returns `true` when legacy plaintext secrets were
/// found — the caller can then rewrite the file encrypted.
pub fn unprotect_value(value: &mut Value) -> bool {
    let mut migrated = false;
    walk(value, &mut |v| {
        if let Value::String(s) = v {
            if !s.is_empty() && !is_protected(s) {
                migrated = true;
            }
            *s = unprotect(s);
        }
    });
    migrated
}

fn walk(value: &mut Value, f: &mut dyn FnMut(&mut Value)) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if SECRET_FIELDS.contains(&key.as_str()) {
                    f(v);
                } else {
                    walk(v, f);
                }
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                walk(v, f);
            }
        }
        _ => {}
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::ptr;

    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;
    /// App-specific entropy: a small extra secret mixed into the key, so a
    /// generic DPAPI dump tool cannot open Culvert's blob by accident.
    const ENTROPY: &[u8] = b"culvert/secrets/v1";

    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            data_in: *const DataBlob,
            description: *const u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;
        fn CryptUnprotectData(
            data_in: *const DataBlob,
            description: *mut *mut u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    fn blob(bytes: &[u8]) -> DataBlob {
        DataBlob {
            cb_data: bytes.len() as u32,
            pb_data: bytes.as_ptr() as *mut u8,
        }
    }

    /// Copy a returned blob into a Vec and release the OS allocation.
    unsafe fn take(out: DataBlob) -> Option<Vec<u8>> {
        if out.pb_data.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(out.pb_data, out.cb_data as usize).to_vec();
        LocalFree(out.pb_data as *mut c_void);
        Some(bytes)
    }

    pub fn protect(plain: &[u8]) -> Option<Vec<u8>> {
        let entropy = blob(ENTROPY);
        let input = blob(plain);
        let mut out = DataBlob {
            cb_data: 0,
            pb_data: ptr::null_mut(),
        };
        let ok = unsafe {
            CryptProtectData(
                &input,
                ptr::null(),
                &entropy,
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return None;
        }
        unsafe { take(out) }
    }

    pub fn unprotect(sealed: &[u8]) -> Option<Vec<u8>> {
        let entropy = blob(ENTROPY);
        let input = blob(sealed);
        let mut out = DataBlob {
            cb_data: 0,
            pb_data: ptr::null_mut(),
        };
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                &entropy,
                ptr::null_mut(),
                ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return None;
        }
        unsafe { take(out) }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn protect(_plain: &[u8]) -> Option<Vec<u8>> {
        None
    }
    pub fn unprotect(_sealed: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_plaintext_passes_through() {
        assert_eq!(unprotect("hunter2"), "hunter2");
        assert!(!is_protected("hunter2"));
    }

    #[test]
    fn walks_only_secret_fields() {
        let mut doc = json!({
            "name": "prod",
            "password": "s3cret",
            "nested": [{ "key_passphrase": "pp" }, { "host": "example.com" }],
            "api_key": "",
        });
        protect_value(&mut doc);
        // Non-secret fields untouched; empty secrets stay empty.
        assert_eq!(doc["name"], "prod");
        assert_eq!(doc["nested"][1]["host"], "example.com");
        assert_eq!(doc["api_key"], "");
        // The plaintext no longer appears anywhere in the document.
        assert!(!doc.to_string().contains("s3cret"));
        assert!(!doc.to_string().contains("\"pp\"") || is_protected(doc["nested"][0]["key_passphrase"].as_str().unwrap()));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip() {
        let secret = "hunter2 · ünïcode";
        let sealed = protect(secret);
        assert!(is_protected(&sealed), "should be sealed: {sealed}");
        assert!(!sealed.contains("hunter2"));
        assert_eq!(unprotect(&sealed), secret);
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_two_seals_differ() {
        // DPAPI salts every call: identical secrets must not produce
        // identical ciphertext, or the file leaks which fields match.
        let a = protect("same-password");
        let b = protect("same-password");
        assert_ne!(a, b);
        assert_eq!(unprotect(&a), unprotect(&b));
    }
}
