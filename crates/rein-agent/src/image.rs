//! Images the model is shown: what a tool gave (an MCP server's
//! screenshot, an image file `read` opened, one you named with `@`),
//! checked once as it arrives and kept as a file of rein's, named by what's
//! in it, so a conversation's file names it and holds none of its bytes.
//!
//! What can't be shown fails closed: an image that can't be read, or that's
//! too big to send as it is, becomes a note saying so, never bytes a
//! provider would turn away, which would break every request after. A PNG,
//! as screenshots are, is shrunk to fit; another kind goes as it is, or not
//! at all.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rein_llm::{Image, ImageKind};

/// The longest side an image is shown at: what Claude Code shrinks to, and
/// within what OpenAI's and Anthropic's models take at their own sizes.
pub const LONGEST_SIDE: u32 = 2000;

/// The most an image may weigh: within every provider's limit.
pub const HEAVIEST: usize = 5 * 1024 * 1024;

/// The longest side any provider takes at all.
const WIDEST: u32 = 8000;

/// A PNG with more pixels than this isn't opened: its pixels alone would
/// take hundreds of megabytes.
const MOST_PIXELS: u64 = 64_000_000;

/// The kind of image `bytes` are, by how they start.
pub fn kind_of(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageKind::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageKind::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageKind::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageKind::Webp)
    } else {
        None
    }
}

/// Its width and height, as its header says: `None` if it doesn't say.
fn size_of(kind: ImageKind, bytes: &[u8]) -> Option<(u32, u32)> {
    let be16 = |at: usize| bytes.get(at..at + 2).map(|b| u32::from(u16::from_be_bytes([b[0], b[1]])));
    let le16 = |at: usize| bytes.get(at..at + 2).map(|b| u32::from(u16::from_le_bytes([b[0], b[1]])));
    let le24 = |at: usize| bytes.get(at..at + 3).map(|b| u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16);
    let size = match kind {
        ImageKind::Png => {
            let b = bytes.get(16..24)?;
            (u32::from_be_bytes([b[0], b[1], b[2], b[3]]), u32::from_be_bytes([b[4], b[5], b[6], b[7]]))
        }
        ImageKind::Gif => (le16(6)?, le16(8)?),
        ImageKind::Webp => match bytes.get(12..16)? {
            b"VP8 " => (le16(26)? & 0x3fff, le16(28)? & 0x3fff),
            b"VP8L" => {
                let b = bytes.get(21..25)?;
                let bits = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
            }
            b"VP8X" => (le24(24)? + 1, le24(27)? + 1),
            _ => return None,
        },
        ImageKind::Jpeg => {
            // The frame's header (a start-of-frame marker) says its size.
            let mut at = 2;
            loop {
                while bytes.get(at) == Some(&0xff) && bytes.get(at + 1) == Some(&0xff) {
                    at += 1;
                }
                if bytes.get(at) != Some(&0xff) {
                    return None;
                }
                let marker = *bytes.get(at + 1)?;
                if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                    break (be16(at + 7)?, be16(at + 5)?);
                }
                if matches!(marker, 0xd0..=0xd9 | 0x01) {
                    at += 2;
                    continue;
                }
                at = at.checked_add(2)?.checked_add(be16(at + 2)? as usize)?;
            }
        }
    };
    (size.0 > 0 && size.1 > 0).then_some(size)
}

