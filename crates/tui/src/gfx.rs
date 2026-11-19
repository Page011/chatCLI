//! Sharper thumbnails with the terminal's own graphics (issue 37): Sixel, Kitty or iTerm2.
//!
//! The half-block thumbnail is always in the conversation lines, so every terminal gets one and
//! it clips with the lines when it scrolls. Where the terminal can draw pixels, an image on
//! screen is drawn over those half-blocks, cut row by row where the top or bottom edge cuts it.
//! Anything unsure falls back to half-blocks: a wrong guess must never print garbage.
//!
//! `choose` is the decision (pure, tested). `detect` asks the terminal once, before the event
//! loop, with a short timeout. `Graphics` keeps the encoded images and draws them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chatcli_core::log;
use chatcli_core::settings::{ImageProtocol, MediaSettings};
use ratatui::layout::{Rect, Size};
use ratatui::Frame;
use ratatui_image::picker::cap_parser::QueryStdioOptions;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::sliced::{SlicedImage, SlicedProtocol};
use ratatui_image::{FilterType, Resize};

use crate::media_view::{decode_checked, one_at_a_time};
use crate::ui::VisibleImage;

/// How long the terminal gets to answer the capability query. Terminals that ignore it are
/// waited for this long once, at start.
const QUERY_TIMEOUT: Duration = Duration::from_millis(200);

/// Encoded images kept; more than this on screen at once is not a real conversation.
const KEEP_MAX: usize = 32;

/// How thumbnails are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Render {
    /// Only the marker: no thumbnail at all.
    Off,
    HalfBlocks,
    Graphics(ProtocolType),
}

/// The setting, what the terminal answered, and whether a multiplexer is in the way. A protocol
/// named in `settings.toml` is used as named (the user knows their terminal); `auto` uses what the
/// terminal reported, and never inside tmux or screen, where graphics need passthrough that is
/// untested.
pub(crate) fn choose(
    setting: ImageProtocol,
    inline: bool,
    detected: Option<ProtocolType>,
    multiplexer: bool,
) -> Render {
    if !inline {
        return Render::Off;
    }
    match setting {
        ImageProtocol::Off => Render::Off,
        ImageProtocol::Halfblocks => Render::HalfBlocks,
        ImageProtocol::Sixel => Render::Graphics(ProtocolType::Sixel),
        ImageProtocol::Kitty => Render::Graphics(ProtocolType::Kitty),
        ImageProtocol::Iterm2 => Render::Graphics(ProtocolType::Iterm2),
        ImageProtocol::Auto => match detected {
            Some(ProtocolType::Halfblocks) | None => Render::HalfBlocks,
            Some(_) if multiplexer => Render::HalfBlocks,
            Some(p) => Render::Graphics(p),
        },
    }
}

/// True inside tmux or GNU screen.
fn in_multiplexer() -> bool {
    let has = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    has("TMUX")
        || has("STY")
        || std::env::var("TERM").is_ok_and(|t| t.starts_with("screen") || t.starts_with("tmux"))
}

/// Asks the terminal what it can draw. Call after the alternate screen is on and before the
/// event loop reads keys. `None` means half-blocks (or nothing): no graphics.
pub(crate) fn detect(media: &MediaSettings) -> Option<Picker> {
    let wants_query = media.inline_images
        && !matches!(
            media.image_protocol,
            ImageProtocol::Off | ImageProtocol::Halfblocks
        );
    if !wants_query {
        return None;
    }
    let options = QueryStdioOptions {
        timeout: QUERY_TIMEOUT,
        ..QueryStdioOptions::default()
    };
    let (picker, detected) = match Picker::from_query_stdio_with_options(options) {
        Ok(p) => {
            let t = p.protocol_type();
            (Some(p), Some(t))
        }
        Err(e) => {
            log::error("tui", &format!("terminal graphics query failed: {e}"));
            (None, None)
        }
    };
    match choose(
        media.image_protocol,
        media.inline_images,
        detected,
        in_multiplexer(),
    ) {
        Render::Graphics(t) => {
            // A forced protocol with no answer from the terminal still needs a cell size:
            // `halfblocks()` carries the usual 10x20.
            let mut p = picker.unwrap_or_else(Picker::halfblocks);
            p.set_protocol_type(t);
            Some(p)
        }
        Render::Off | Render::HalfBlocks => None,
    }
}

