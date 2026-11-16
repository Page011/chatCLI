//! Sharper thumbnails with the terminal's own graphics (issue 37): Sixel, Kitty or iTerm2.
//!
//! The half-block thumbnail is always in the conversation lines, so every terminal gets one and
//! it clips with the lines when it scrolls. Where the terminal can draw pixels, an image that is
//! wholly on screen is drawn over those half-blocks. Anything unsure falls back to half-blocks:
//! a wrong guess must never print garbage.
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
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Image, Resize};

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
/// cells. Slow (a Sixel encode of a photo can take a while): run it on a worker thread.
pub(crate) fn encode(
    picker: &Picker,
    path: &Path,
    cols: u16,
    rows: u16,
    max_bytes: u64,
) -> Result<Protocol, String> {
    one_at_a_time(|| {
        let img = decode_checked(path, max_bytes)?;
        picker
            .new_protocol(
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
    ready: HashMap<GfxKey, Protocol>,
    pending: HashSet<GfxKey>,
    failed: HashSet<GfxKey>,
    /// What the last frame overlaid. When it changes the screen is cleared first, so a Sixel
    /// image that went away leaves no pixels behind.
    last: Vec<(GfxKey, Rect)>,
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
            rows: v.rect.height,
        }
    }

    /// Images on screen that still need encoding: the key to file the result under, and a
    /// picker to encode with. Each is asked for once.
    pub fn wanted(&mut self, vis: &[VisibleImage]) -> Vec<(GfxKey, Picker)> {
        if self.ready.len() > KEEP_MAX {
            self.ready.clear();
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
    pub fn store(&mut self, key: GfxKey, result: Result<Protocol, String>) {
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
    fn overlay(&self, vis: &[VisibleImage]) -> Vec<(GfxKey, Rect)> {
        vis.iter()
            .map(|v| (Self::key_of(v), v.rect))
            .filter(|(k, _)| self.ready.contains_key(k))
            .collect()
    }

    /// True when the overlay differs from the last frame's: the caller clears the screen first.
    pub fn changed(&mut self, vis: &[VisibleImage]) -> bool {
        let now = self.overlay(vis);
        let changed = now != self.last;
        self.last = now;
        changed
    }

    /// Draws each ready image over its half-block thumbnail.
    pub fn render(&self, f: &mut Frame, vis: &[VisibleImage]) {
        for v in vis {
            if let Some(p) = self.ready.get(&Self::key_of(v)) {
                f.render_widget(Image::new(p), v.rect);
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
    fn the_overlay_is_drawn_only_when_ready_and_changes_clear_the_screen() {
        let dir = temp("overlay");
        let path = png(&dir, "p.png");
        let mut g = Graphics::new(Picker::halfblocks());
        let v = VisibleImage {
            path: path.clone(),
            rect: Rect::new(2, 1, 6, 3),
        };
        assert!(
            !g.changed(std::slice::from_ref(&v)),
            "nothing ready, nothing drawn: no change"
        );
        let jobs = g.wanted(std::slice::from_ref(&v));
        let (key, picker) = jobs[0].clone();
        let proto = encode(&picker, &path, key.cols, key.rows, 1 << 20).unwrap();
        assert!(proto.size().width <= 6 && proto.size().height <= 3);
        g.store(key, Ok(proto));
        assert!(
            g.changed(std::slice::from_ref(&v)),
            "an image appeared: clear first"
        );
        assert!(
            !g.changed(std::slice::from_ref(&v)),
            "the same frame again: no clear"
        );
        assert!(
            g.changed(&[]),
            "it scrolled away: clear, or Sixel leaves ghosts"
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
