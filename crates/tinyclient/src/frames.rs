//! Moving pictures: animated GIF, WebP, APNG and (with the GUI) Lottie, decoded at the size they
//! are drawn. A file's header is read first, without decoding, for its canvas, frame count and
//! how many times it plays; that decides the size and how many frames are kept so one
//! animation never passes its [`Limits`]. Frames are scaled down one at a time as they are
//! decoded, so a large GIF is never held whole.
//!
//! Plain Rust, no window: the shell turns what this returns into GPUI's multi-frame image.

use image::codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder};
use image::{imageops, AnimationDecoder, RgbaImage};
use std::io::Cursor;
use std::time::Duration;

/// What one animation may cost decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bytes of every kept frame together, four a pixel.
    pub bytes: usize,
    /// Frames kept; a longer file keeps every second (third, ...) frame and holds each longer.
    pub frames: usize,
}

/// A message's picture, sticker or link preview: the size of a 400 x 300 pt box at 2x holds
/// eight frames whole, so a long or large GIF is drawn a little softer instead.
pub const MESSAGE: Limits = Limits { bytes: 16 << 20, frames: 120 };
/// A custom emoji, an avatar, a picker cell: many are on screen at once.
pub const SMALL: Limits = Limits { bytes: 2 << 20, frames: 120 };

/// A file with more frames than this is drawn still: decoding it would take seconds.
const MOST_FRAMES: usize = 1_000;
/// A canvas larger than this is drawn still: one frame of it is 64 MB before scaling.
const MOST_PIXELS: u64 = 4096 * 4096;

/// What a moving picture's header says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sniff {
    pub width: u32,
    pub height: u32,
    pub frames: usize,
    /// How many times it plays through: `None` is forever.
    pub plays: Option<u32>,
}

/// Reads a file's header for whether it moves. `None` for a still picture, one frame, or a
/// file that is not a supported animation format.
pub fn sniff(bytes: &[u8]) -> Option<Sniff> {
    let found = if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        gif(bytes)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        webp(bytes)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        apng(bytes)
    } else {
        #[cfg(feature = "gui")]
        { crate::lottie::sniff(bytes) }
        #[cfg(not(feature = "gui"))]
        { None }
    }?;
    (found.frames > 1 && found.width > 0 && found.height > 0).then_some(found)
}

/// Walks a GIF's blocks without decoding: its screen, its images, and the loop count of its
/// NETSCAPE block. With no such block a browser plays it once, so this does too.
fn gif(bytes: &[u8]) -> Option<Sniff> {
    let width = u32::from(u16::from_le_bytes([*bytes.get(6)?, *bytes.get(7)?]));
    let height = u32::from(u16::from_le_bytes([*bytes.get(8)?, *bytes.get(9)?]));
    let table = |packed: u8| if packed & 0x80 != 0 { 3usize << ((packed & 7) + 1) } else { 0 };
    let mut at = 13 + table(*bytes.get(10)?);
    // Data sub-blocks: a length byte, that many bytes, until a zero length.
    let skip = |mut at: usize| -> Option<usize> {
        loop {
            let length = *bytes.get(at)? as usize;
            at += 1;
            if length == 0 {
                return Some(at);
            }
            at += length;
        }
    };
    let (mut frames, mut plays) = (0usize, Some(1));
    // A truncated file counts what it has.
    while let Some(&block) = bytes.get(at) {
        match block {
            0x21 => {
                let label = *bytes.get(at + 1)?;
                let looping = bytes.get(at + 3..at + 14).is_some_and(|id| id == b"NETSCAPE2.0" || id == b"ANIMEXTS1.0");
                if label == 0xff && looping && bytes.get(at + 14) == Some(&3) && bytes.get(at + 15) == Some(&1) {
                    let count = u16::from_le_bytes([*bytes.get(at + 16)?, *bytes.get(at + 17)?]);
                    // The count is of repeats after the first play; zero is forever.
                    plays = (count != 0).then(|| u32::from(count) + 1);
                }
                let Some(next) = skip(at + 2) else { break };
                at = next;
            }
            0x2c => {
                frames += 1;
                let Some(&packed) = bytes.get(at + 9) else { break };
                // The descriptor, its color table, the code size byte, then the data.
                let Some(next) = skip(at + 10 + table(packed) + 1) else { break };
                at = next;
            }
            _ => break,
        }
    }
    Some(Sniff { width, height, frames, plays })
}

