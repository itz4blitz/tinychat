//! Pictures for the rows on screen. A file is decoded off the main thread, by the system's own
//! decoder, straight to the size it is shown at: a 4000 px photo in a 198 pt tile costs 0.6 MB,
//! not 48. A moving picture (GIF, WebP, APNG) is decoded to frames at that size too, within
//! its [`Limits`] and the window's [`anim::MESSAGES`] budget, and keeps only its first frame
//! once it scrolls away. At most [`DECODES`] files are being read at once, and a row that
//! scrolled away before its turn is never read at all. Stills that scroll away are dropped once
//! the total passes [`BUDGET`].

use crate::frames::{self, Limits};
use crate::gui::anim::{self, Play};
use gpui::{App, Context, RenderImage, Window};
use image::{Frame, ImageBuffer};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Most decoded bytes held. Pictures on screen are never dropped to get under it.
pub const BUDGET: usize = 32 * 1024 * 1024;

/// Most files being decoded at once. Each holds its pixels twice for a moment (the system's
/// and ours), so this bounds the peak as well as the threads taken.
pub const DECODES: usize = 2;

#[cfg(target_os = "macos")]
type Ref = *const c_void;

#[cfg(target_os = "macos")]
#[repr(C)]
struct Opaque {
    _private: [u8; 0],
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    static kCFBooleanTrue: Ref;
    fn CFURLCreateFromFileSystemRepresentation(allocator: Ref, path: *const u8, len: isize, directory: u8) -> Ref;
    fn CFDictionaryCreate(
        allocator: Ref,
        keys: *const Ref,
        values: *const Ref,
        count: isize,
        key_callbacks: *const Opaque,
        value_callbacks: *const Opaque,
    ) -> Ref;
    fn CFNumberCreate(allocator: Ref, kind: isize, value: *const c_void) -> Ref;
    fn CFRelease(object: Ref);
}

#[cfg(target_os = "macos")]
#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    static kCGImageSourceCreateThumbnailFromImageAlways: Ref;
    static kCGImageSourceCreateThumbnailWithTransform: Ref;
    static kCGImageSourceThumbnailMaxPixelSize: Ref;
    fn CGImageSourceCreateWithURL(url: Ref, options: Ref) -> Ref;
    fn CGImageSourceCreateThumbnailAtIndex(source: Ref, index: usize, options: Ref) -> Ref;
    fn CGImageSourceGetCount(source: Ref) -> usize;
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    static kCGColorSpaceSRGB: Ref;
    fn CGImageGetWidth(image: Ref) -> usize;
    fn CGImageGetHeight(image: Ref) -> usize;
    fn CGColorSpaceCreateWithName(name: Ref) -> Ref;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: Ref,
        bitmap_info: u32,
    ) -> Ref;
    fn CGContextDrawImage(context: Ref, rect: Rect, image: Ref);
}

#[cfg(target_os = "macos")]
/// `kCFNumberSInt32Type`
const SINT32: isize = 3;
#[cfg(target_os = "macos")]
/// `kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little`: BGRA in memory.
const BGRA: u32 = 2 | (2 << 12);

#[cfg(target_os = "macos")]
/// Releases a Core Foundation object when it goes out of scope.
struct Owned(Ref);

#[cfg(target_os = "macos")]
impl Owned {
    fn new(object: Ref) -> Option<Owned> {
        (!object.is_null()).then_some(Owned(object))
    }
}

#[cfg(target_os = "macos")]
impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a Create function, is not null, and is released once.
        unsafe { CFRelease(self.0) }
    }
}

pub struct Decoded {
    pub width: u32,
    pub height: u32,
    /// Straight alpha, four bytes a pixel.
    pub bgra: Vec<u8>,
}

