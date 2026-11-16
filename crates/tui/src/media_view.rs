//! Media in the conversation (issues 16 to 19): what an attachment chip says, the state of one
//! download or preview, and the half-block thumbnail drawn under an image.
//!
//! Pure and tested without a terminal. The only file work is `make_thumb`, which the run loop
//! calls on a worker thread: App itself never reads a picture.
//!
//! Colour: a thumbnail needs RGB, the one exception to the ANSI-16-only rule (recorded in
//! docs/DESIGN.md). A terminal without 24-bit colour gets the nearest of the 256 palette colours.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chatcli_core::media::MAX_PIXELS;
use chatcli_core::model::MediaKind;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// Longest edge of a stored thumbnail in pixels. A cell holds two pixels one above the other, so
/// this is at most 40 columns by 20 rows, and a thumbnail is a few kilobytes.
pub const THUMB_PX: u32 = 40;

/// The one sentence for a picture that is too big to preview.
pub const TOO_LARGE: &str = "IMAGE TOO LARGE, press o to open";

const NOT_A_PICTURE: &str = "PREVIEW FAILED: not a picture the viewer can read, press o to open";

// ---- colour ----

/// How many colours the terminal can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// 24-bit colour.
    True,
    /// The 256-colour palette.
    Indexed,
}

impl Depth {
    /// From the environment: `COLORTERM`, `TERM`, and the platform. Windows Terminal does not set
    /// `COLORTERM` but draws 24-bit colour, as does the classic console since Windows 10 1703.
    pub fn detect() -> Depth {
        depth_from_env(
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
            cfg!(windows),
        )
    }
}

pub fn depth_from_env(colorterm: Option<&str>, term: Option<&str>, windows: bool) -> Depth {
    let has = |v: Option<&str>, needles: &[&str]| {
        v.is_some_and(|v| {
            let v = v.to_ascii_lowercase();
            needles.iter().any(|n| v.contains(n))
        })
    };
    if windows || has(colorterm, &["truecolor", "24bit"]) || has(term, &["direct", "truecolor"]) {
        Depth::True
    } else {
        Depth::Indexed
    }
}

/// The 256-palette index nearest to an RGB colour: the 6x6x6 cube (16..=231) or the grey ramp
/// (232..=255), whichever is closer.
pub fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| -> usize {
        (0..6)
            .min_by_key(|&i| (LEVELS[i] - i32::from(v)).abs())
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (near(r), near(g), near(b));
    let dist = |a: [i32; 3]| -> i32 {
        let d = [
            a[0] - i32::from(r),
            a[1] - i32::from(g),
            a[2] - i32::from(b),
        ];
        d.iter().map(|x| x * x).sum()
    };
    let cube = dist([LEVELS[ri], LEVELS[gi], LEVELS[bi]]);
    let avg = (i32::from(r) + i32::from(g) + i32::from(b)) / 3;
    let grey_i = ((avg - 8 + 5) / 10).clamp(0, 23);
    let grey_v = 8 + 10 * grey_i;
    let grey = dist([grey_v, grey_v, grey_v]);
    if grey < cube {
        (232 + grey_i) as u8
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u8
    }
}

fn map_color(rgb: [u8; 3], depth: Depth) -> Color {
    match depth {
        Depth::True => Color::Rgb(rgb[0], rgb[1], rgb[2]),
        Depth::Indexed => Color::Indexed(nearest_256(rgb[0], rgb[1], rgb[2])),
    }
}

// ---- thumbnails ----

/// A picture reduced to at most `THUMB_PX` pixels on a side, as colours ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct Thumb {
    pub w: u16,
    pub h: u16,
    px: Vec<Color>,
}

impl Thumb {
    /// From RGB pixels, row by row (`rgb.len()` must be `w * h`).
    pub fn from_rgb(w: u16, h: u16, rgb: &[[u8; 3]], depth: Depth) -> Thumb {
        let want = usize::from(w) * usize::from(h);
        let mut px: Vec<Color> = rgb
            .iter()
            .take(want)
            .map(|p| map_color(*p, depth))
            .collect();
        px.resize(want, Color::Reset);
        Thumb { w, h, px }
    }