/// Walks a WebP's chunks: `VP8X` says whether it moves and its canvas, `ANIM` its loop
/// count, and each `ANMF` is a frame.
fn webp(bytes: &[u8]) -> Option<Sniff> {
    let (mut found, mut moving, mut at) = (Sniff { width: 0, height: 0, frames: 0, plays: None }, false, 12);
    while let Some(head) = bytes.get(at..at + 8) {
        let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        let data = bytes.get(at + 8..).unwrap_or(&[]);
        let le24 = |d: &[u8]| u32::from(d[0]) | u32::from(d[1]) << 8 | u32::from(d[2]) << 16;
        match &head[..4] {
            b"VP8X" if data.len() >= 10 => {
                moving = data[0] & 0x02 != 0;
                found.width = le24(&data[4..7]) + 1;
                found.height = le24(&data[7..10]) + 1;
            }
            b"ANIM" if data.len() >= 6 => {
                let count = u16::from_le_bytes([data[4], data[5]]);
                found.plays = (count != 0).then_some(u32::from(count));
            }
            b"ANMF" => found.frames += 1,
            // A plain VP8 or VP8L file is one picture.
            b"VP8 " | b"VP8L" if !moving => return None,
            _ => {}
        }
        at += 8 + size + (size & 1);
    }
    moving.then_some(found)
}

/// Reads a PNG's chunks up to its first image data: an `acTL` there makes it an APNG.
fn apng(bytes: &[u8]) -> Option<Sniff> {
    let mut at = 8;
    let (mut width, mut height) = (0, 0);
    while let Some(head) = bytes.get(at..at + 8) {
        let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let data = bytes.get(at + 8..at + 8 + size)?;
        let be = |d: &[u8]| u32::from_be_bytes([d[0], d[1], d[2], d[3]]);
        match &head[4..] {
            b"IHDR" if size >= 8 => (width, height) = (be(&data[0..4]), be(&data[4..8])),
            b"acTL" if size >= 8 => {
                let plays = be(&data[4..8]);
                return Some(Sniff { width, height, frames: be(&data[0..4]) as usize, plays: (plays != 0).then_some(plays) });
            }
            b"IDAT" => return None,
            _ => {}
        }
        at += 12 + size;
    }
    None
}

/// The size an animation is decoded at and which of its frames are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub width: u32,
    pub height: u32,
    /// Every `step`th frame is kept and held for the frames it stands for.
    pub step: usize,
}

/// The pixels of a `canvas` scaled to `edge` on its longer side, then cut to `crop` times its
/// width from the top.
pub(crate) fn scaled(canvas: (u32, u32), edge: u32, crop: Option<f32>) -> (u32, u32) {
    let (w, h) = (u64::from(canvas.0.max(1)), u64::from(canvas.1.max(1)));
    let edge = u64::from(edge.max(1));
    let (width, height) = if w >= h { (edge, (h * edge / w).max(1)) } else { ((w * edge / h).max(1), edge) };
    let height = match crop {
        Some(ratio) => ((width as f32 * ratio).round() as u64).clamp(1, height),
        None => height,
    };
    (width as u32, height as u32)
}

/// Fits an animation in its limits: never larger than the file or than `edge`; then smaller,
/// down to half of that; then fewer frames. `None` when even that will not fit: it is drawn
/// still.
pub fn plan(sniff: &Sniff, edge: u32, crop: Option<f32>, limits: Limits) -> Option<Plan> {
    if sniff.frames > MOST_FRAMES || u64::from(sniff.width) * u64::from(sniff.height) > MOST_PIXELS {
        return None;
    }
    let first = edge.min(sniff.width.max(sniff.height)).max(1);
    let floor = (first / 2).max(64).min(first);
    let (mut edge, mut step) = (first, sniff.frames.div_ceil(limits.frames.max(2)).max(1));
    loop {
        let (width, height) = scaled((sniff.width, sniff.height), edge, crop);
        let kept = sniff.frames.div_ceil(step);
        if kept < 2 {
            return None;
        }
        let bytes = width as usize * height as usize * 4 * kept;
        if bytes <= limits.bytes {
            return Some(Plan { width, height, step });
        }
        if edge > floor {
            // Pixels go with the square of the edge.
            let shrink = (limits.bytes as f64 / bytes as f64).sqrt();
            edge = ((f64::from(edge) * shrink).floor() as u32).clamp(floor, edge - 1);
        } else {
            step += 1;
        }
    }
}

