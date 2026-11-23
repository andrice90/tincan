//! Settings: defaults, validation and persistence (one JSON document in the store's
//! settings table). The display name is NOT part of it (own kv key, see `profile`).

use std::path::{Path, PathBuf};

use tincan_store::SqliteStorage;

use crate::dto::{Settings, Theme};

/// Attachments larger than this cannot be sent or downloaded (architecture.md; tincan-files
/// enforces the same cap on downloads).
pub(crate) const MAX_ATTACHMENT_BYTES: u64 = tincan_files::MAX_ATTACHMENT_BYTES;

impl Default for Settings {
    fn default() -> Self {
        Self {
            input_device_id: None,
            output_device_id: None,
            noise_suppression: true,
            echo_cancellation: true,
            input_volume: 1.0,
            output_volume: 1.0,
            auto_fetch_limit: MAX_ATTACHMENT_BYTES,
            download_dir: default_download_dir().to_string_lossy().into_owned(),
            allow_hosting: true,
            custom_relay_url: None,
            theme: Theme::System,
        }
    }
}

/// `%USERPROFILE%\Downloads\Tincan` (falls back to the current directory's `Downloads\Tincan`).
pub(crate) fn default_download_dir() -> PathBuf {
    let base = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("Downloads").join("Tincan")
}

/// Load the stored settings, or the defaults (also when the stored document is unreadable).
pub(crate) fn load(storage: &SqliteStorage) -> Settings {
    match storage.get_settings::<Settings>() {
        Ok(Some(s)) => normalize(s).unwrap_or_default(),
        Ok(None) => Settings::default(),
        Err(e) => {
            tracing::warn!(error = %e, "stored settings unreadable; using defaults");
            Settings::default()
        }
    }
}

/// Persist settings.
pub(crate) fn save(storage: &SqliteStorage, s: &Settings) -> Result<(), String> {
    storage.set_settings(s).map_err(|e| {
        tracing::error!(error = %e, "saving settings failed");
        "Couldn't save the settings. Try again.".to_string()
    })
}

/// Validate and clean user input. Errors are user-facing.
pub(crate) fn normalize(mut s: Settings) -> Result<Settings, String> {
    for (v, what) in [(&mut s.input_volume, "Input"), (&mut s.output_volume, "Output")] {
        if !v.is_finite() {
            return Err(format!("{what} volume must be a number between 0 and 2."));
        }
        *v = v.clamp(0.0, 2.0);
    }
    s.auto_fetch_limit = s.auto_fetch_limit.min(MAX_ATTACHMENT_BYTES);
    let dir = s.download_dir.trim().to_string();
    s.download_dir = if dir.is_empty() { default_download_dir().to_string_lossy().into_owned() } else { dir };
    if !Path::new(&s.download_dir).is_absolute() {
        return Err("Pick a full folder path for downloads.".into());
    }
    s.custom_relay_url = match s.custom_relay_url.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(url) => {
            if !valid_relay_url(url) {
                return Err("The relay URL must start with https:// (for example https://relay.example.com).".into());
            }
            Some(url.to_string())
        }
    };
    for id in [&mut s.input_device_id, &mut s.output_device_id] {
        if id.as_deref().is_some_and(|v| v.trim().is_empty()) {
            *id = None;
        }
    }
    Ok(s)
}

/// A plausible relay URL: http(s) scheme, a host, no whitespace.
pub(crate) fn valid_relay_url(url: &str) -> bool {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
    match rest {
        Some(rest) => {
            let host = rest.split(['/', '?', '#']).next().unwrap_or("");
            !host.is_empty() && !url.chars().any(char::is_whitespace) && url.len() <= 2048
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let s = Settings::default();
        assert!(s.noise_suppression && s.echo_cancellation && s.allow_hosting);
        assert_eq!(s.auto_fetch_limit, MAX_ATTACHMENT_BYTES);
        assert!(s.download_dir.ends_with("Tincan"));
        assert_eq!(normalize(s.clone()).unwrap(), s);
    }

    #[test]
    fn normalize_clamps_and_rejects() {
        let mut s = Settings { input_volume: 5.0, output_volume: -1.0, auto_fetch_limit: u64::MAX, ..Settings::default() };
        s.custom_relay_url = Some("  ".into());
        let n = normalize(s).unwrap();
        assert_eq!((n.input_volume, n.output_volume), (2.0, 0.0));
        assert_eq!(n.auto_fetch_limit, MAX_ATTACHMENT_BYTES);
        assert_eq!(n.custom_relay_url, None);
        let bad = Settings { custom_relay_url: Some("relay.example.com".into()), ..Settings::default() };
        assert!(normalize(bad).is_err());
        let good = Settings { custom_relay_url: Some(" https://relay.example.com/ ".into()), ..Settings::default() };
        assert_eq!(normalize(good).unwrap().custom_relay_url.as_deref(), Some("https://relay.example.com/"));
        let nan = Settings { input_volume: f64::NAN, ..Settings::default() };
        assert!(normalize(nan).is_err());
        let rel = Settings { download_dir: "downloads".into(), ..Settings::default() };
        assert!(normalize(rel).is_err());
    }

    #[test]
    fn relay_urls() {
        assert!(valid_relay_url("https://r.example.com"));
        assert!(valid_relay_url("http://10.0.0.1:3340/"));
        assert!(!valid_relay_url("https://"));
        assert!(!valid_relay_url("ftp://x"));
        assert!(!valid_relay_url("https://a b"));
    }

    #[test]
    fn round_trip_through_storage() {
        let st = SqliteStorage::open_in_memory().unwrap();
        assert_eq!(load(&st), Settings::default());
        let s = Settings { theme: Theme::Light, allow_hosting: false, ..Settings::default() };
        save(&st, &s).unwrap();
        assert_eq!(load(&st), s);
    }
}