    /// Bytes this thumbnail holds in memory (the cache is the thumbnail, never the picture).
    pub fn bytes(&self) -> usize {
        self.px.len() * std::mem::size_of::<Color>() + std::mem::size_of::<Thumb>()
    }

    /// Pixel size when drawn at most `max_cols` columns wide (never scaled up).
    fn target(&self, max_cols: usize) -> (usize, usize) {
        let (w, h) = (usize::from(self.w).max(1), usize::from(self.h).max(1));
        if w <= max_cols {
            (w, h)
        } else {
            let cols = max_cols.max(1);
            (cols, ((h * cols + w / 2) / w).max(1))
        }
    }

    /// Columns and rows it takes at most `max_cols` columns wide (two pixels per row).
    pub fn cells(&self, max_cols: usize) -> (usize, usize) {
        let (tw, th) = self.target(max_cols);
        (tw, th.div_ceil(2))
    }

    /// The rows of half-block characters: the upper pixel is the foreground of an upper half
    /// block, the lower pixel its background. Equal neighbours share one span.
    pub fn lines(&self, max_cols: usize) -> Vec<Vec<Span<'static>>> {
        let (tw, th) = self.target(max_cols);
        let (w, h) = (usize::from(self.w).max(1), usize::from(self.h).max(1));
        let at = |x: usize, y: usize| -> Color {
            if y >= th {
                return Color::Reset;
            }
            let (sx, sy) = (x * w / tw, y * h / th);
            self.px.get(sy * w + sx).copied().unwrap_or(Color::Reset)
        };
        (0..th.div_ceil(2))
            .map(|row| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                let mut run: Option<((Color, Color), usize)> = None;
                let flush = |run: Option<((Color, Color), usize)>,
                             spans: &mut Vec<Span<'static>>| {
                    if let Some(((top, bottom), n)) = run {
                        spans.push(Span::styled(
                            "\u{2580}".repeat(n),
                            Style::new().fg(top).bg(bottom),
                        ));
                    }
                };
                for x in 0..tw {
                    let pair = (at(x, row * 2), at(x, row * 2 + 1));
                    run = match run {
                        Some((p, n)) if p == pair => Some((p, n + 1)),
                        other => {
                            flush(other, &mut spans);
                            Some((pair, 1))
                        }
                    };
                }
                flush(run, &mut spans);
                spans
            })
            .collect()
    }
}

/// One picture is decoded at a time: a 25-megapixel picture holds about 100 MB while it is
/// decoded, and two together (the user pressing `v` on several messages) would double that.
static DECODE_SLOT: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn one_at_a_time<T>(work: impl FnOnce() -> T) -> T {
    // A panic in an earlier decode poisons the lock; the next picture can still go.
    let _slot = DECODE_SLOT.lock().unwrap_or_else(|e| e.into_inner());
    work()
}

/// A picture file decoded for drawing or for a terminal graphics protocol. Refuses a file over
/// `max_bytes` or more than 25 megapixels with `TOO_LARGE`, checking the size in the header
/// before any pixel is decoded, so a small file that claims a huge picture costs nothing.
pub fn decode_checked(path: &Path, max_bytes: u64) -> Result<image::DynamicImage, String> {
    use image::{DynamicImage, ImageDecoder, ImageReader, Limits};

    let meta = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "FILE NOT FOUND".to_string()
        } else {
            "PREVIEW FAILED: cannot read the cached file, press o to open".to_string()
        }
    })?;
    if meta.len() > max_bytes {
        return Err(TOO_LARGE.to_string());
    }
    let mut reader = ImageReader::open(path)
        .map_err(|_| NOT_A_PICTURE.to_string())?
        .with_guessed_format()
        .map_err(|_| NOT_A_PICTURE.to_string())?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| NOT_A_PICTURE.to_string())?;
    let (w, h) = decoder.dimensions();
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(TOO_LARGE.to_string());
    }
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).map_err(|_| NOT_A_PICTURE.to_string())?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok(img)
}