/// One encoded image: the file and the cell box it was made for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GfxKey {
    pub path: PathBuf,
    pub cols: u16,
    pub rows: u16,
}

/// Decodes `path` and encodes it for the picker's protocol, scaled to fill `cols` x `rows`
/// cells, in rows that can be drawn on their own. Slow (a Sixel encode of a photo can take a
/// while): run it on a worker thread.
pub(crate) fn encode(
    picker: &Picker,
    path: &Path,
    cols: u16,
    rows: u16,
    max_bytes: u64,
) -> Result<SlicedProtocol, String> {
    one_at_a_time(|| {
        let img = decode_checked(path, max_bytes)?;
        SlicedProtocol::new_with_resize(
            picker,
            img,
            Size::new(cols, rows),
            Resize::Scale(Some(FilterType::Triangle)),
        )
        .map_err(|e| format!("PREVIEW FAILED: {e}"))
    })
}

/// The encoded images and what was drawn last frame.
pub(crate) struct Graphics {
    picker: Picker,
    ready: HashMap<GfxKey, SlicedProtocol>,
    pending: HashSet<GfxKey>,
    failed: HashSet<GfxKey>,
    /// What the last frame overlaid: the image, its box on screen and the rows cut off its top.
    /// Where an image was and is not drawn again, its cells are rewritten, so a Sixel image that
    /// moved or went away leaves no pixels behind.
    last: Vec<(GfxKey, Rect, u16)>,
}

impl Graphics {
    pub fn new(picker: Picker) -> Graphics {
        Graphics {
            picker,
            ready: HashMap::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            last: Vec::new(),
        }
    }

    fn key_of(v: &VisibleImage) -> GfxKey {
        GfxKey {
            path: v.path.clone(),
            cols: v.rect.width,
            rows: v.rows,
        }
    }

    /// Images on screen that still need encoding: the key to file the result under, and a
    /// picker to encode with. Each is asked for once.
    pub fn wanted(&mut self, vis: &[VisibleImage]) -> Vec<(GfxKey, Picker)> {
        if self.ready.len() > KEEP_MAX {
            // Drop only what is off screen, or a busy screen would re-encode for ever.
            let on: std::collections::HashSet<GfxKey> = vis.iter().map(Self::key_of).collect();
            self.ready.retain(|k, _| on.contains(k));
        }
        let mut jobs = Vec::new();
        for v in vis {
            let key = Self::key_of(v);
            if self.ready.contains_key(&key)
                || self.pending.contains(&key)
                || self.failed.contains(&key)
            {
                continue;
            }
            self.pending.insert(key.clone());
            jobs.push((key, self.picker.clone()));
        }
        jobs
    }

    /// A worker finished: keep the encoding, or remember that this one cannot be done (it stays
    /// half-blocks, and is not retried every frame).
    pub fn store(&mut self, key: GfxKey, result: Result<SlicedProtocol, String>) {
        self.pending.remove(&key);
        match result {
            Ok(p) => {
                self.ready.insert(key, p);
            }
            Err(e) => {
                log::error("tui", &format!("graphics image not drawn: {e}"));
                self.failed.insert(key);
            }
        }
    }

    /// What `render` will overlay for `vis`.
    fn overlay(&self, vis: &[VisibleImage]) -> Vec<(GfxKey, Rect, u16)> {
        vis.iter()
            .map(|v| (Self::key_of(v), v.rect, v.skip))
            .filter(|(k, _, _)| self.ready.contains_key(k))
            .collect()
    }

    /// Boxes where the last frame drew an image that this frame does not draw again in the same
    /// place: the caller rewrites those cells. Only a removed or moved image can leave a ghost;
    /// a new one draws over text.
    pub fn stale(&mut self, vis: &[VisibleImage]) -> Vec<Rect> {
        let now = self.overlay(vis);
        let out = self
            .last
            .iter()
            .filter(|e| !now.contains(e))
            .map(|(_, r, _)| *r)
            .collect();
        self.last = now;
        out
    }

