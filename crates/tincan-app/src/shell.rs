//! File-system helpers: MIME types, safe export names, peer text clean-up, and how an
//! exported file is handed to the shell (opened, or revealed in Explorer).

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
/// is served as `application/octet-stream`. `hint` is the type of the attachment's name;
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

/// Unicode format characters that change how the text around them renders: zero-width
/// characters and joiners, bidi marks, embeddings, overrides and isolates, word joiners and
/// invisible operators, the Arabic letter mark and the byte order mark. A right-to-left
/// override makes `photo<RLO>gnp.exe` display as `photoexe.png`.
pub(crate) fn is_hidden_format(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{061C}' | '\u{FEFF}')
}

/// Peer-supplied text for display (names of people, servers, channels, files and sounds):
/// control and hidden format characters removed, surrounding whitespace trimmed. May be
/// empty; callers pick a fallback.
pub(crate) fn clean_text(s: &str) -> String {
    let kept: String = s.chars().filter(|c| !c.is_control() && !is_hidden_format(*c)).collect();
    kept.trim().to_string()
}

/// Peer text as one line: hidden format characters removed, control characters and runs
/// of whitespace folded to one space, cut to `max` characters.
pub(crate) fn clean_line(s: &str, max: usize) -> String {
    let spaced: String = s.chars().filter(|c| !is_hidden_format(*c)).map(|c| if c.is_control() { ' ' } else { c }).collect();
    let mut out = String::new();
    for word in spaced.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out.chars().take(max).collect::<String>().trim_end().to_string()
}

/// Longest export name, in characters.
const MAX_FILE_NAME_CHARS: usize = 200;
/// Longest extension that truncation keeps.
const MAX_EXT_CHARS: usize = 16;

fn trim_name_end(s: &mut String) {
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
}

/// A file name that is safe to create on Windows and that shows its real extension: no
/// directories, no control or hidden format characters, no reserved characters, whitespace
/// runs (no-break spaces included) folded to one space, no trailing dots or spaces, no
/// reserved device names, at most 200 characters with the last extension kept when it is
/// cut.
pub(crate) fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out = String::with_capacity(base.len());
    let mut in_space = false;
    for c in base.chars() {
        if c.is_control() || is_hidden_format(c) {
            continue;
        }
        if c.is_whitespace() {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
            continue;
        }
        in_space = false;
        out.push(if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') { '_' } else { c });
    }
    let mut out = out.trim_start().to_string();
    trim_name_end(&mut out);
    if out.is_empty() {
        return "file".to_string();
    }
    if out.chars().count() > MAX_FILE_NAME_CHARS {
        let ext = out
            .rsplit_once('.')
            .map(|(_, e)| e.to_string())
            .filter(|e| (1..=MAX_EXT_CHARS).contains(&e.chars().count()) && !e.contains(' '));
        out = match ext {
            Some(ext) => {
                let stem_len = out.chars().count() - ext.chars().count() - 1;
                let keep = stem_len.min(MAX_FILE_NAME_CHARS - ext.chars().count() - 1);
                let mut stem: String = out.chars().take(keep).collect();
                trim_name_end(&mut stem);
                if stem.is_empty() {
                    stem = "file".to_string();
                }
                format!("{stem}.{ext}")
            }
            None => {
                let mut cut: String = out.chars().take(MAX_FILE_NAME_CHARS).collect();
                trim_name_end(&mut cut);
                if cut.is_empty() { "file".to_string() } else { cut }
            }
        };
    }
    let stem = out.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    if is_reserved_device(&stem) { format!("_{out}") } else { out }
}

/// A Windows device name (`stem` upper-cased, without extension): CON, PRN, AUX, NUL,
/// CONIN$, CONOUT$, and COM or LPT followed by one digit or a superscript 1, 2 or 3.
fn is_reserved_device(stem: &str) -> bool {
    if matches!(stem, "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    let Some(n) = stem.strip_prefix("COM").or_else(|| stem.strip_prefix("LPT")) else { return false };
    let mut c = n.chars();
    matches!((c.next(), c.next()), (Some('0'..='9' | '\u{B9}' | '\u{B2}' | '\u{B3}'), None))
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

/// What `open_file` does with an exported file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenAction {
    /// Hand it to its default app.
    Open,
    /// Show it selected in Explorer; the user decides.
    Reveal,
}

/// Images that are opened when their bytes match the extension.
const OPEN_IMAGES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
/// Documents, audio and video that are opened by extension.
const OPEN_OTHER: &[&str] = &["pdf", "txt", "mp3", "wav", "ogg", "oga", "opus", "flac", "m4a", "mp4", "m4v", "webm", "mov"];

/// Open or reveal: only an allowlist of document and media extensions is opened, and an
/// image only when its first bytes are that image type. Everything else (programs,
/// scripts, shortcuts, HTML applications, archives, office files, unknown types) is
/// revealed, and so is any file whose Mark of the Web could not be written. `file_name` is
/// the final exported name (never the peer's raw name); `head` is its first bytes.
pub(crate) fn open_action(file_name: &str, head: &[u8], marked: bool) -> OpenAction {
    if !marked {
        return OpenAction::Reveal;
    }
    let Some(ext) = Path::new(file_name).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase) else {
        return OpenAction::Reveal;
    };
    if OPEN_IMAGES.contains(&ext.as_str()) {
        let want = mime_for_name(file_name);
        return if sniff_image(head) == Some(want) { OpenAction::Open } else { OpenAction::Reveal };
    }
    if OPEN_OTHER.contains(&ext.as_str()) { OpenAction::Open } else { OpenAction::Reveal }
}