/// `bytes`, said to be `what` (`image shot.png`), ready to show: checked,
/// shrunk where it's a PNG too big to send, and kept in `folder`. Or why it
/// can't be shown. It takes a while for a big PNG: run it off the loop's
/// thread.
pub fn prepare(bytes: &[u8], what: &str, folder: &Path) -> Result<Image, String> {
    let kind = kind_of(bytes).ok_or_else(|| format!("{what} isn't an image rein can show: PNG, JPEG, GIF or WebP"))?;
    let (width, height) = size_of(kind, bytes).ok_or_else(|| format!("{what} doesn't say how big it is, so rein can't show it"))?;
    let fits = |w: u32, h: u32, weight: usize| w.max(h) <= LONGEST_SIDE && weight <= HEAVIEST;
    let (data, shown) = if fits(width, height, bytes.len()) {
        (bytes.to_vec(), (width, height))
    } else if kind == ImageKind::Png {
        shrink_png(bytes, width, height).map_err(|why| format!("{what} is too big to show as it is, and couldn't be shrunk: {why}"))?
    } else if width.max(height) <= WIDEST && bytes.len() <= HEAVIEST {
        // Within what providers take: they shrink it themselves.
        (bytes.to_vec(), (width, height))
    } else {
        let megabytes = bytes.len() as f64 / 1_048_576.0;
        return Err(format!("{what} is {width}×{height}, {megabytes:.1} MB: too big to show, and rein shrinks only PNGs"));
    };
    let file = keep(&data, kind, folder).map_err(|e| format!("can't keep {what} to show it: {e}"))?;
    let about = if shown == (width, height) {
        format!("{what}, {width}×{height}")
    } else {
        let scale = f64::from(width) / f64::from(shown.0);
        format!("{what}, {width}×{height}, shown at {}×{}: multiply what you see by {scale:.2} for its own pixels", shown.0, shown.1)
    };
    Ok(Image { kind, data: Some(Arc::from(data)), file, width: shown.0, height: shown.1, about })
}

/// Keeps `data` in `folder` under a name of what's in it, once: only you
/// can read it, and it's written whole.
fn keep(data: &[u8], kind: ImageKind, folder: &Path) -> Result<PathBuf, String> {
    use sha2::Digest;
    let name: String = sha2::Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect();
    let file = folder.join(format!("{name}.{}", kind.extension()));
    if !file.exists() {
        rein_llm::store::write_private(&file, data).map_err(|e| e.to_string())?;
    }
    Ok(file)
}

/// A PNG of `width`×`height` shrunk to fit: to [`LONGEST_SIDE`], then
/// smaller still while it weighs more than [`HEAVIEST`].
fn shrink_png(bytes: &[u8], width: u32, height: u32) -> Result<(Vec<u8>, (u32, u32)), String> {
    if u64::from(width) * u64::from(height) > MOST_PIXELS {
        return Err(format!("{width}×{height} is more pixels than rein opens"));
    }
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let mut pixels = vec![0; reader.output_buffer_size().ok_or("it's too big to open")?];
    let frame = reader.next_frame(&mut pixels).map_err(|e| e.to_string())?;
    let (color, channels) = (frame.color_type, frame.color_type.samples());
    let (w, h) = (frame.width as usize, frame.height as usize);
    let mut side = f64::from(LONGEST_SIDE.min(width.max(height)));
    loop {
        let scale = side / w.max(h) as f64;
        let (nw, nh) = (((w as f64 * scale).round() as usize).max(1), ((h as f64 * scale).round() as usize).max(1));
        let small = area_average(&pixels, w, h, channels, nw, nh);
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, nw as u32, nh as u32);
            encoder.set_color(color);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().and_then(|mut writer| writer.write_image_data(&small)).map_err(|e| e.to_string())?;
        }
        if out.len() <= HEAVIEST || side <= 256.0 {
            return Ok((out, (nw as u32, nh as u32)));
        }
        side *= 0.75;
    }
}