/// Decoded frames, ready for the window.
pub struct Animation {
    pub width: u32,
    pub height: u32,
    /// Blue, green, red, alpha (straight), and how long each is shown.
    pub frames: Vec<(Vec<u8>, Duration)>,
    pub plays: Option<u32>,
}

impl Animation {
    pub fn bytes(&self) -> usize {
        self.frames.iter().map(|(pixels, _)| pixels.len()).sum()
    }
}

/// How long a frame is shown. Browsers show a frame of 10 ms or less for 100 ms, and files
/// are made for browsers.
fn shown_for(delay: image::Delay) -> Duration {
    let (numerator, denominator) = delay.numer_denom_ms();
    let ms = u64::from(numerator) / u64::from(denominator.max(1));
    Duration::from_millis(if ms <= 10 { 100 } else { ms })
}

/// Decodes the frames of a moving picture per its plan. `None` when it does not move, does not
/// fit, or will not decode.
pub fn decode(bytes: &[u8], edge: u32, crop: Option<f32>, limits: Limits) -> Option<Animation> {
    #[cfg(feature = "gui")]
    if crate::lottie::candidate(bytes) { return crate::lottie::decode(bytes, edge, crop, Some(limits)); }

    let sniff = sniff(bytes)?;
    let plan = plan(&sniff, edge, crop, limits)?;
    let source = || Cursor::new(bytes);
    let frames = if bytes.starts_with(b"GIF") {
        GifDecoder::new(source()).ok()?.into_frames()
    } else if bytes.starts_with(b"RIFF") {
        WebPDecoder::new(source()).ok()?.into_frames()
    } else {
        PngDecoder::new(source()).ok()?.apng().ok()?.into_frames()
    };
    let mut kept: Vec<(Vec<u8>, Duration)> = Vec::with_capacity(sniff.frames.div_ceil(plan.step));
    for (at, frame) in frames.enumerate() {
        let frame = frame.ok()?;
        let delay = shown_for(frame.delay());
        if at % plan.step != 0 {
            if let Some((_, held)) = kept.last_mut() {
                *held += delay;
            }
            continue;
        }
        kept.push((pixels(frame.into_buffer(), plan), delay));
    }
    if kept.len() < 2 {
        return None;
    }
    Some(Animation { width: plan.width, height: plan.height, frames: kept, plays: sniff.plays })
}

