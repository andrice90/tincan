//! Avatars and server icons: the rules every replica applies to an [`ImageRef`], and the
//! checks an app runs on image bytes before publishing them (magic bytes, size, dimensions
//! read from the file header; nothing is decoded).

use crate::event::ImageRef;

/// Largest avatar or server icon file: 64 KiB.
pub const MAX_IMAGE_BYTES: u64 = 64 * 1024;
/// Largest width or height accepted from an image header (the UI makes 128x128).
pub const MAX_IMAGE_SIDE: u32 = 256;
/// The image types an avatar or icon may have.
pub const IMAGE_MIMES: [&str; 2] = ["image/webp", "image/png"];

/// Check an image reference carried in an event: 1 to [`MAX_IMAGE_BYTES`] bytes and a type in
/// [`IMAGE_MIMES`]. The size is only the author's claim; downloads prove the real size.
pub fn validate_image_ref(img: &ImageRef) -> Result<(), String> {
    if img.size == 0 || img.size > MAX_IMAGE_BYTES {
        return Err(format!("image must be 1 to {MAX_IMAGE_BYTES} bytes"));
    }
    if !IMAGE_MIMES.contains(&img.mime.as_str()) {
        return Err("image must be image/webp or image/png".into());
    }
    Ok(())
}

/// What the header of an image file says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    /// `image/webp` or `image/png`.
    pub mime: &'static str,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// The type of an avatar or icon file from its magic bytes: PNG or WebP, else None.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn u32_be(b: &[u8], at: usize) -> Option<u32> { Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?)) }
fn u32_le(b: &[u8], at: usize) -> Option<u32> { Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?)) }
fn u16_le(b: &[u8], at: usize) -> Option<u16> { Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?)) }
fn u24_le(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 3)?;
    Some(u32::from(s[0]) | (u32::from(s[1]) << 8) | (u32::from(s[2]) << 16))
}

