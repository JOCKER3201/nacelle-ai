//! A [`Surface`] with no window behind it, so the view model can be
//! tested without a GPU.
//!
//! Everything a view asks about the THEME is answered by the real theme
//! engine: `default.theme` is compiled into the toolkit and parses with
//! no display, no device and no font, so a test here reads the same
//! tokens the window does. What Paper replaces is the two answers that
//! genuinely need a machine — the glyph atlas that measures text and the
//! draw list that receives it.
//!
//! Text is measured as half its size per character. Not a font: a ruler.
//! It is monotonic in both length and size, which is every property the
//! wrap and the ellipsis arithmetic actually depend on, and it makes a
//! test's expected line breaks arithmetic rather than a golden file that
//! changes with whatever typeface the machine happens to have installed.

use nacelle::theme::{self, parse::State, Color, TokenId};
use nacelle::ui::Align;
use nacelle::view::surface::{StateInk, Surface};
use nacelle::Rect;

/// The width of one character, as a fraction of the type size.
const CHAR_W: f32 = 0.5;

pub struct Paper {
    /// How many times text has been measured. A cache that is working
    /// does not measure twice, and this is how a test sees that.
    measurements: usize,
}

impl Paper {
    pub fn new() -> Paper {
        // The theme engine is built by the first LOAD, and `theme::id`
        // answers "no such token" until it has been — silently, because
        // a missing id is a legitimate answer for a theme that declares
        // no such key. A surface that reads tokens therefore makes sure
        // there is a theme to read from before it is asked anything.
        // (The window does the same, at startup, and for the same
        // reason: the alternative is a first frame drawn from kind
        // fallbacks and an epoch that moves under the caches.)
        let _ = theme::resolved();
        Paper { measurements: 0 }
    }

    pub fn measurements(&self) -> usize {
        self.measurements
    }
}

impl Default for Paper {
    fn default() -> Self {
        Paper::new()
    }
}

fn id(name: &str) -> TokenId {
    theme::id(name).unwrap_or(TokenId::MISSING)
}

impl Surface for Paper {
    fn rect(&mut self, _r: Rect, _c: Color) {}
    fn rect_outline(&mut self, _r: Rect, _w: f32, _c: Color) {}
    fn line(&mut self, _x0: f32, _y0: f32, _x1: f32, _y1: f32, _w: f32, _c: Color) {}
    fn polyline(&mut self, _pts: &[[f32; 2]], _w: f32, _c: Color, _closed: bool) {}

    fn text(&mut self, _px: f32, _x: f32, _y: f32, _s: &str, _c: Color, _t: f32, _a: Align) {}

    fn measure(&mut self, px: f32, s: &str, track: f32) -> f32 {
        self.measurements += 1;
        let n = s.chars().count();
        // Tracking is charged for the gaps BETWEEN characters, which is
        // one fewer than there are characters.
        n as f32 * px * CHAR_W + n.saturating_sub(1) as f32 * track
    }

    fn clip(&mut self, _r: Rect) -> bool {
        false
    }

    fn unclip(&mut self) {}

    fn can_clip(&self) -> bool {
        false
    }

    fn has_token(&mut self, name: &str) -> bool {
        id(name) != TokenId::MISSING
    }

    fn px(&mut self, name: &str) -> f32 {
        theme::resolved().px(id(name))
    }

    fn color(&mut self, name: &str) -> Color {
        theme::resolved().color(id(name))
    }

    fn bed(&mut self, name: &str) -> Color {
        theme::resolved().bed(id(name))
    }

    fn flag(&mut self, name: &str) -> bool {
        theme::resolved().flag(id(name))
    }

    fn word(&mut self, name: &str) -> String {
        theme::enum_word_of(id(name)).unwrap_or_default()
    }

    fn class_state(&mut self, class: &str, state: State) -> StateInk {
        match theme::class_id(class) {
            Some(c) => StateInk::from(theme::resolved().class_state(c, state)),
            None => StateInk::raw(),
        }
    }

    fn epoch(&mut self) -> u32 {
        theme::epoch()
    }

    fn now(&self) -> f64 {
        0.0
    }

    fn mouse(&self) -> (f32, f32) {
        // Off the page, so nothing a test draws is ever hovered by
        // accident.
        (-1.0, -1.0)
    }

    fn scale(&self) -> f32 {
        1.0
    }
}