/// Decodes `path` and reduces it to a thumbnail. Called on a worker thread by the run loop.
pub fn make_thumb(path: &Path, depth: Depth, max_bytes: u64) -> Result<Thumb, String> {
    let rgba = one_at_a_time(|| {
        let img = decode_checked(path, max_bytes)?;
        let img = if img.width() > THUMB_PX || img.height() > THUMB_PX {
            img.thumbnail(THUMB_PX, THUMB_PX)
        } else {
            img
        };
        Ok::<_, String>(img.to_rgba8())
    })?;
    let (w, h) = (rgba.width() as u16, rgba.height() as u16);
    // A transparent pixel has no colour of its own: show it over dark grey.
    let rgb: Vec<[u8; 3]> = rgba
        .pixels()
        .map(|p| {
            let a = u32::from(p[3]);
            let mix = |c: u8| ((a * u32::from(c) + (255 - a) * 32) / 255) as u8;
            [mix(p[0]), mix(p[1]), mix(p[2])]
        })
        .collect();
    Ok(Thumb::from_rgb(w, h, &rgb, depth))
}

// ---- chips and markers ----

/// One word for the chip: `[image photo.png 240 KB]`.
pub fn kind_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Document | MediaKind::Other => "file",
    }
}

/// `12 B`, `240 KB`, `1.5 MB`, `2.0 GB` (1 KB = 1024 bytes).
pub fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    match bytes {
        0..KB => format!("{bytes} B"),
        KB..MB => format!("{} KB", bytes.div_ceil(KB).min(1023)),
        MB..GB => format!("{:.1} MB", bytes as f64 / MB as f64),
        _ => format!("{:.1} GB", bytes as f64 / GB as f64),
    }
}

/// How far a transfer is, in whole percent; `None` while the total is not known (0).
pub fn percent(done: u64, total: u64) -> Option<u64> {
    (done.min(total) * 100).checked_div(total)
}

fn is_marker(inner: &str) -> bool {
    matches!(
        inner,
        "image"
            | "video"
            | "audio"
            | "voice message"
            | "sticker"
            | "file"
            | "attachment"
            | "document"
    ) || inner.starts_with("document: ")
}

/// The text of a message without the `[image]`, `[document: name]` ... markers the backends put
/// in it, when the message has attachments the TUI draws as chips instead. At most `count`
/// markers go (one per attachment), so a user's own `[image]` beyond that stays. Whitespace the
/// removal leaves behind is tidied; text without a marker comes back unchanged.
pub fn strip_markers(text: &str, count: usize) -> String {
    let mut left = count;
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut removed = false;
    while left > 0 {
        let Some(open) = rest.find('[') else { break };
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else { break };
        if is_marker(&after[..close]) {
            out.push_str(&rest[..open]);
            rest = &after[close + 1..];
            left -= 1;
            removed = true;
        } else {
            out.push_str(&rest[..=open]);
            rest = after;
        }
    }
    out.push_str(rest);
    if !removed {
        return text.to_string();
    }
    out.lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_matches('\n')
        .to_string()
}

// ---- per-attachment state ----

/// One attachment of one message in one chat.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MediaKey {
    pub chat: String,
    pub msg: String,
    pub idx: u32,
}