/// Contents of the `Zone.Identifier` stream: downloaded from the Internet zone, so
/// SmartScreen, Attachment Manager and Office Protected View treat the file as untrusted.
pub(crate) const ZONE_IDENTIFIER: &str = "[ZoneTransfer]\r\nZoneId=3\r\n";

/// The alternate data stream that carries the Mark of the Web for `path`.
pub(crate) fn zone_stream(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(":Zone.Identifier");
    PathBuf::from(s)
}

/// Writes the Mark of the Web (`ZoneId=3`) on `path`. Fails where streams are not
/// supported (FAT and exFAT drives); `open_file` then reveals instead of opening.
pub(crate) async fn mark_of_the_web(path: &Path) -> std::io::Result<()> {
    if cfg!(windows) { tokio::fs::write(zone_stream(path), ZONE_IDENTIFIER).await } else { Ok(()) }
}

/// The argument that makes `explorer.exe` open the folder of `path` with it selected.
/// Passed raw: with normal argument quoting the whole `/select,<path>` would become one
/// quoted token, which Explorer misreads for paths with spaces.
pub(crate) fn reveal_arg(path: &Path) -> String { format!("/select,\"{}\"", path.display()) }

/// Show `path` selected in its folder, without a console window.
pub(crate) fn reveal_in_folder(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("explorer.exe").raw_arg(reveal_arg(path)).creation_flags(CREATE_NO_WINDOW).spawn().map(drop)
    }
    #[cfg(not(windows))]
    {
        let dir = path.parent().unwrap_or(path);
        std::process::Command::new("xdg-open").arg(dir).spawn().map(drop)
    }
}

