//! The `tincan` URI scheme: serves complete blobs for image previews (`file_url`).
//!
//! On Windows, Tauri 2.12 maps a custom scheme to `http://<scheme>.localhost/` (or `https://`
//! with `use_https_scheme`, which this app never enables), so `file_url` returns
//! `http://tincan.localhost/file/<hashHex>`. The page itself is served from another origin
//! (`http://tauri.localhost`, or the Vite dev server) and runs with COEP `require-corp`, so
//! every file response carries `Cross-Origin-Resource-Policy: cross-origin`.

use tauri::http::{Method, Request, Response, StatusCode, header};
use tauri::{Manager, Runtime, UriSchemeContext, UriSchemeResponder};

use crate::Shell;

/// Scheme name registered with Tauri.
pub const SCHEME: &str = "tincan";

/// Path prefix for blobs.
const FILE_PREFIX: &str = "/file/";

/// Blobs are content-addressed: a hash never changes its bytes.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// The URL origin Tauri gives the scheme on this platform.
pub fn origin() -> &'static str {
    if cfg!(any(windows, target_os = "android")) { "http://tincan.localhost" } else { "tincan://localhost" }
}

/// `AppConfig::file_url_base`: the prefix `file_url` puts before the hash hex.
pub fn file_url_base() -> String {
    format!("{}{FILE_PREFIX}", origin())
}

/// The blob hash in a `/file/<hashHex>` path: exactly 64 lowercase hex digits.
pub fn hash_from_path(path: &str) -> Option<&str> {
    let hash = path.strip_prefix(FILE_PREFIX)?;
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(hash)
}

/// 200 with the blob. HEAD gets the headers only.
pub fn file_response(bytes: Vec<u8>, mime: &str, head: bool) -> Response<Vec<u8>> {
    let mime = if header::HeaderValue::from_str(mime).is_ok() && !mime.is_empty() { mime } else { "application/octet-stream" };
    let len = bytes.len();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, len)
        .header("Cross-Origin-Resource-Policy", "cross-origin")
        .header(header::CACHE_CONTROL, IMMUTABLE)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        // a blob opened as a document (it never should be) runs nothing
        .header(header::CONTENT_SECURITY_POLICY, "default-src 'none'; sandbox")
        .body(if head { Vec::new() } else { bytes })
        .expect("static headers are valid")
}

/// An empty error response that is not cached (the blob may complete later).
pub fn error_response(status: StatusCode) -> Response<Vec<u8>> {
    let mut b = Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store")
        .header("Cross-Origin-Resource-Policy", "cross-origin");
    if status == StatusCode::METHOD_NOT_ALLOWED {
        b = b.header(header::ALLOW, "GET, HEAD");
    }
    b.body(Vec::new()).expect("static headers are valid")
}

/// Asynchronous scheme handler: answers on the async runtime, never on the webview thread.
pub fn handle<R: Runtime>(ctx: UriSchemeContext<'_, R>, request: Request<Vec<u8>>, responder: UriSchemeResponder) {
    let app = ctx.app_handle().clone();
    tauri::async_runtime::spawn(async move {
        let head = request.method() == Method::HEAD;
        let response = if request.method() != Method::GET && !head {
            error_response(StatusCode::METHOD_NOT_ALLOWED)
        } else if let Some(hash) = hash_from_path(request.uri().path()) {
            let backend = app.state::<Shell>().gate.clone();
            match backend.get().await {
                Ok(backend) => match backend.file_for_url(hash).await {
                    Some((bytes, mime)) => file_response(bytes, &mime, head),
                    None => error_response(StatusCode::NOT_FOUND),
                },
                Err(_) => error_response(StatusCode::SERVICE_UNAVAILABLE),
            }
        } else {
            error_response(StatusCode::NOT_FOUND)
        };
        responder.respond(response);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn url_base_matches_the_windows_scheme_origin() {
        #[cfg(windows)]
        assert_eq!(file_url_base(), "http://tincan.localhost/file/");
        assert!(file_url_base().ends_with("/file/"));
    }

    #[test]
    fn only_full_lowercase_hashes_parse() {
        assert_eq!(hash_from_path(&format!("/file/{HASH}")), Some(HASH));
        assert_eq!(hash_from_path(&format!("/file/{}", HASH.to_uppercase())), None);
        assert_eq!(hash_from_path(&format!("/file/{}", &HASH[1..])), None);
        assert_eq!(hash_from_path(&format!("/file/{HASH}0")), None);
        assert_eq!(hash_from_path(&format!("/files/{HASH}")), None);
        assert_eq!(hash_from_path(&format!("/file/{HASH}/x")), None);
        assert_eq!(hash_from_path("/file/../etc"), None);
        assert_eq!(hash_from_path("/"), None);
    }

    #[test]
    fn file_response_headers() {
        let r = file_response(vec![1, 2, 3], "image/png", false);
        assert_eq!(r.status(), StatusCode::OK);
        let h = r.headers();
        assert_eq!(h["content-type"], "image/png");
        assert_eq!(h["content-length"], "3");
        assert_eq!(h["cross-origin-resource-policy"], "cross-origin");
        assert!(h["cache-control"].to_str().unwrap().contains("immutable"));
        assert_eq!(h["x-content-type-options"], "nosniff");
        assert_eq!(r.body(), &vec![1, 2, 3]);

        let head = file_response(vec![1, 2, 3], "image/png", true);
        assert!(head.body().is_empty());
        assert_eq!(head.headers()["content-length"], "3");

        let odd = file_response(vec![], "bad\nmime", false);
        assert_eq!(odd.headers()["content-type"], "application/octet-stream");
    }

    #[test]
    fn errors_are_not_cached() {
        let r = error_response(StatusCode::NOT_FOUND);
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(r.headers()["cache-control"], "no-store");
        assert_eq!(error_response(StatusCode::METHOD_NOT_ALLOWED).headers()["allow"], "GET, HEAD");
    }
}