/// Reads the picture at `path` no larger than `edge` pixels on its longer side, upright. The
/// system decodes at a reduced size where the format allows, so a large photo is never held
/// whole. `crop` keeps only the top of a very tall picture: that many times its width.
/// `None` when the file is missing or is not a picture.
pub fn decode(path: &Path, edge: u32, crop: Option<f32>) -> Option<Decoded> {
    if !looks_like_a_picture(path) {
        return None;
    }
    #[cfg(target_os = "macos")]
    return decode_imageio(path, edge, crop);
    #[cfg(not(target_os = "macos"))]
    return decode_portable(path, edge, crop);
}

#[cfg(not(target_os = "macos"))]
fn decode_portable(path: &Path, edge: u32, crop: Option<f32>) -> Option<Decoded> {
    let mut img = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()?.into_rgba8();
    let (w, h) = (img.width(), img.height());
    // Cache paths have no suffix, and a requested thumbnail must not enlarge a small source.
    let edge = edge.clamp(1, 8192).min(w.max(h));
    let (nw, nh) = if w >= h {
        let nh = ((h as u64 * edge as u64) / w as u64).max(1) as u32;
        (edge, nh)
    } else {
        let nw = ((w as u64 * edge as u64) / h as u64).max(1) as u32;
        (nw, edge)
    };
    if (nw, nh) != (w, h) {
        img = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
    }
    let (width, height) = (img.width() as usize, img.height() as usize);
    let height = match crop {
        Some(ratio) => ((width as f32 * ratio).round() as usize).clamp(1, height),
        None => height,
    };
    let mut bgra = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let pixel = img.get_pixel(x as u32, y as u32);
            bgra.extend([pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    Some(Decoded { width: width as u32, height: height as u32, bgra })
}

#[cfg(target_os = "macos")]
fn decode_imageio(path: &Path, edge: u32, crop: Option<f32>) -> Option<Decoded> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let edge = edge.clamp(1, 8192) as i32;
    // SAFETY: every pointer handed to the system is either a constant it exports or an object
    // created just above and kept alive by an `Owned` until after the call. The pixel buffer is
    // `width * height * 4` bytes, which is what the bitmap context is told it may write.
    unsafe {
        let null = std::ptr::null();
        let url = Owned::new(CFURLCreateFromFileSystemRepresentation(null, bytes.as_ptr(), bytes.len() as isize, 0))?;
        let source = Owned::new(CGImageSourceCreateWithURL(url.0, null))?;
        // A file that is not a picture holds none; asking it for one anyway is a trap.
        if CGImageSourceGetCount(source.0) == 0 {
            return None;
        }
        let size = Owned::new(CFNumberCreate(null, SINT32, (&raw const edge).cast()))?;
        let keys = [
            kCGImageSourceCreateThumbnailFromImageAlways,
            kCGImageSourceCreateThumbnailWithTransform,
            kCGImageSourceThumbnailMaxPixelSize,
        ];
        let values = [kCFBooleanTrue, kCFBooleanTrue, size.0];
        let options = Owned::new(CFDictionaryCreate(
            null,
            keys.as_ptr(),
            values.as_ptr(),
            keys.len() as isize,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        ))?;
        // Index 0: an animated file gives its first frame and the rest is never touched.
        let image = Owned::new(CGImageSourceCreateThumbnailAtIndex(source.0, 0, options.0))?;
        let (width, height) = (CGImageGetWidth(image.0), CGImageGetHeight(image.0));
        if width == 0 || height == 0 {
            return None;
        }
        let mut bgra = vec![0u8; width * height * 4];
        let space = Owned::new(CGColorSpaceCreateWithName(kCGColorSpaceSRGB))?;
        let context = Owned::new(CGBitmapContextCreate(bgra.as_mut_ptr().cast(), width, height, 8, width * 4, space.0, BGRA))?;
        let all = Rect { x: 0., y: 0., width: width as f64, height: height as f64 };
        CGContextDrawImage(context.0, all, image.0);
        drop(context);

        // The system hands back premultiplied color; the renderer wants straight.
        for pixel in bgra.as_chunks_mut::<4>().0 {
            let alpha = u32::from(pixel[3]);
            if alpha != 0 && alpha != 255 {
                for channel in &mut pixel[..3] {
                    *channel = (u32::from(*channel) * 255 / alpha).min(255) as u8;
                }
            }
        }
        // Rows run top to bottom, so what is kept is the top.
        let height = match crop {
            Some(ratio) => ((width as f32 * ratio).round() as usize).clamp(1, height),
            None => height,
        };
        bgra.truncate(width * height * 4);
        Some(Decoded { width: width as u32, height: height as u32, bgra })
    }
}

/// True when the file is there and starts the way a picture the system reads starts.
fn looks_like_a_picture(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 12];
    let read = std::fs::File::open(path).and_then(|mut file| file.read(&mut head)).unwrap_or(0);
    let head = &head[..read];
    let starts = |magic: &[u8]| head.starts_with(magic);
    starts(b"\x89PNG\r\n\x1a\n")
        || starts(b"\xff\xd8\xff")
        || starts(b"GIF8")
        || starts(b"BM")
        || starts(b"II*\0")
        || starts(b"MM\0*")
        || starts(b"\0\0\x01\0")
        || (starts(b"RIFF") && head.get(8..12) == Some(b"WEBP"))
        // HEIC and AVIF: an ISO media file says so in its second word.
        || head.get(4..8) == Some(b"ftyp")
}

