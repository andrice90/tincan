//! Profile selection (architecture.md, "Profiles"): `--profile <name>`, `--profile=<name>`
//! or the `TINCAN_PROFILE` environment variable, default `default`. Each profile is an
//! independent identity in `%APPDATA%\Tincan\profiles\<name>\`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Profile used when neither the command line nor the environment names one.
pub const DEFAULT_PROFILE: &str = "default";
/// Environment variable that selects a profile when `--profile` is absent.
pub const PROFILE_ENV: &str = "TINCAN_PROFILE";

const MISSING_VALUE: &str = "Couldn't start Tincan: --profile needs a name, for example --profile e2e-a.";
const INVALID_NAME: &str = "Couldn't start Tincan: a profile name is 1 to 32 letters, digits, - or _.";

/// Picks the profile name from the process arguments (without the program name) and the
/// value of `TINCAN_PROFILE`. The command line wins; the last `--profile` counts. An invalid
/// or missing name is an error, never a silent fallback to the default profile.
pub fn select(args: impl IntoIterator<Item = OsString>, env: Option<OsString>) -> Result<String, String> {
    let mut chosen: Option<OsString> = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--profile" {
            chosen = Some(args.next().ok_or(MISSING_VALUE)?);
        } else if let Some(value) = arg.to_str().and_then(|a| a.strip_prefix("--profile=")) {
            chosen = Some(value.into());
        }
    }
    match chosen.or(env) {
        None => Ok(DEFAULT_PROFILE.to_string()),
        Some(raw) => {
            let name = raw.into_string().map_err(|_| INVALID_NAME)?;
            if is_valid(&name) { Ok(name) } else { Err(INVALID_NAME.to_string()) }
        }
    }
}

/// True for names matching `[A-Za-z0-9_-]{1,32}`.
pub fn is_valid(name: &str) -> bool {
    (1..=32).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The profile directory under the roaming app data folder (`%APPDATA%`).
pub fn data_dir(roaming_app_data: &Path, name: &str) -> PathBuf {
    roaming_app_data.join("Tincan").join("profiles").join(name)
}

/// The WebView2 user data folder of a profile. Separate per profile so two instances get
/// separate browser processes, storage and caches.
pub fn webview_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("webview")
}

/// Window title: "Tincan", plus " (<profile>)" for any profile but the default one.
pub fn window_title(name: &str) -> String {
    if name == DEFAULT_PROFILE { "Tincan".to_string() } else { format!("Tincan ({name})") }
}

/// True when an environment flag is set to `1` or `true` (any case).
pub fn env_flag(value: Option<OsString>) -> bool {
    value.and_then(|v| v.into_string().ok()).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn default_when_nothing_given() {
        assert_eq!(select(args(&[]), None).unwrap(), "default");
        assert_eq!(select(args(&["--other", "x"]), None).unwrap(), "default");
    }

    #[test]
    fn command_line_forms_and_precedence() {
        assert_eq!(select(args(&["--profile", "e2e-a"]), None).unwrap(), "e2e-a");
        assert_eq!(select(args(&["--profile=e2e_b"]), None).unwrap(), "e2e_b");
        assert_eq!(select(args(&[]), Some("e2e-env".into())).unwrap(), "e2e-env");
        assert_eq!(select(args(&["--profile", "cli"]), Some("env".into())).unwrap(), "cli");
        assert_eq!(select(args(&["--profile", "a", "--profile=b"]), None).unwrap(), "b");
    }

    #[test]
    fn bad_names_are_errors_not_the_default() {
        assert!(select(args(&["--profile"]), None).is_err());
        assert!(select(args(&["--profile", ""]), None).is_err());
        assert!(select(args(&["--profile=a b"]), None).is_err());
        assert!(select(args(&["--profile", "../default"]), None).is_err());
        assert!(select(args(&[]), Some("".into())).is_err());
        assert!(select(args(&[]), Some("x".repeat(33).into())).is_err());
        // the command line wins, even over a bad environment value
        assert_eq!(select(args(&["--profile", "ok"]), Some("bad name".into())).unwrap(), "ok");
    }

    #[test]
    fn name_rules() {
        assert!(is_valid("a"));
        assert!(is_valid(&"Z9_-".repeat(8)));
        assert!(!is_valid(&"a".repeat(33)));
        for bad in ["", "a.b", "a/b", "a\\b", "é", "a b", "con:"] {
            assert!(!is_valid(bad), "{bad:?}");
        }
    }

    #[test]
    fn paths_and_titles() {
        let base = Path::new(r"C:\Users\u\AppData\Roaming");
        let dir = data_dir(base, "e2e-a");
        assert_eq!(dir, base.join("Tincan").join("profiles").join("e2e-a"));
        assert_eq!(webview_dir(&dir), dir.join("webview"));
        assert_eq!(window_title("default"), "Tincan");
        assert_eq!(window_title("e2e-a"), "Tincan (e2e-a)");
    }

    #[test]
    fn flags() {
        assert!(env_flag(Some("1".into())));
        assert!(env_flag(Some("TRUE".into())));
        assert!(!env_flag(Some("0".into())));
        assert!(!env_flag(None));
    }
}