/// Open `path` with its default app, without a console window. Only for files
/// [`open_action`] allows.
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

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0, 0, 0, 0, 0];
    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x00\x00\x00\x00\x00\x00";

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
        assert_eq!(safe_file_name("nul .txt"), "_nul .txt");
        assert_eq!(safe_file_name("lpt\u{B9}.txt"), "_lpt\u{B9}.txt");
        assert_eq!(safe_file_name("COM\u{B3}"), "_COM\u{B3}");
        assert_eq!(safe_file_name("conout$.log"), "_conout$.log");
        assert_eq!(safe_file_name("COM10.txt"), "COM10.txt");
        assert_eq!(safe_file_name("console.txt"), "console.txt");
        assert_eq!(safe_file_name("name. . "), "name");
        assert_eq!(safe_file_name(""), "file");
        assert_eq!(safe_file_name(".."), "file");
        assert_eq!(safe_file_name("tab\there.txt"), "tabhere.txt");
    }

    #[test]
    fn hidden_format_characters_are_dropped_from_file_names() {
        // Right-to-left override: would display as "photoexe.png".
        assert_eq!(safe_file_name("photo\u{202E}gnp.exe"), "photognp.exe");
        for c in ['\u{200B}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202D}', '\u{2066}', '\u{2069}', '\u{2060}', '\u{061C}', '\u{FEFF}'] {
            assert_eq!(safe_file_name(&format!("a{c}b.pdf")), "ab.pdf", "U+{:04X}", c as u32);
        }
        assert_eq!(safe_file_name("\u{202E}\u{200B}"), "file");
    }

    #[test]
    fn padding_cannot_hide_the_real_extension() {
        // No-break spaces fold to one space, so the export shows ".pdf .exe".
        let padded = format!("Invoice.pdf{}.exe", "\u{a0}".repeat(60));
        assert_eq!(safe_file_name(&padded), "Invoice.pdf .exe");
        // A name cut to length keeps its last extension, not an earlier one.
        let long = format!("invoice{}.exe.pdf", "x".repeat(189));
        let safe = safe_file_name(&long);
        assert!(safe.ends_with(".exe.pdf") || safe.ends_with(".pdf"), "{safe}");
        assert!(safe.chars().count() <= 200);
        let long = format!("Holiday{}.exe.jpg", "y".repeat(400));
        let safe = safe_file_name(&long);
        assert!(safe.ends_with(".jpg"), "{safe}");
        assert_eq!(safe.chars().count(), 200);
        // Trailing dots and spaces left by the cut are trimmed again.
        let dots = format!("{}{}.png", "a".repeat(190), ". ".repeat(20));
        let safe = safe_file_name(&dots);
        assert!(safe.ends_with("a.png"), "{safe}");
        // A long name without a usable extension is cut and trimmed.
        let plain = format!("{} .", "z".repeat(300));
        assert_eq!(safe_file_name(&plain), "z".repeat(200));
    }

    #[test]
    fn display_text_drops_hidden_characters() {
        assert_eq!(clean_text(" Bob\u{200B} "), "Bob");
        assert_eq!(clean_text("Eve\u{202E}lacitirc"), "Evelacitirc");
        assert_eq!(clean_text("\u{1b}[31mred\u{7}"), "[31mred");
        assert_eq!(clean_text("\u{2066}\u{2069}"), "");
        assert_eq!(clean_line("a \u{a0}\u{a0} b\n\nc", 100), "a b c");
        assert_eq!(clean_line(&"x".repeat(300), 200).chars().count(), 200);
    }

    #[test]
    fn only_documents_and_media_are_opened() {
        for name in ["a.pdf", "a.txt", "a.mp3", "a.wav", "a.flac", "a.m4a", "a.ogg", "a.mp4", "a.webm", "a.mov", "A.PDF"] {
            assert_eq!(open_action(name, b"", true), OpenAction::Open, "{name}");
        }
        for name in [
            "a.exe", "a.scr", "a.com", "a.bat", "a.cmd", "a.ps1", "a.js", "a.jse", "a.vbs", "a.wsf", "a.hta", "a.lnk", "a.url", "a.msi",
            "a.cpl", "a.jar", "a.html", "a.svg", "a.zip", "a.iso", "a.docx", "a.xlsm", "a.reg", "a.pdf.exe", "noext", ".hta",
        ] {
            assert_eq!(open_action(name, PNG, true), OpenAction::Reveal, "{name}");
        }
    }

    #[test]
    fn images_open_only_when_the_bytes_match() {
        assert_eq!(open_action("cat.png", PNG, true), OpenAction::Open);
        assert_eq!(open_action("cat.gif", GIF, true), OpenAction::Open);
        assert_eq!(open_action("cat.JPEG", &[0xff, 0xd8, 0xff, 0xe0, 0, 0x10], true), OpenAction::Open);
        assert_eq!(open_action("cat.webp", b"RIFF\x24\x00\x00\x00WEBPVP8 ", true), OpenAction::Open);
        assert_eq!(open_action("cat.bmp", b"BM\x36\x00\x00\x00\x00\x00\x00\x00\x36\x00\x00\x00\x28\x00", true), OpenAction::Open);
        assert_eq!(open_action("cat.webp", PNG, true), OpenAction::Reveal, "a PNG named .webp");
        assert_eq!(open_action("cat.bmp", b"BM", true), OpenAction::Reveal, "too short to be a bitmap");
        assert_eq!(open_action("cat.png", GIF, true), OpenAction::Reveal, "a GIF named .png");
        assert_eq!(open_action("cat.jpg", b"<script>x</script>", true), OpenAction::Reveal);
        assert_eq!(open_action("cat.hta", GIF, true), OpenAction::Reveal, "a GIF polyglot named .hta");
    }

    #[test]
    fn nothing_opens_without_the_mark_of_the_web() {
        assert_eq!(open_action("a.pdf", b"", false), OpenAction::Reveal);
        assert_eq!(open_action("cat.png", PNG, false), OpenAction::Reveal);
    }

    #[test]
    fn reveal_selects_the_file() {
        let p = Path::new(r"C:\Users\me\Downloads\Tincan\my file.exe");
        assert_eq!(reveal_arg(p), r#"/select,"C:\Users\me\Downloads\Tincan\my file.exe""#);
        assert_eq!(zone_stream(p), PathBuf::from(r"C:\Users\me\Downloads\Tincan\my file.exe:Zone.Identifier"));
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

    #[cfg(windows)]
    #[tokio::test]
    async fn the_mark_of_the_web_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.pdf");
        std::fs::write(&p, b"%PDF").unwrap();
        mark_of_the_web(&p).await.unwrap();
        assert_eq!(std::fs::read_to_string(zone_stream(&p)).unwrap(), ZONE_IDENTIFIER);
        assert_eq!(std::fs::read(&p).unwrap(), b"%PDF", "the file itself is unchanged");
    }
}