fn render_image(decoded: Decoded) -> Option<Arc<RenderImage>> {
    let buffer = ImageBuffer::from_raw(decoded.width, decoded.height, decoded.bgra)?;
    Some(Arc::new(RenderImage::new(vec![Frame::new(buffer)])))
}

/// Whether a picture moves, and whether its frames are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moves {
    No,
    /// Every kept frame is held, paid for from [`anim::MESSAGES`].
    Playing,
    /// It moves, but only its first frame is held: it will not play here, it was off screen,
    /// or there was no room.
    Still,
}

/// The largest file read whole to look for frames. A moving picture that large is drawn still.
const MOST_READ: u64 = 32 << 20;

/// Reads the picture at `path` for the window: its frames, at most `limits`, when it moves and
/// they are wanted and fit in the budget; else its first frame.
pub fn load(path: &Path, edge: u32, crop: Option<f32>, frames: Option<Limits>) -> Option<(Arc<RenderImage>, Moves)> {
    let moving = std::fs::metadata(path)
        .ok()
        .filter(|meta| meta.len() <= MOST_READ && might_move(path))
        .and_then(|_| std::fs::read(path).ok())
        .filter(|bytes| frames::sniff(bytes).is_some());
    if let (Some(bytes), Some(limits)) = (&moving, frames) {
        if let Some(image) = frames::decode(bytes, edge, crop, limits).filter(|found| anim::MESSAGES.take(found.bytes())).and_then(anim::render_image) {
            return Some((image, Moves::Playing));
        }
    }
    // The first frame, by the same reader a still picture uses.
    let still = decode(path, edge, crop).and_then(render_image)?;
    Some((still, if moving.is_some() { Moves::Still } else { Moves::No }))
}