    /// Draws each ready image over its half-block thumbnail: only its rows inside `rect`.
    pub fn render(&self, f: &mut Frame, vis: &[VisibleImage]) {
        for v in vis {
            if let Some(p) = self.ready.get(&Self::key_of(v)) {
                let above = -i16::try_from(v.skip).unwrap_or(i16::MAX);
                f.render_widget(SlicedImage::new(p, (0, above).into()), v.rect);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use ProtocolType::{Halfblocks, Iterm2, Kitty, Sixel};

    #[test]
    fn the_choice_table() {
        use ImageProtocol as S;
        // (setting, inline, detected, multiplexer) -> render
        let cases: &[(S, bool, Option<ProtocolType>, bool, Render)] = &[
            // Auto follows the terminal.
            (S::Auto, true, Some(Sixel), false, Render::Graphics(Sixel)),
            (S::Auto, true, Some(Kitty), false, Render::Graphics(Kitty)),
            (S::Auto, true, Some(Iterm2), false, Render::Graphics(Iterm2)),
            (S::Auto, true, Some(Halfblocks), false, Render::HalfBlocks),
            (S::Auto, true, None, false, Render::HalfBlocks),
            // Inside tmux or screen auto never guesses graphics.
            (S::Auto, true, Some(Sixel), true, Render::HalfBlocks),
            // Naming a protocol uses it, with or without an answer, even in a multiplexer.
            (S::Sixel, true, None, false, Render::Graphics(Sixel)),
            (S::Kitty, true, Some(Sixel), false, Render::Graphics(Kitty)),
            (S::Iterm2, true, None, true, Render::Graphics(Iterm2)),
            (S::Halfblocks, true, Some(Sixel), false, Render::HalfBlocks),
            // Off and inline_images = false show only the marker.
            (S::Off, true, Some(Sixel), false, Render::Off),
            (S::Auto, false, Some(Sixel), false, Render::Off),
            (S::Sixel, false, None, false, Render::Off),
        ];
        for (setting, inline, detected, mux, want) in cases {
            assert_eq!(
                choose(*setting, *inline, *detected, *mux),
                *want,
                "{setting:?} inline={inline} detected={detected:?} mux={mux}"
            );
        }
    }

    #[test]
    fn nothing_is_asked_of_the_terminal_when_only_half_blocks_can_result() {
        for (protocol, inline) in [
            (ImageProtocol::Off, true),
            (ImageProtocol::Halfblocks, true),
            (ImageProtocol::Auto, false),
        ] {
            let media = MediaSettings {
                image_protocol: protocol,
                inline_images: inline,
                ..MediaSettings::default()
            };
            assert!(detect(&media).is_none(), "{protocol:?} inline={inline}");
        }
    }

    fn visible(path: &str, x: u16, y: u16) -> VisibleImage {
        VisibleImage {
            path: PathBuf::from(path),
            rect: Rect::new(x, y, 6, 3),
            rows: 3,
            skip: 0,
        }
    }

    fn png(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        image::RgbaImage::from_pixel(60, 30, image::Rgba([200, 30, 30, 255]))
            .save_with_format(&p, image::ImageFormat::Png)
            .unwrap();
        p
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chatcli-gfx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn each_image_is_encoded_once_and_a_failure_is_not_retried() {
        let mut g = Graphics::new(Picker::halfblocks());
        let a = visible("a.png", 1, 1);
        let b = visible("b.png", 1, 5);
        let jobs = g.wanted(&[a, b]);
        assert_eq!(jobs.len(), 2);
        // Asked again while the workers run: nothing new.
        assert!(g
            .wanted(&[visible("a.png", 1, 1), visible("b.png", 1, 5)])
            .is_empty());
        let (key_a, _) = jobs[0].clone();
        g.store(key_a, Err("PREVIEW FAILED: x".into()));
        assert!(
            g.wanted(&[visible("a.png", 1, 1)]).is_empty(),
            "a failure stays half-blocks"
        );
        // The same file in another box size is a new encoding.
        let mut small = visible("a.png", 1, 1);
        small.rect.width = 3;
        assert_eq!(g.wanted(&[small]).len(), 1);
    }

    #[test]
    fn the_overlay_is_drawn_only_when_ready_and_a_moved_image_leaves_its_box_stale() {
        let dir = temp("overlay");
        let path = png(&dir, "p.png");
        let mut g = Graphics::new(Picker::halfblocks());
        let v = VisibleImage {
            path: path.clone(),
            rect: Rect::new(2, 1, 6, 3),
            rows: 3,
            skip: 0,
        };
        assert!(
            g.stale(std::slice::from_ref(&v)).is_empty(),
            "nothing ready, nothing drawn: nothing stale"
        );
        let jobs = g.wanted(std::slice::from_ref(&v));
        let (key, picker) = jobs[0].clone();
        let proto = encode(&picker, &path, key.cols, key.rows, 1 << 20).unwrap();
        assert!(proto.size().width <= 6 && proto.size().height <= 3);
        g.store(key, Ok(proto));
        assert!(
            g.stale(std::slice::from_ref(&v)).is_empty(),
            "an image appeared: it draws over text, nothing to rewrite"
        );
        assert!(
            g.stale(std::slice::from_ref(&v)).is_empty(),
            "the same frame again: nothing to rewrite"
        );
        let moved = VisibleImage {
            path: path.clone(),
            rect: Rect::new(2, 2, 6, 3),
            rows: 3,
            skip: 0,
        };
        assert_eq!(
            g.stale(std::slice::from_ref(&moved)),
            vec![v.rect],
            "it scrolled one row: its old box is rewritten"
        );
        assert_eq!(
            g.stale(&[]),
            vec![moved.rect],
            "it scrolled away: its box is rewritten, or Sixel leaves ghosts"
        );

        let mut term = Terminal::new(TestBackend::new(20, 8)).unwrap();
        term.draw(|f| g.render(f, std::slice::from_ref(&v)))
            .unwrap();
        let buf = term.backend().buffer();
        // Half-blocks of one flat colour are drawn as coloured cells: look at the colours.
        let painted = |x, y| {
            let c = &buf[(x, y)];
            c.fg != ratatui::style::Color::Reset || c.bg != ratatui::style::Color::Reset
        };
        assert!(painted(2, 1), "the image is in its box");
        // A 2:1 picture in a 6x3 box (10x20 pixel cells) fills the width and two of the rows.
        assert!(painted(7, 2), "to its far corner");
        assert!(
            !painted(12, 6) && !painted(1, 1) && !painted(8, 1),
            "and nowhere else"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_image_cut_by_the_top_edge_stays_sharp_and_draws_only_its_rows_on_screen() {
        let dir = temp("cut");
        let path = png(&dir, "p.png");
        let mut g = Graphics::new(Picker::halfblocks());
        let whole = visible(path.to_str().unwrap(), 2, 1);
        let (key, picker) = g.wanted(std::slice::from_ref(&whole))[0].clone();
        let proto = encode(&picker, &path, key.cols, key.rows, 1 << 20).unwrap();
        g.store(key, Ok(proto));
        // Scrolled up two rows: its first row is above the screen, two are on it.
        let cut = VisibleImage {
            path: path.clone(),
            rect: Rect::new(2, 0, 6, 2),
            rows: 3,
            skip: 1,
        };
        assert!(
            g.wanted(std::slice::from_ref(&cut)).is_empty(),
            "the same encoding, not a new one per scroll step"
        );
        g.stale(std::slice::from_ref(&whole));
        assert_eq!(
            g.stale(std::slice::from_ref(&cut)),
            vec![whole.rect],
            "the box it left is rewritten"
        );
        assert!(g.stale(std::slice::from_ref(&cut)).is_empty());

        let mut term = Terminal::new(TestBackend::new(20, 8)).unwrap();
        term.draw(|f| g.render(f, std::slice::from_ref(&cut)))
            .unwrap();
        let buf = term.backend().buffer();
        let painted = |x, y| {
            let c = &buf[(x, y)];
            c.fg != ratatui::style::Color::Reset || c.bg != ratatui::style::Color::Reset
        };
        // The picture fills two of its three rows: with one cut off, its second row is the
        // only one left, on the screen's top row.
        assert!(painted(2, 0) && painted(7, 0), "its second row, across");
        assert!(
            !painted(2, 1) && !painted(2, 2) && !painted(8, 0),
            "nothing below it or beside it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_big_or_broken_file_is_refused_with_the_standard_reason() {
        let dir = temp("refuse");
        let junk = dir.join("j.png");
        std::fs::write(&junk, b"not a picture").unwrap();
        let p = Picker::halfblocks();
        assert!(encode(&p, &junk, 4, 2, 1 << 20)
            .err()
            .expect("refused")
            .starts_with("PREVIEW FAILED"));
        let ok = png(&dir, "ok.png");
        assert_eq!(
            encode(&p, &ok, 4, 2, 10).err().expect("refused"),
            crate::media_view::TOO_LARGE
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