/// Width and height from a PNG IHDR or a WebP VP8, VP8L or VP8X header.
fn dimensions(bytes: &[u8], mime: &str) -> Option<(u32, u32)> {
    if mime == "image/png" {
        // Signature (8), IHDR length (4), "IHDR" (4), width, height (big-endian).
        if bytes.get(12..16)? != b"IHDR" {
            return None;
        }
        return Some((u32_be(bytes, 16)?, u32_be(bytes, 20)?));
    }
    // "RIFF" size "WEBP", then the first chunk: fourcc (12..16), size (16..20), data (20..).
    match bytes.get(12..16)? {
        b"VP8 " => {
            // Frame tag (3 bytes), start code 9d 01 2a, then 14-bit width and height.
            if bytes.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            Some((u32::from(u16_le(bytes, 26)? & 0x3fff), u32::from(u16_le(bytes, 28)? & 0x3fff)))
        }
        b"VP8L" => {
            if *bytes.get(20)? != 0x2f {
                return None;
            }
            let b = u32_le(bytes, 21)?;
            Some(((b & 0x3fff) + 1, ((b >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => Some((u24_le(bytes, 24)? + 1, u24_le(bytes, 27)? + 1)),
        _ => None,
    }
}

/// Check the bytes of an avatar or icon before publishing them: at most
/// [`MAX_IMAGE_BYTES`], PNG or WebP by magic bytes, and a header whose width and height are
/// 1 to [`MAX_IMAGE_SIDE`]. Errors are user-facing.
pub fn inspect(bytes: &[u8]) -> Result<ImageInfo, String> {
    if bytes.is_empty() {
        return Err("That image is empty.".into());
    }
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err("That image is larger than 64 KB. Pick a smaller one.".into());
    }
    let mime = sniff(bytes).ok_or_else(|| "That isn't a PNG or WebP image.".to_string())?;
    let (width, height) = dimensions(bytes, mime).ok_or_else(|| "That image file is damaged.".to_string())?;
    if width == 0 || height == 0 || width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE {
        return Err(format!("That image is {width}x{height}. Use one of at most {MAX_IMAGE_SIDE}x{MAX_IMAGE_SIDE} pixels."));
    }
    Ok(ImageInfo { mime, width, height })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BlobHash;

    /// A PNG signature plus an IHDR chunk header for `w` x `h`.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        b
    }

    fn riff(chunk: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut b = b"RIFF".to_vec();
        b.extend_from_slice(&u32::try_from(data.len() + 12).unwrap().to_le_bytes());
        b.extend_from_slice(b"WEBP");
        b.extend_from_slice(chunk);
        b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
        b.extend_from_slice(data);
        b
    }

    fn webp_lossy(w: u16, h: u16) -> Vec<u8> {
        let mut d = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a];
        d.extend_from_slice(&w.to_le_bytes());
        d.extend_from_slice(&h.to_le_bytes());
        d.extend_from_slice(&[0; 8]);
        riff(b"VP8 ", &d)
    }

    fn webp_lossless(w: u32, h: u32) -> Vec<u8> {
        let bits = (w - 1) | ((h - 1) << 14);
        let mut d = vec![0x2f];
        d.extend_from_slice(&bits.to_le_bytes());
        d.extend_from_slice(&[0; 8]);
        riff(b"VP8L", &d)
    }

    fn webp_extended(w: u32, h: u32) -> Vec<u8> {
        let mut d = vec![0x10, 0, 0, 0];
        d.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
        d.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
        riff(b"VP8X", &d)
    }

    #[test]
    fn image_refs_are_checked() {
        let ok = ImageRef { hash: BlobHash([1; 32]), size: 4000, mime: "image/webp".into() };
        assert!(validate_image_ref(&ok).is_ok());
        assert!(validate_image_ref(&ImageRef { mime: "image/png".into(), ..ok.clone() }).is_ok());
        assert!(validate_image_ref(&ImageRef { size: MAX_IMAGE_BYTES, ..ok.clone() }).is_ok());
        assert!(validate_image_ref(&ImageRef { size: MAX_IMAGE_BYTES + 1, ..ok.clone() }).is_err());
        assert!(validate_image_ref(&ImageRef { size: 0, ..ok.clone() }).is_err());
        for bad in ["image/jpeg", "image/svg+xml", "text/html", "", "IMAGE/PNG"] {
            assert!(validate_image_ref(&ImageRef { mime: bad.into(), ..ok.clone() }).is_err(), "{bad}");
        }
    }

    #[test]
    fn headers_give_type_and_size() {
        assert_eq!(inspect(&png(128, 128)), Ok(ImageInfo { mime: "image/png", width: 128, height: 128 }));
        assert_eq!(inspect(&webp_lossy(128, 96)), Ok(ImageInfo { mime: "image/webp", width: 128, height: 96 }));
        assert_eq!(inspect(&webp_lossless(128, 128)), Ok(ImageInfo { mime: "image/webp", width: 128, height: 128 }));
        assert_eq!(inspect(&webp_extended(200, 1)), Ok(ImageInfo { mime: "image/webp", width: 200, height: 1 }));
    }

    #[test]
    fn bad_bytes_are_refused() {
        assert!(inspect(&[]).is_err());
        assert!(inspect(b"<svg xmlns='http://www.w3.org/2000/svg'/>").is_err(), "SVG is never an avatar");
        assert!(inspect(&[0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0]).is_err(), "JPEG is not accepted");
        assert!(inspect(&png(128, 128)[..20]).is_err(), "a cut header");
        assert!(inspect(&png(0, 128)).is_err());
        assert!(inspect(&png(4096, 4096)).is_err(), "too many pixels");
        assert!(inspect(&webp_lossless(257, 10)).is_err());
        let mut big = png(128, 128);
        big.resize(usize::try_from(MAX_IMAGE_BYTES).unwrap() + 1, 0);
        assert!(inspect(&big).is_err(), "over 64 KiB");
        let mut fits = png(128, 128);
        fits.resize(usize::try_from(MAX_IMAGE_BYTES).unwrap(), 0);
        assert!(inspect(&fits).is_ok());
        let mut broken = webp_lossy(10, 10);
        broken[23] = 0;
        assert!(inspect(&broken).is_err(), "no VP8 start code");
        assert!(inspect(&riff(b"ALPH", &[0; 16])).is_err(), "an unknown first chunk");
    }
}