/// Whether the start of a file says it may move: a GIF, a WebP marked animated, a PNG with an
/// animation chunk before its pixels. Only those are read whole.
fn might_move(path: &Path) -> bool {
    use std::io::Read;
    let mut head = vec![0u8; 4096];
    let read = std::fs::File::open(path).and_then(|mut file| file.read(&mut head)).unwrap_or(0);
    let head = &head[..read];
    head.starts_with(b"GIF8")
        || (head.starts_with(b"RIFF") && head.get(8..16) == Some(b"WEBPVP8X") && head.get(20).is_some_and(|flags| flags & 0x02 != 0))
        || (head.starts_with(b"\x89PNG") && head.windows(4).take_while(|chunk| *chunk != b"IDAT").any(|chunk| chunk == b"acTL"))
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Slot {
    path: Arc<str>,
    edge: u32,
    /// Height kept per thousand of width; 0 keeps it all.
    crop: u32,
}

impl Slot {
    fn new(path: &str, edge: u32, crop: Option<f32>) -> Slot {
        // A few sizes rather than every size: a box that differs by a pixel reuses the decode.
        let edge = edge.div_ceil(32) * 32;
        Slot { path: path.into(), edge, crop: crop.map_or(0, |ratio| (ratio * 1000.) as u32) }
    }
}

/// What a decode produced, to hand back to [`Images::store`].
pub struct Loaded {
    slot: Slot,
    image: Option<(Arc<RenderImage>, Moves)>,
}

enum Held {
    /// Asked for; `asked` is the last draw that wanted it.
    Loading { asked: u64 },
    Failed,
    Ready {
        image: Arc<RenderImage>,
        /// Bytes counted against [`BUDGET`]; frames are counted in [`anim::MESSAGES`].
        bytes: usize,
        used: u64,
        moves: Moves,
        /// Its frames are being read again, last asked for at this draw.
        upgrading: Option<u64>,
        /// No sooner than this: the last try found no room.
        retry: Option<Instant>,
    },
}

/// The decoded pictures of one pane.
pub struct Images {
    held: HashMap<Slot, Held>,
    bytes: usize,
    /// Counts draws, to tell what is on screen now from what was.
    frame: u64,
    /// Waiting for a turn to be decoded, oldest ask first, and whether its frames are wanted.
    queue: VecDeque<(Slot, bool)>,
    decoding: usize,
    /// What one moving picture here may hold.
    limits: Limits,
    /// First frames that stood in for an animation, whose textures go at the next sweep.
    dropped: Vec<Arc<RenderImage>>,
    /// Pictures dropped to stay under the budget, for measuring.
    pub evicted: usize,
    /// The most bytes ever held at once, for measuring.
    pub peak: usize,
}

impl Default for Images {
    fn default() -> Images {
        Images::with_limits(frames::MESSAGE)
    }
}

pub enum Picture {
    Ready(Arc<RenderImage>),
    Loading,
    Failed,
}

/// How long a moving picture that found no room waits before it asks again.
const RETRY: Duration = Duration::from_secs(1);

impl Images {
    /// A cache whose moving pictures each hold at most `limits`: a picker's cells are small.
    pub fn with_limits(limits: Limits) -> Images {
        Images {
            held: HashMap::new(),
            bytes: 0,
            frame: 0,
            queue: VecDeque::new(),
            decoding: 0,
            limits,
            dropped: Vec::new(),
            evicted: 0,
            peak: 0,
        }
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    #[cfg_attr(not(feature = "gui-shot"), allow(dead_code))]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Bytes of frames held here, for measuring.
    #[cfg_attr(not(feature = "gui-shot"), allow(dead_code))]
    pub fn frame_bytes(&self) -> usize {
        self.held.values().map(|held| match held {
            Held::Ready { image, moves: Moves::Playing, .. } => anim::bytes_of(image),
            _ => 0,
        }).sum()
    }

    /// Nothing is waiting to be decoded or being decoded.
    #[cfg_attr(not(feature = "gui-shot"), allow(dead_code))]
    pub fn settled(&self) -> bool {
        self.decoding == 0 && self.queue.is_empty()
    }

    /// Whether the picture decoded as `image` moves, held whole or not: what wears a GIF badge.
    pub fn moves(&self, image: &RenderImage) -> bool {
        image.frame_count() > 1
            || self.held.values().any(|held| matches!(held, Held::Ready { image: held, moves: Moves::Still, .. } if held.id == image.id))
    }

    /// The picture at `path`, `edge` pixels on its longer side, playing as messages do. The
    /// first ask queues the decode for a background thread; `landed` is told when it is done,
    /// and must hand the result to [`Images::store`] and call [`Images::pump`].
    pub fn get<T: 'static>(
        &mut self,
        path: &str,
        edge: u32,
        crop: Option<f32>,
        cx: &mut Context<T>,
        landed: fn(&mut T, Loaded, &mut Context<T>),
    ) -> Picture {
        self.get_to(path, edge, crop, Play::Inline, cx, landed)
    }

    /// [`Images::get`] for a place that plays differently: the viewer plays what it opens.
    pub fn get_to<T: 'static>(
        &mut self,
        path: &str,
        edge: u32,
        crop: Option<f32>,
        play: Play,
        cx: &mut Context<T>,
        landed: fn(&mut T, Loaded, &mut Context<T>),
    ) -> Picture {
        let slot = Slot::new(path, edge, crop);
        let frame = self.frame;
        let wants = anim::wants_frames(play);
        match self.held.get_mut(&slot) {
            Some(Held::Ready { image, used, moves, upgrading, retry, .. }) => {
                *used = frame;
                let image = image.clone();
                // Back on screen, or room again: its frames are read once more, and its
                // first frame stands in meanwhile.
                let due = retry.is_none_or(|at| at <= Instant::now());
                if *moves == Moves::Still && wants && due && anim::MESSAGES.room(image.as_bytes(0).map_or(0, <[u8]>::len) * 2) {
                    if upgrading.is_none() {
                        self.queue.push_back((slot, true));
                    }
                    *upgrading = Some(frame);
                    self.pump(cx, landed);
                }
                return Picture::Ready(image);
            }
            Some(Held::Loading { asked }) => {
                *asked = frame;
                // It may be waiting behind pictures nobody wants any more, with nothing being
                // decoded to move the queue on.
                self.pump(cx, landed);
                return Picture::Loading;
            }
            Some(Held::Failed) => return Picture::Failed,
            None => {}
        }
        self.held.insert(slot.clone(), Held::Loading { asked: frame });
        self.queue.push_back((slot, wants));
        self.pump(cx, landed);
        Picture::Loading
    }

    /// The largest whole picture already decoded from `path`, at whatever size: what stands in
    /// while the size wanted now is being read.
    pub fn largest(&mut self, path: &str) -> Option<Arc<RenderImage>> {
        let frame = self.frame;
        let held = self.held.iter_mut().filter(|(slot, _)| &*slot.path == path && slot.crop == 0);
        let (_, held) = held.filter(|(_, held)| matches!(held, Held::Ready { .. })).max_by_key(|(slot, _)| slot.edge)?;
        match held {
            Held::Ready { image, used, .. } => {
                *used = frame;
                Some(image.clone())
            }
            _ => None,
        }
    }

    /// Starts decodes while there is room for them. A picture nobody has drawn since it was
    /// queued is skipped: its row scrolled away, and asking again queues it again.
    pub fn pump<T: 'static>(&mut self, cx: &mut Context<T>, landed: fn(&mut T, Loaded, &mut Context<T>)) {
        while self.decoding < DECODES {
            let Some((slot, wants)) = self.queue.pop_front() else {
                return;
            };
            match self.held.get_mut(&slot) {
                Some(Held::Loading { asked } | Held::Ready { upgrading: Some(asked), .. }) if *asked + 1 >= self.frame => {}
                Some(Held::Loading { .. }) => {
                    self.held.remove(&slot);
                    continue;
                }
                Some(Held::Ready { upgrading, .. }) => {
                    *upgrading = None;
                    continue;
                }
                _ => continue,
            }
            self.decoding += 1;
            let limits = self.limits;
            cx.spawn(async move |this, cx| {
                let (file, edge) = (slot.path.clone(), slot.edge);
                let crop = (slot.crop > 0).then(|| slot.crop as f32 / 1000.);
                let frames = wants.then_some(limits);
                let image = cx.background_executor().spawn(async move { load(Path::new(&*file), edge, crop, frames) }).await;
                let _ = this.update(cx, |this, cx| landed(this, Loaded { slot, image }, cx));
            })
            .detach();
        }
    }

    /// Stores what a decode produced. False when nobody is waiting for it any more.
    pub fn store(&mut self, loaded: Loaded) -> bool {
        let Loaded { slot, image } = loaded;
        self.decoding = self.decoding.saturating_sub(1);
        let upgrade = match self.held.get(&slot) {
            Some(Held::Loading { .. }) => false,
            Some(Held::Ready { upgrading: Some(_), .. }) => true,
            _ => {
                // Nobody waits for it: its frames go back.
                if let Some((image, Moves::Playing)) = &image {
                    anim::MESSAGES.give(anim::bytes_of(image));
                    anim::forget(image);
                }
                return false;
            }
        };
        if upgrade {
            match image {
                Some((image, Moves::Playing)) => {
                    // The first frame that stood in goes; its texture with it at the next sweep.
                    if let Some(Held::Ready { image: old, bytes, .. }) = self.held.remove(&slot) {
                        self.bytes -= bytes;
                        self.dropped.push(old);
                    }
                    self.held.insert(slot, Held::Ready { image, bytes: 0, used: self.frame, moves: Moves::Playing, upgrading: None, retry: None });
                }
                _ => {
                    if let Some(Held::Ready { upgrading, retry, .. }) = self.held.get_mut(&slot) {
                        *upgrading = None;
                        *retry = Some(Instant::now() + RETRY);
                    }
                }
            }
            return true;
        }
        let held = match image {
            Some((image, moves)) => {
                let bytes = if moves == Moves::Playing { 0 } else { image.as_bytes(0).map_or(0, <[u8]>::len) };
                self.bytes += bytes;
                self.peak = self.peak.max(self.bytes);
                // A moving picture that found no room tries again a little later.
                let retry = (moves == Moves::Still).then(|| Instant::now() + RETRY);
                Held::Ready { image, bytes, used: self.frame, moves, upgrading: None, retry }
            }
            None => Held::Failed,
        };
        self.held.insert(slot, held);
        true
    }

    /// After a draw: lets go of the frames of every moving picture that is no longer on
    /// screen, keeping its first frame, then drops stills that are no longer on screen, oldest
    /// first, until the total is back under the budget. Their textures go with them.
    pub fn sweep(&mut self, window: &mut Window, cx: &mut App) {
        for image in self.dropped.drain(..) {
            cx.drop_image(image, Some(window));
        }
        let gone: Vec<Slot> = self
            .held
            .iter()
            .filter_map(|(slot, held)| match held {
                Held::Ready { used, moves: Moves::Playing, .. } if *used < self.frame => Some(slot.clone()),
                _ => None,
            })
            .collect();
        for slot in gone {
            let Some(Held::Ready { image, used, .. }) = self.held.remove(&slot) else { continue };
            let still = anim::first_frame(&image);
            self.drop_frames(image, window, cx);
            if let Some(still) = still {
                let bytes = still.as_bytes(0).map_or(0, <[u8]>::len);
                self.bytes += bytes;
                self.held.insert(slot, Held::Ready { image: still, bytes, used, moves: Moves::Still, upgrading: None, retry: None });
            }
        }
        if self.bytes <= BUDGET {
            return;
        }
        let mut idle: Vec<(u64, Slot)> = self
            .held
            .iter()
            .filter_map(|(slot, held)| match held {
                Held::Ready { used, .. } if *used < self.frame => Some((*used, slot.clone())),
                _ => None,
            })
            .collect();
        idle.sort_unstable_by_key(|(used, _)| *used);
        for (_, slot) in idle {
            if self.bytes <= BUDGET {
                break;
            }
            self.evicted += 1;
            self.drop_slot(&slot, window, cx);
        }
    }

    /// An animation's frames leave the window and the budget.
    fn drop_frames(&mut self, image: Arc<RenderImage>, window: &mut Window, cx: &mut App) {
        anim::MESSAGES.give(anim::bytes_of(&image));
        anim::forget(&image);
        cx.drop_image(image, Some(window));
    }

    fn drop_slot(&mut self, slot: &Slot, window: &mut Window, cx: &mut App) {
        if let Some(Held::Ready { image, bytes, moves, .. }) = self.held.remove(slot) {
            self.bytes -= bytes;
            if moves == Moves::Playing {
                self.drop_frames(image, window, cx);
            } else {
                cx.drop_image(image, Some(window));
            }
        }
    }

    /// Drops one whole picture decoded from `path` at `edge`, as asked of [`Images::get`]: the
    /// viewer's original, once nothing shows it.
    pub fn let_go(&mut self, path: &str, edge: u32, window: &mut Window, cx: &mut App) {
        // One still being decoded goes too: what it brings back is then not kept.
        self.drop_slot(&Slot::new(path, edge, None), window, cx);
    }

    /// The file at `path` was replaced by a larger one: what was decoded from the old one goes,
    /// so the next draw reads the new.
    pub fn replaced(&mut self, path: &str, window: &mut Window, cx: &mut App) {
        let slots: Vec<Slot> = self.held.keys().filter(|slot| &*slot.path == path).cloned().collect();
        for slot in slots {
            self.drop_slot(&slot, window, cx);
            self.held.remove(&slot);
        }
    }

    /// Lets go of everything: the conversation changed, or the viewer needs the room.
    pub fn clear(&mut self, window: &mut Window, cx: &mut App) {
        let slots: Vec<Slot> = self.held.keys().cloned().collect();
        for slot in slots {
            self.drop_slot(&slot, window, cx);
        }
        for image in self.dropped.drain(..) {
            cx.drop_image(image, Some(window));
        }
        self.held.clear();
        self.queue.clear();
    }

    /// A file that failed to decode may have been half-written; a new path is a new try.
    pub fn forget_failed(&mut self) {
        self.held.retain(|_, held| !matches!(held, Held::Failed));
    }
}