/// What to do when a download finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Then {
    Nothing,
    /// Open it in the default app (`o`).
    Open,
    /// Draw its thumbnail (`v`).
    View,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Fetch {
    #[default]
    Idle,
    Downloading(Then),
    /// In the media cache.
    Ready(PathBuf),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum View {
    #[default]
    Hidden,
    /// The file is being decoded on a worker thread.
    Decoding,
    Shown(Thumb),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Item {
    pub fetch: Fetch,
    pub view: View,
    /// `o` handed the file to the OS.
    pub opened: bool,
}

/// States of every attachment the user acted on in one network.
#[derive(Debug, Default)]
pub struct MediaState {
    pub items: HashMap<MediaKey, Item>,
}

impl MediaState {
    pub fn get(&self, chat: &str, msg: &str, idx: u32) -> Option<&Item> {
        self.items.get(&MediaKey {
            chat: chat.to_string(),
            msg: msg.to_string(),
            idx,
        })
    }

    pub fn entry(&mut self, key: MediaKey) -> &mut Item {
        self.items.entry(key).or_default()
    }

    /// The short state text on a chip, `None` when idle and nothing to say. `shown` tells the
    /// thumbnail is drawn, which needs no words.
    pub fn status_text(item: &Item) -> Option<(String, bool)> {
        match (&item.fetch, &item.view) {
            (Fetch::Downloading(_), _) => Some(("downloading...".to_string(), false)),
            (Fetch::Failed(r), _) => Some((format!("FAILED: {r}"), true)),
            (_, View::Failed(r)) => Some((r.clone(), true)),
            (_, View::Decoding) => Some(("loading preview...".to_string(), false)),
            (_, View::Shown(_)) => None,
            (Fetch::Ready(_), _) if item.opened => Some(("opened".to_string(), false)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chatcli-mv-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn png(path: &Path, w: u32, h: u32, color: [u8; 4]) {
        RgbaImage::from_pixel(w, h, Rgba(color))
            .save_with_format(path, ImageFormat::Png)
            .unwrap();
    }

    #[test]
    fn depth_follows_the_environment() {
        assert_eq!(depth_from_env(Some("truecolor"), None, false), Depth::True);
        assert_eq!(
            depth_from_env(Some("24bit"), Some("xterm"), false),
            Depth::True
        );
        assert_eq!(
            depth_from_env(None, Some("xterm-direct"), false),
            Depth::True
        );
        assert_eq!(
            depth_from_env(None, Some("xterm-256color"), false),
            Depth::Indexed
        );
        assert_eq!(depth_from_env(None, None, false), Depth::Indexed);
        // Windows Terminal sets no COLORTERM and still draws 24-bit colour.
        assert_eq!(depth_from_env(None, None, true), Depth::True);
    }

    #[test]
    fn nearest_256_picks_the_cube_or_the_grey_ramp() {
        assert_eq!(nearest_256(0, 0, 0), 16);
        assert_eq!(nearest_256(255, 255, 255), 231);
        assert_eq!(nearest_256(255, 0, 0), 196);
        assert_eq!(nearest_256(0, 255, 0), 46);
        assert_eq!(nearest_256(0, 0, 255), 21);
        // Mid grey is nearer to the ramp (value 128 at index 244) than to any cube colour.
        assert_eq!(nearest_256(128, 128, 128), 244);
        // A saturated colour stays in the cube.
        assert_eq!(nearest_256(95, 135, 175), 16 + 36 + 6 * 2 + 3);
    }

    #[test]
    fn sizes_read_like_a_file_manager() {
        for (n, want) in [
            (0, "0 B"),
            (12, "12 B"),
            (1023, "1023 B"),
            (1024, "1 KB"),
            (240 * 1024, "240 KB"),
            (1024 * 1024 - 1, "1023 KB"),
            (1024 * 1024, "1.0 MB"),
            (1_572_864, "1.5 MB"),
            (3 * 1024 * 1024 * 1024, "3.0 GB"),
        ] {
            assert_eq!(human_size(n), want, "{n}");
        }
    }

    #[test]
    fn markers_go_once_per_attachment_and_nothing_else() {
        // (text, attachments, want)
        for (text, count, want) in [
            ("[image]", 1, ""),
            ("[image] sunset", 1, "sunset"),
            ("see [image] and [image]", 2, "see and"),
            ("see [image] and [image]", 1, "see and [image]"),
            ("[document: plan.pdf] read p2", 1, "read p2"),
            ("[voice message]", 1, ""),
            ("[sticker]", 1, ""),
            ("[attachment] test reply [attachment]", 2, "test reply"),
            // Not a marker: someone's own brackets stay.
            ("[note] [image]", 1, "[note]"),
            ("no marker here", 1, "no marker here"),
            ("[image]", 0, "[image]"),
            ("line one\n[image]\nline two", 1, "line one\n\nline two"),
            ("unclosed [image", 1, "unclosed [image"),
        ] {
            assert_eq!(strip_markers(text, count), want, "{text:?}");
        }
    }

    #[test]
    fn a_thumbnail_pairs_two_pixels_per_cell_and_merges_runs() {
        // 2x2: top row red red, bottom row blue green.
        let rgb = [[255, 0, 0], [255, 0, 0], [0, 0, 255], [0, 255, 0]];
        let t = Thumb::from_rgb(2, 2, &rgb, Depth::True);
        assert_eq!(t.cells(40), (2, 1));
        let lines = t.lines(40);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 2, "two different pairs, two spans");
        assert_eq!(lines[0][0].content, "\u{2580}");
        assert_eq!(
            lines[0][0].style,
            Style::new()
                .fg(Color::Rgb(255, 0, 0))
                .bg(Color::Rgb(0, 0, 255))
        );
        // Equal neighbours become one span of two blocks.
        let flat = Thumb::from_rgb(2, 2, &[[9, 9, 9]; 4], Depth::True);
        let l = flat.lines(40);
        assert_eq!(l[0].len(), 1);
        assert_eq!(l[0][0].content, "\u{2580}\u{2580}");
    }

    #[test]
    fn an_odd_height_pads_the_last_row_and_a_narrow_pane_scales_down() {
        let t = Thumb::from_rgb(3, 3, &[[1, 2, 3]; 9], Depth::True);
        assert_eq!(t.cells(40), (3, 2));
        let lines = t.lines(40);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1][0].style.bg,
            Some(Color::Reset),
            "no pixel below the last row"
        );
        // 40x20 pixels into 10 columns: 10x5 pixels, 3 rows.
        let wide = Thumb::from_rgb(40, 20, &vec![[5, 5, 5]; 800], Depth::True);
        assert_eq!(wide.cells(40), (40, 10));
        assert_eq!(wide.cells(10), (10, 3));
        let rows = wide.lines(10);
        assert_eq!(rows.len(), 3);
        for row in &rows {
            let width: usize = row.iter().map(Span::width).sum();
            assert_eq!(width, 10);
        }
        // Never scaled up, and never zero.
        assert_eq!(t.cells(100), (3, 2));
        assert_eq!(t.cells(0).0, 1);
    }

    #[test]
    fn indexed_depth_uses_palette_colours_only() {
        let t = Thumb::from_rgb(1, 2, &[[255, 0, 0], [0, 0, 255]], Depth::Indexed);
        let s = t.lines(40)[0][0].style;
        assert_eq!(s.fg, Some(Color::Indexed(196)));
        assert_eq!(s.bg, Some(Color::Indexed(21)));
    }

    #[test]
    fn make_thumb_reduces_a_picture_and_blends_transparency() {
        let dir = temp("thumb");
        let p = dir.join("big.png");
        png(&p, 200, 100, [255, 0, 0, 255]);
        let t = make_thumb(&p, Depth::True, 1 << 20).unwrap();
        assert_eq!((t.w, t.h), (40, 20), "aspect kept, longest edge 40");
        assert!(
            t.bytes() < 40 * 20 * 16 + 64,
            "a thumbnail is kilobytes: {}",
            t.bytes()
        );
        let small = dir.join("small.png");
        png(&small, 8, 4, [0, 255, 0, 255]);
        let t = make_thumb(&small, Depth::True, 1 << 20).unwrap();
        assert_eq!((t.w, t.h), (8, 4), "never scaled up");
        // Fully transparent shows as the dark grey, not as black or as garbage.
        let clear = dir.join("clear.png");
        png(&clear, 4, 4, [255, 0, 0, 0]);
        let t = make_thumb(&clear, Depth::True, 1 << 20).unwrap();
        assert_eq!(t.lines(40)[0][0].style.fg, Some(Color::Rgb(32, 32, 32)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn make_thumb_refuses_big_and_broken_files_with_a_reason() {
        let dir = temp("refuse");
        let p = dir.join("a.png");
        png(&p, 50, 50, [1, 2, 3, 255]);
        let size = std::fs::metadata(&p).unwrap().len();
        assert_eq!(
            make_thumb(&p, Depth::True, size - 1).unwrap_err(),
            TOO_LARGE,
            "over the size cap"
        );
        let junk = dir.join("junk.png");
        std::fs::write(&junk, b"this is not a picture").unwrap();
        assert!(make_thumb(&junk, Depth::True, 1 << 20)
            .unwrap_err()
            .starts_with("PREVIEW FAILED"));
        assert_eq!(
            make_thumb(&dir.join("gone.png"), Depth::True, 1 << 20).unwrap_err(),
            "FILE NOT FOUND"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_picture_over_25_megapixels_is_refused_from_its_header() {
        let dir = temp("pixels");
        let p = dir.join("wide.png");
        // 5100 x 5100 = 26.01 MP of one grey: a small file that claims a huge picture.
        image::GrayImage::new(5100, 5100)
            .save_with_format(&p, ImageFormat::Png)
            .unwrap();
        assert!(std::fs::metadata(&p).unwrap().len() < 1 << 20);
        assert_eq!(make_thumb(&p, Depth::True, 1 << 30).unwrap_err(), TOO_LARGE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The memory claim of issue 19: the cache holds thumbnails, never pictures. Twenty of the
    /// largest size (40 x 40 pixels) are about 130 KB. (The decode of one big file is the real
    /// peak, held to one at a time by `one_at_a_time`; process memory is measured by hand.)
    #[test]
    fn twenty_largest_thumbnails_are_a_few_hundred_kilobytes() {
        let t = Thumb::from_rgb(40, 40, &vec![[1, 2, 3]; 1600], Depth::True);
        let total = 20 * t.bytes();
        assert!(total < 256 * 1024, "{total} bytes");
        eprintln!(
            "one 40x40 thumbnail: {} bytes, twenty: {total} bytes",
            t.bytes()
        );
    }

    #[test]
    fn percent_never_divides_by_zero_or_passes_100() {
        assert_eq!(percent(0, 0), None);
        assert_eq!(percent(5, 0), None);
        assert_eq!(percent(0, 10), Some(0));
        assert_eq!(percent(1_350_000, 3_000_000), Some(45));
        assert_eq!(percent(10, 10), Some(100));
        assert_eq!(percent(99, 10), Some(100), "a late over-count stays at 100");
    }

    #[test]
    fn chip_state_text() {
        let mut item = Item::default();
        assert_eq!(MediaState::status_text(&item), None);
        item.fetch = Fetch::Downloading(Then::Open);
        assert_eq!(MediaState::status_text(&item).unwrap().0, "downloading...");
        item.fetch = Fetch::Failed("MEDIA EXPIRED".into());
        assert_eq!(
            MediaState::status_text(&item),
            Some(("FAILED: MEDIA EXPIRED".to_string(), true))
        );
        item.fetch = Fetch::Ready(PathBuf::from("x"));
        item.opened = true;
        assert_eq!(MediaState::status_text(&item).unwrap().0, "opened");
        item.view = View::Shown(Thumb::from_rgb(1, 1, &[[0, 0, 0]], Depth::True));
        assert_eq!(
            MediaState::status_text(&item),
            None,
            "a thumbnail needs no words"
        );
        assert_eq!(kind_label(MediaKind::Other), "file");
    }
}