/// One composed frame scaled to the plan, top rows kept, in the window's byte order.
fn pixels(frame: RgbaImage, plan: Plan) -> Vec<u8> {
    let (w, h) = (frame.width().max(1), frame.height().max(1));
    // Scaled to the plan's width, keeping the shape; the plan's height may cut the bottom.
    let full = ((u64::from(h) * u64::from(plan.width) / u64::from(w)).max(1) as u32).max(plan.height);
    let mut frame = if (w, h) == (plan.width, full) { frame } else { imageops::resize(&frame, plan.width, full, imageops::FilterType::Triangle) };
    let mut out = Vec::with_capacity(plan.width as usize * plan.height as usize * 4);
    for row in frame.rows_mut().take(plan.height as usize) {
        for pixel in row {
            out.extend([pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn a_header_says_whether_a_file_moves_and_how_often_it_plays() {
        let at = |name| sniff(&fixture(name));
        assert_eq!(at("forever.gif"), Some(Sniff { width: 64, height: 32, frames: 4, plays: None }));
        // No NETSCAPE block: once, as a browser plays it.
        assert_eq!(at("once.gif"), Some(Sniff { width: 64, height: 32, frames: 2, plays: Some(1) }));
        assert_eq!(at("twice.webp"), Some(Sniff { width: 64, height: 32, frames: 3, plays: Some(2) }));
        assert_eq!(at("apng.png"), Some(Sniff { width: 64, height: 32, frames: 3, plays: Some(1) }));
        assert_eq!(at("still.webp"), None);
        assert_eq!(sniff(b"GIF89a"), None);
        assert_eq!(sniff(b"not a picture"), None);
    }

    #[test]
    fn frames_are_decoded_at_the_size_drawn_with_their_own_delays() {
        let gif = decode(&fixture("forever.gif"), 16, None, MESSAGE).unwrap();
        assert_eq!((gif.width, gif.height, gif.frames.len(), gif.plays), (16, 8, 4, None));
        let delays: Vec<u64> = gif.frames.iter().map(|(_, held)| held.as_millis() as u64).collect();
        // The last frame says 10 ms, which browsers show for 100.
        assert_eq!(delays, [50, 50, 100, 100]);
        // Blue, green, red: the left half is each frame's color, the right half black.
        let left = |frame: usize| gif.frames[frame].0[..4].to_vec();
        assert_eq!([left(0), left(1), left(2)], [vec![0, 0, 255, 255], vec![0, 255, 0, 255], vec![255, 0, 0, 255]]);
        let right = &gif.frames[0].0[(16 - 1) * 4..16 * 4];
        assert!(right[..3].iter().all(|channel| *channel < 40), "{right:?}");
        assert!(gif.frames.iter().all(|(pixels, _)| pixels.len() == 16 * 8 * 4));

        let webp = decode(&fixture("twice.webp"), 32, None, SMALL).unwrap();
        assert_eq!((webp.width, webp.height, webp.frames.len(), webp.plays), (32, 16, 3, Some(2)));
        let apng = decode(&fixture("apng.png"), 4096, None, SMALL).unwrap();
        // Never larger than the file.
        assert_eq!((apng.width, apng.height, apng.frames.len(), apng.plays), (64, 32, 3, Some(1)));
        // A crop keeps the top: a quarter as tall as wide.
        let top = decode(&fixture("forever.gif"), 16, Some(0.25), MESSAGE).unwrap();
        assert_eq!((top.width, top.height, top.frames[0].0.len()), (16, 4, 16 * 4 * 4));
        assert!(decode(&fixture("still.webp"), 64, None, MESSAGE).is_none());
    }

    #[test]
    fn a_long_or_large_animation_is_drawn_smaller_then_with_fewer_frames_to_fit() {
        let gif = |width, height, frames| Sniff { width, height, frames, plays: None };
        // 40 frames of a 498 x 280 GIF wanted at 800 px: never more than the file has, then
        // smaller until it fits 16 MB.
        let plan_of = |sniff: Sniff, edge| plan(&sniff, edge, None, MESSAGE).unwrap();
        let fits = plan_of(gif(498, 280, 40), 800);
        assert_eq!(fits.step, 1);
        assert!(fits.width < 498 && fits.width >= 249, "{fits:?}");
        assert!(fits.width as usize * fits.height as usize * 4 * 40 <= MESSAGE.bytes);
        // Small enough already: the file's own size, every frame.
        assert_eq!(plan_of(gif(220, 124, 30), 440), Plan { width: 220, height: 124, step: 1 });
        // More frames than kept: every second one.
        assert_eq!(plan_of(gif(100, 100, 300), 100).step, 3);
        // Half the size is as small as it goes; past that, frames go.
        let long = plan(&gif(800, 800, 200), 800, None, SMALL).unwrap();
        assert_eq!(long.width, 400);
        assert!(long.step > 2 && 400 * 400 * 4 * 200usize.div_ceil(long.step) <= SMALL.bytes, "{long:?}");
        // Nothing that fits keeps two frames: still.
        assert_eq!(plan(&gif(4000, 4000, 2), 4000, None, SMALL), None);
        assert_eq!(plan(&gif(5000, 5000, 4), 64, None, MESSAGE), None);
        assert_eq!(plan(&gif(64, 64, 5000), 64, None, MESSAGE), None);
    }

    #[test]
    fn kept_frames_hold_for_the_ones_they_stand_for() {
        // Four frames kept two at a time: 50 + 50, then 100 + 100.
        let limits = Limits { bytes: 1 << 20, frames: 2 };
        let gif = decode(&fixture("forever.gif"), 16, None, limits).unwrap();
        let delays: Vec<u64> = gif.frames.iter().map(|(_, held)| held.as_millis() as u64).collect();
        assert_eq!(delays, [100, 200]);
        assert_eq!(gif.bytes(), 2 * 16 * 8 * 4);
    }
}
