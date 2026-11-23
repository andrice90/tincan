//! File-system helpers: MIME types, safe export names and opening files with the shell.

use std::path::{Path, PathBuf};

/// MIME type from a file name's extension (`application/octet-stream` when unknown).
pub(crate) fn mime_for_name(name: &str) -> &'static str {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg" | "jfif") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("avif") => "image/avif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("txt" | "log") => "text/plain",
        Some("md") => "text/markdown",
        Some("csv") => "text/csv",
        Some("html" | "htm") => "text/html",
        Some("json") => "application/json",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("7z") => "application/x-7z-compressed",
        Some("rar") => "application/vnd.rar",
        Some("gz") => "application/gzip",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("ogg" | "oga") => "audio/ogg",
        Some("opus") => "audio/opus",
        Some("flac") => "audio/flac",
        Some("m4a") => "audio/mp4",
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("mkv") => "video/x-matroska",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// MIME type for serving a blob to the webview: raster images, audio and video keep their
/// type, everything else (SVG and HTML included, which could run script on the app origin)
/// is served as `application/octet-stream`. `hint` is the attachment's declared type;
/// the first bytes win for common image formats.
pub(crate) fn serve_mime(hint: Option<&str>, head: &[u8]) -> String {
    if let Some(sniffed) = sniff_image(head) {
        return sniffed.to_string();
    }
    match hint {
        Some(m) if is_safe_inline(m) => m.to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

fn is_safe_inline(mime: &str) -> bool {
    matches!(mime, "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp" | "image/avif" | "image/x-icon")
        || mime.starts_with("audio/")
        || mime.starts_with("video/")
}

/// Image type from magic bytes.
pub(crate) fn sniff_image(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if head.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else if head.starts_with(b"BM") && head.len() > 14 {
        Some("image/bmp")
    } else {
        None
    }
}

/// A file name that is safe to create on Windows: no directories, no reserved characters,
/// no reserved device names, no trailing dots or spaces.
pub(crate) fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out: String = base
        .chars()
        .map(|c| if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    let out = out.trim_start().to_string();
    if out.is_empty() {
        return "file".to_string();
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    let out = if reserved { format!("_{out}") } else { out };
    out.chars().take(200).collect()
}

/// `dir/name`, or `dir/stem (n).ext` with the first free `n` if that exists.
pub(crate) fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = p.extension().and_then(|s| s.to_str());
    for n in 1..10_000 {
        let candidate = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let path = dir.join(candidate);
        if !path.exists() {
            return path;
        }
    }
    dir.join(format!("{stem}-{}", tincan_core::now_ms()))
}

/// Open `path` with its default app, without a console window.
pub(crate) fn open_with_default_app(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // explorer.exe hands the file to its registered app and returns at once.
        std::process::Command::new("explorer.exe").arg(path).creation_flags(CREATE_NO_WINDOW).spawn().map(drop)
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn().map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_by_extension() {
        assert_eq!(mime_for_name("a.PNG"), "image/png");
        assert_eq!(mime_for_name("photo.jpeg"), "image/jpeg");
        assert_eq!(mime_for_name("notes"), "application/octet-stream");
        assert_eq!(mime_for_name("x.tar.gz"), "application/gzip");
    }

    #[test]
    fn serving_never_inlines_script_types() {
        assert_eq!(serve_mime(Some("image/svg+xml"), b"<svg"), "application/octet-stream");
        assert_eq!(serve_mime(Some("text/html"), b"<html>"), "application/octet-stream");
        assert_eq!(serve_mime(None, &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0]), "image/png");
        assert_eq!(serve_mime(Some("image/gif"), b"xx"), "image/gif");
        assert_eq!(serve_mime(Some("text/html"), b"GIF89a.."), "image/gif");
    }

    #[test]
    fn file_names_are_made_safe() {
        assert_eq!(safe_file_name("..\\..\\evil.exe"), "evil.exe");
        assert_eq!(safe_file_name("a/b/c.txt"), "c.txt");
        assert_eq!(safe_file_name("what?.txt"), "what_.txt");
        assert_eq!(safe_file_name("con.txt"), "_con.txt");
        assert_eq!(safe_file_name("COM1"), "_COM1");
        assert_eq!(safe_file_name("name. . "), "name");
        assert_eq!(safe_file_name(""), "file");
        assert_eq!(safe_file_name(".."), "file");
    }

    #[test]
    fn unique_paths() {
        let dir = tempfile::tempdir().unwrap();
        let a = unique_path(dir.path(), "x.txt");
        assert_eq!(a, dir.path().join("x.txt"));
        std::fs::write(&a, b"1").unwrap();
        let b = unique_path(dir.path(), "x.txt");
        assert_eq!(b, dir.path().join("x (1).txt"));
        std::fs::write(&b, b"1").unwrap();
        assert_eq!(unique_path(dir.path(), "x.txt"), dir.path().join("x (2).txt"));
        std::fs::write(dir.path().join("y"), b"1").unwrap();
        assert_eq!(unique_path(dir.path(), "y"), dir.path().join("y (1)"));
    }
}