impl Drop for Images {
    /// A pane or picker that goes without clearing still gives its frames back.
    fn drop(&mut self) {
        for held in self.held.values() {
            if let Held::Ready { image, moves: Moves::Playing, .. } = held {
                anim::MESSAGES.give(anim::bytes_of(image));
                anim::forget(image);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
    }

    #[test]
    fn cached_images_decode_without_filename_extensions() {
        for (ix, format) in [image::ImageFormat::Png, image::ImageFormat::Jpeg, image::ImageFormat::WebP].into_iter().enumerate() {
            let path = std::env::temp_dir().join(format!("tc-extensionless-{}-{ix}", std::process::id()));
            let image = image::RgbImage::from_pixel(32, 16, image::Rgb([220, 40, 60]));
            image.save_with_format(&path, format).unwrap();
            let decoded = decode(&path, 16, None).expect("cache files have no extension; use their bytes");
            assert_eq!((decoded.width, decoded.height), (16, 8));
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn a_moving_picture_is_read_for_frames_only_when_they_are_wanted() {
        // Only these are read whole: a GIF, a WebP marked animated, a PNG with an animation chunk.
        assert!(might_move(&fixture("forever.gif")) && might_move(&fixture("twice.webp")) && might_move(&fixture("apng.png")));
        assert!(!might_move(&fixture("still.webp")));
        let (playing, moves) = load(&fixture("forever.gif"), 16, None, Some(frames::SMALL)).unwrap();
        assert_eq!((playing.frame_count(), moves, playing.size(0).width.0), (4, Moves::Playing, 16));
        anim::MESSAGES.give(anim::bytes_of(&playing));
        // Not wanted (the setting is Never): its first frame, known to move.
        let (still, moves) = load(&fixture("apng.png"), 16, None, None).unwrap();
        assert_eq!((still.frame_count(), moves), (1, Moves::Still));
        let (plain, moves) = load(&fixture("still.webp"), 16, None, Some(frames::SMALL)).unwrap();
        assert_eq!((plain.frame_count(), moves), (1, Moves::No));
    }

    #[test]
    fn a_missing_or_unreadable_file_is_no_picture() {
        // Neither reaches the system's reader, which is what trapped: a path that is not
        // there, and a directory.
        assert!(!looks_like_a_picture(Path::new("/nonexistent/picture.png")));
        assert!(decode(Path::new("/nonexistent/picture.png"), 256, None).is_none());
        assert!(decode(&std::env::temp_dir(), 256, None).is_none());
    }

    #[test]
    fn only_a_file_that_starts_like_a_picture_is_handed_to_the_system() {
        let dir = std::env::temp_dir();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.join(format!("tc-fc-{name}-{}", std::process::id()));
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let empty = write("empty", b"");
        let short = write("short", b"\x89P");
        let web = write("web", b"RIFF\x10\0\0\0WEBPVP8 ");
        let wave = write("wave", b"RIFF\x10\0\0\0WAVEfmt ");
        assert!(!looks_like_a_picture(&empty) && !looks_like_a_picture(&short) && !looks_like_a_picture(&wave));
        assert!(looks_like_a_picture(&web));
        // Truncated to its signature: a picture by its first bytes, and still no trap.
        let stub = write("stub", b"\x89PNG\r\n\x1a\n");
        assert!(looks_like_a_picture(&stub));
        assert!(decode(&stub, 64, None).is_none());
        for path in [empty, short, web, wave, stub] {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn a_file_that_is_not_a_picture_is_no_picture() {
        let not_a_picture = std::env::temp_dir().join(format!("tc-s5-not-a-picture-{}", std::process::id()));
        std::fs::write(&not_a_picture, b"plain text").unwrap();
        assert!(decode(&not_a_picture, 256, None).is_none());
        std::fs::remove_file(not_a_picture).unwrap();
    }

    #[test]
    fn a_picture_is_decoded_at_the_size_asked_for_not_its_own() {
        // A 64 x 32 BMP, bottom-up BGR: the left half red, the right half blue.
        let (width, height) = (64u32, 32u32);
        let mut bmp = Vec::new();
        let pixels = width * height * 3;
        bmp.extend(b"BM");
        bmp.extend((54 + pixels).to_le_bytes());
        bmp.extend([0; 4]);
        bmp.extend(54u32.to_le_bytes());
        bmp.extend(40u32.to_le_bytes());
        bmp.extend(width.to_le_bytes());
        bmp.extend(height.to_le_bytes());
        bmp.extend(1u16.to_le_bytes());
        bmp.extend(24u16.to_le_bytes());
        bmp.extend([0; 24]);
        for _ in 0..height {
            for x in 0..width {
                bmp.extend(if x < width / 2 { [0, 0, 255] } else { [255, 0, 0] });
            }
        }
        let path = std::env::temp_dir().join(format!("tc-s5-halves-{}.bmp", std::process::id()));
        std::fs::write(&path, bmp).unwrap();

        let small = decode(&path, 16, None).unwrap();
        assert_eq!((small.width, small.height, small.bgra.len()), (16, 8, 16 * 8 * 4));
        // Bytes are blue, green, red, alpha: red on the left, blue on the right.
        let pixel = |x: usize| &small.bgra[x * 4..x * 4 + 4];
        assert!(pixel(1)[2] > 200 && pixel(1)[0] < 60, "left is {:?}", pixel(1));
        assert!(pixel(14)[0] > 200 && pixel(14)[2] < 60, "right is {:?}", pixel(14));
        assert_eq!(pixel(1)[3], 255);

        // Asking for more than the file has does not enlarge it.
        let whole = decode(&path, 4096, None).unwrap();
        assert_eq!((whole.width, whole.height), (64, 32));
        // A crop keeps the top rows: a quarter as tall as it is wide is 4 rows of 16.
        let top = decode(&path, 16, Some(0.25)).unwrap();
        assert_eq!((top.width, top.height, top.bgra.len()), (16, 4, 16 * 4 * 4));
        std::fs::remove_file(path).unwrap();
    }
}