/// `pixels` (`w`×`h`, `channels` a pixel) as `nw`×`nh`, each new pixel the
/// average of those it covers: what shrinking text in a screenshot needs.
fn area_average(pixels: &[u8], w: usize, h: usize, channels: usize, nw: usize, nh: usize) -> Vec<u8> {
    let mut out = vec![0u8; nw * nh * channels];
    for y in 0..nh {
        let (y0, y1) = (y * h / nh, ((y + 1) * h / nh).max(y * h / nh + 1).min(h));
        for x in 0..nw {
            let (x0, x1) = (x * w / nw, ((x + 1) * w / nw).max(x * w / nw + 1).min(w));
            let count = ((y1 - y0) * (x1 - x0)) as u32;
            for c in 0..channels {
                let mut sum = 0u32;
                for row in y0..y1 {
                    for column in x0..x1 {
                        sum += u32::from(pixels[(row * w + column) * channels + c]);
                    }
                }
                out[(y * nw + x) * channels + c] = (sum / count) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A PNG of `width`×`height`, its pixels a pattern, compressed as `level` says.
    pub(crate) fn png(width: u32, height: u32, level: png::Compression) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(level);
        let pixels: Vec<u8> = (0..width * height * 3).map(|i| (i * 7 % 251) as u8).collect();
        encoder.write_header().unwrap().write_image_data(&pixels).unwrap();
        out
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rein-images-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn each_kind_says_its_size_by_its_header() {
        assert_eq!(size_of(ImageKind::Png, &png(3, 2, png::Compression::Fast)), Some((3, 2)));
        // GIF: logical screen 10×20.
        let gif = [b"GIF89a".as_slice(), &[10, 0, 20, 0, 0, 0, 0]].concat();
        assert_eq!((kind_of(&gif), size_of(ImageKind::Gif, &gif)), (Some(ImageKind::Gif), Some((10, 20))));
        // JPEG: an APP0 segment, then a baseline frame of 640×480.
        let jpeg = [&[0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0][..], &[0xff, 0xc0, 0, 11, 8, 0x01, 0xe0, 0x02, 0x80, 3, 0, 0, 0]].concat();
        assert_eq!((kind_of(&jpeg), size_of(ImageKind::Jpeg, &jpeg)), (Some(ImageKind::Jpeg), Some((640, 480))));
        // WebP, lossless: 100×50, each less one, packed in 14 bits.
        let bits: u32 = 99 | (49 << 14);
        let webp = [b"RIFF\0\0\0\0WEBPVP8L\0\0\0\0\x2f".as_slice(), &bits.to_le_bytes()].concat();
        assert_eq!((kind_of(&webp), size_of(ImageKind::Webp, &webp)), (Some(ImageKind::Webp), Some((100, 50))));
        assert_eq!(kind_of(b"hello"), None);
        assert_eq!(size_of(ImageKind::Jpeg, &[0xff, 0xd8, 0xff]), None, "cut short: it doesn't say");
    }

    #[test]
    fn a_small_image_goes_as_it_is_kept_once_by_what_it_holds() {
        let dir = folder("small");
        let bytes = png(40, 30, png::Compression::Fast);
        let image = prepare(&bytes, "image a.png", &dir).unwrap();
        assert_eq!((image.width, image.height, image.about.as_str()), (40, 30, "image a.png, 40×30"));
        assert_eq!(image.data.as_deref(), Some(bytes.as_slice()), "its own bytes");
        assert_eq!(std::fs::read(&image.file).unwrap(), bytes);
        assert_eq!(prepare(&bytes, "image b.png", &dir).unwrap().file, image.file, "the same file for the same image");
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&image.file).unwrap().permissions()) & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_big_png_is_shrunk_and_the_model_told_how_much() {
        let dir = folder("big");
        let image = prepare(&png(3000, 1500, png::Compression::Fast), "a screenshot", &dir).unwrap();
        assert_eq!((image.width, image.height), (2000, 1000));
        assert_eq!(image.about, "a screenshot, 3000×1500, shown at 2000×1000: multiply what you see by 1.50 for its own pixels");
        assert_eq!(size_of(ImageKind::Png, image.data.as_deref().unwrap()), Some((2000, 1000)), "it is what it says");
        assert!(image.data.as_deref().unwrap().len() <= HEAVIEST);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn what_cant_be_shown_is_said_why_never_sent() {
        let dir = folder("refused");
        assert!(prepare(b"not an image", "notes.txt", &dir).unwrap_err().contains("isn't an image rein can show"));
        let huge_jpeg = [&[0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0][..], &[0xff, 0xc0, 0, 11, 8, 0x2e, 0xe0, 0x2e, 0xe0, 3, 0, 0, 0]].concat();
        assert!(prepare(&huge_jpeg, "photo.jpg", &dir).unwrap_err().contains("12000×12000"), "only PNGs are shrunk");
        let mut broken = png(3000, 10, png::Compression::Fast);
        broken.truncate(60);
        assert!(prepare(&broken, "cut.png", &dir).unwrap_err().contains("couldn't be shrunk"));
        assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none(), "nothing kept for what isn't shown");
    }

    #[test]
    fn shrinking_averages_what_each_pixel_covers() {
        // A 4×2 grey image of 0 and 100 in columns, halved: 50s.
        let pixels = [0, 100, 0, 100, 0, 100, 0, 100];
        assert_eq!(area_average(&pixels, 4, 2, 1, 2, 1), [50, 50]);
    }
}
