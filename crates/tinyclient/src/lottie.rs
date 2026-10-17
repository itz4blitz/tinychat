//! Native Lottie stickers, rendered off the UI thread into the existing frame cache.
//! tlottie never resolves external resources. Input, canvas and retained frames are bounded.

use crate::frames::{self, Animation, Limits, Sniff};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_INPUT: usize = 2 << 20;
const MAX_EDGE: u32 = 512;

pub fn candidate(bytes: &[u8]) -> bool {
    bytes.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'{')
}

pub fn read(path: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0; 64];
    let n = file.read(&mut head).ok()?;
    if !candidate(&head[..n]) {
        return None;
    }
    use std::io::Seek;
    file.rewind().ok()?;
    let mut bytes = Vec::new();
    file.take((MAX_INPUT + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_INPUT).then_some(bytes)
}

pub fn sniff(bytes: &[u8]) -> Option<Sniff> {
    if bytes.len() > MAX_INPUT || !candidate(bytes) {
        return None;
    }
    let header: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let width = u32::try_from(header["w"].as_u64()?).ok()?;
    let height = u32::try_from(header["h"].as_u64()?).ok()?;
    let rate = header["fr"].as_f64()?;
    let count = (header["op"].as_f64()? - header["ip"].as_f64()?).ceil();
    header["layers"].as_array()?;
    if !(1..=4096).contains(&width)
        || !(1..=4096).contains(&height)
        || !(0.1..=240.).contains(&rate)
        || !(1.0..=1000.0).contains(&count)
    {
        return None;
    }
    Some(Sniff {
        width,
        height,
        frames: count as usize,
        plays: None,
    })
}

pub fn decode(
    bytes: &[u8],
    edge: u32,
    crop: Option<f32>,
    limits: Option<Limits>,
) -> Option<Animation> {
    let sniff = sniff(bytes)?;
    let parse_limits = tlottie::Limits {
        max_input_bytes: MAX_INPUT,
        max_dimension: 4096,
        max_parse_bytes: 64 << 20,
        ..Default::default()
    };
    let composition = tlottie::Composition::parse(bytes, &parse_limits).ok()?;
    let moving = limits.is_some() && !composition.is_static() && sniff.frames > 1;
    let edge = edge.clamp(1, MAX_EDGE).min(sniff.width.max(sniff.height));
    let (width, height, step) = if moving {
        let plan = frames::plan(&sniff, edge, crop, limits?)?;
        (plan.width, plan.height, plan.step)
    } else {
        let (width, height) = frames::scaled((sniff.width, sniff.height), edge, crop);
        (width, height, 1)
    };
    // Render the full aspect ratio, then keep the top rows for CoverTop.
    let full_height = ((u64::from(sniff.height) * u64::from(width) / u64::from(sniff.width)).max(1)
        as u32)
        .max(height);
    let count = if moving { sniff.frames } else { 1 };
    let rate = f64::from(composition.frame_rate);
    let first = composition.in_point;
    let mut renderer = tlottie::CPURenderer::new(composition);
    let mut pixels = vec![0u32; width as usize * full_height as usize];
    let mut kept = Vec::new();
    let started = Instant::now();
    for at in (0..count).step_by(step) {
        if started.elapsed() > Duration::from_secs(2) {
            return None;
        }
        renderer
            .render(
                first + at as f32,
                &mut pixels,
                width,
                full_height,
                tlottie::RenderOptions::default(),
            )
            .ok()?;
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for &pixel in pixels.iter().take(width as usize * height as usize) {
            let alpha = pixel >> 24;
            let straight = |channel: u32| {
                if alpha == 0 {
                    0
                } else {
                    ((channel * 255 + alpha / 2) / alpha).min(255) as u8
                }
            };
            bgra.extend([
                straight((pixel >> 16) & 255),
                straight((pixel >> 8) & 255),
                straight(pixel & 255),
                alpha as u8,
            ]);
        }
        let held = Duration::from_secs_f64((step.min(count - at)) as f64 / rate);
        kept.push((bgra, held));
    }
    Some(Animation {
        width,
        height,
        frames: kept,
        plays: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<u8> {
        include_bytes!("../tests/fixtures/moving.lottie").to_vec()
    }
    #[test]
    fn native_frames_move_and_have_straight_bgra_and_correct_timing() {
        let bytes = fixture();
        let image = decode(&bytes, 16, None, Some(frames::SMALL)).unwrap();
        assert_eq!((image.width, image.height, image.frames.len()), (16, 16, 4));
        assert_ne!(image.frames[0].0, image.frames[3].0);
        assert!(
            image.frames[0]
                .0
                .chunks_exact(4)
                .any(|p| p[0] == 0 && p[2] == 255 && (120..=130).contains(&p[3]))
        );
        assert!(
            image
                .frames
                .iter()
                .all(|(_, delay)| *delay == Duration::from_millis(100))
        );
        assert!(image.plays.is_none());
        let still = decode(&bytes, 16, None, None).unwrap();
        assert_eq!(still.frames.len(), 1);
        assert_eq!(still.frames[0].0, image.frames[0].0);
    }
    #[test]
    fn cropped_portrait_stickers_keep_the_original_aspect_ratio() {
        let mut value: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        value["h"] = 32.into();
        let bytes = serde_json::to_vec(&value).unwrap();
        let full = decode(&bytes, 32, None, None).unwrap();
        let cropped = decode(&bytes, 32, Some(0.5), None).unwrap();
        assert_eq!((full.width, full.height), (16, 32));
        assert_eq!((cropped.width, cropped.height), (16, 8));
        assert_eq!(cropped.frames[0].0, full.frames[0].0[..16 * 8 * 4]);
    }

    #[test]
    fn invalid_and_oversized_inputs_are_rejected_and_frame_budgets_hold() {
        for bytes in [b"{}".as_slice(), b"{not json}", b"[]"] {
            assert!(decode(bytes, 16, None, None).is_none());
        }
        let mut value: serde_json::Value = serde_json::from_slice(&fixture()).unwrap();
        value["w"] = 100000.into();
        assert!(sniff(&serde_json::to_vec(&value).unwrap()).is_none());
        let limits = Limits {
            bytes: 16 * 16 * 4 * 2,
            frames: 2,
        };
        let image = decode(&fixture(), 16, None, Some(limits)).unwrap();
        assert!(image.bytes() <= limits.bytes && image.frames.len() <= limits.frames);
        assert_eq!(
            image.frames.iter().map(|(_, d)| *d).sum::<Duration>(),
            Duration::from_millis(400)
        );
    }
}
