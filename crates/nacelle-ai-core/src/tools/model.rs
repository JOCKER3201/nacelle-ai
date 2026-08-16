//! The typed shape of `nacelle-desktop.ron`, as this agent reads and
//! writes it.
//!
//! Patterned on the desktop's own model (nacelle-desktop,
//! `src/config/model.rs`) rather than invented here, because the two
//! programs edit the SAME file and must agree byte for byte about what
//! it means. Every field the desktop's model carries is carried here —
//! including ones this agent has no tool for, such as `variant` and
//! `screens` — so that a read-modify-write of the user's file never
//! drops a setting the desktop honours.
//!
//! Three properties, inherited from that model:
//!
//! **Three states, not two.** A file can say nothing about a setting,
//! or it can say "nothing" — and those are different answers. Saying
//! nothing lets the next file down the cascade answer; saying "nothing"
//! OUTRANKS it. That is [`Choice`], and it is why clearing a setting
//! REMOVES the field (for a control with no "none" to offer) or writes
//! `Off` (for one that has).
//!
//! **Everything defaulted.** RON parses all or nothing, so a file that
//! is merely INCOMPLETE — an old version's, a half-written one — must
//! still parse. `#[serde(default)]` on every struct is what makes a
//! missing field ordinary.
//!
//! **Nothing about the LOOK.** A default here is a default for a
//! setting, never for an appearance: an unset field is answered by the
//! cascade and ultimately by the theme, which is why so much of this is
//! `Option` rather than a number with an opinion in it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Whether a field is worth writing down: a field nothing was said
/// about is left out of the file entirely, which is what makes a
/// cleared setting indistinguishable from one that was never set.
fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// What one file says about one setting that NAMES something — a
/// theme, a layaut, a font family, a colour space.
///
/// `Off` is a user saying "none", and it has to beat a system file
/// that names one. Absence is the opposite answer: it hands the
/// question to the next file down.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Choice {
    /// Not written down. The rest of the cascade answers, and when
    /// nothing does, the program's own default stands. This is what an
    /// ABSENT field parses as, so it is never written out.
    #[default]
    Inherit,
    /// Written down as "nothing" — an explicit off that outranks
    /// whatever a system file names.
    Off,
    /// The name the setting takes.
    Named(String),
}

impl Choice {
    /// The name settled on, if one was. `Off` and `Inherit` both answer
    /// `None` — they differ in the CASCADE, not in the value.
    pub fn name(&self) -> Option<&str> {
        match self {
            Choice::Named(n) => Some(n.as_str()),
            _ => None,
        }
    }

    /// A name the user picked. An empty one is not a name: it means
    /// nothing was chosen, so the question goes back to the cascade.
    pub fn named(name: &str) -> Choice {
        let n = name.trim();
        if n.is_empty() {
            Choice::Inherit
        } else {
            Choice::Named(n.to_string())
        }
    }

    /// A control that offers "none" as one of its answers: nothing
    /// chosen is an explicit off, not a question passed on.
    pub fn or_off(name: Option<&str>) -> Choice {
        match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => Choice::Named(n.to_string()),
            None => Choice::Off,
        }
    }

    /// The old format's reading of a value for a key whose control
    /// offered NO "none" — a theme, a layaut, a sound set, a font
    /// family. Empty is an absence there.
    fn from_legacy(value: &str) -> Choice {
        Choice::named(value)
    }

    /// The same, for a key whose control DID offer one: the grading
    /// LUT, the ICC profile, the contrast variant. Empty was how the
    /// settings window wrote that answer, and it has to keep beating a
    /// system file that names something.
    fn from_legacy_offable(value: &str) -> Choice {
        Choice::or_off(Some(value))
    }
}

/// A document — or a part of one — laid over the same thing read from
/// a less specific file. The cascade is per FIELD: the user's file
/// answering the theme does not stop the system file answering the
/// sound set.
pub trait Layered {
    /// `self` is the more specific file. Everything it does not carry
    /// comes from `base`.
    fn over(self, base: Self) -> Self;
}

impl Layered for Choice {
    fn over(self, base: Self) -> Self {
        match self {
            Choice::Inherit => base,
            settled => settled,
        }
    }
}

impl<T> Layered for Option<T> {
    fn over(self, base: Self) -> Self {
        self.or(base)
    }
}

impl Layered for BTreeMap<String, Choice> {
    fn over(self, mut base: Self) -> Self {
        base.extend(self);
        base
    }
}

/// Everything `nacelle-desktop.ron` can say. The FOLDER is the family
/// and the FILE is the program, so this type is the whole of one
/// program's configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopConf {
    /// The theme the engine loads. Absent = the built-in master.
    #[serde(skip_serializing_if = "is_default")]
    pub theme: Choice,
    /// The contrast variant on top of it. Carried for round-trip
    /// fidelity — this agent has no tool that sets it.
    #[serde(skip_serializing_if = "is_default")]
    pub variant: Choice,
    /// The DEFAULT desktop arrangement.
    #[serde(skip_serializing_if = "is_default")]
    pub layaut: Choice,
    /// One screen, one desktop: connector name → layaut. Carried for
    /// round-trip fidelity — this agent has no tool that sets it.
    #[serde(skip_serializing_if = "is_default")]
    pub screens: BTreeMap<String, Choice>,
    /// The sound set: a directory name under `sounds/`.
    #[serde(skip_serializing_if = "is_default")]
    pub sounds: Choice,
    #[serde(skip_serializing_if = "is_default")]
    pub term_font: FontConf,
    #[serde(skip_serializing_if = "is_default")]
    pub ui_font: FontConf,
    #[serde(skip_serializing_if = "is_default")]
    pub sound: SoundConf,
    #[serde(skip_serializing_if = "is_default")]
    pub grid: GridConf,
    #[serde(skip_serializing_if = "is_default")]
    pub color: ColorConf,
    #[serde(skip_serializing_if = "is_default")]
    pub blur: BlurConf,
}

impl Layered for DesktopConf {
    fn over(self, base: Self) -> Self {
        DesktopConf {
            theme: self.theme.over(base.theme),
            variant: self.variant.over(base.variant),
            layaut: self.layaut.over(base.layaut),
            screens: self.screens.over(base.screens),
            sounds: self.sounds.over(base.sounds),
            term_font: self.term_font.over(base.term_font),
            ui_font: self.ui_font.over(base.ui_font),
            sound: self.sound.over(base.sound),
            grid: self.grid.over(base.grid),
            color: self.color.over(base.color),
            blur: self.blur.over(base.blur),
        }
    }
}

impl DesktopConf {
    /// The same document as the old `Key=Value` file said it.
    ///
    /// An empty value is read the way the setter of that same key wrote
    /// one: `Choice::named` where the control has no "none" to offer,
    /// `Choice::or_off` for exactly the ones the settings window writes
    /// empty on purpose (the contrast variant, a per-screen assignment,
    /// the grading LUT and the ICC profile), and a blank number or
    /// switch is a line somebody typed and left, not an answer.
    pub fn from_legacy(kv: &std::collections::HashMap<String, String>) -> DesktopConf {
        let text = |key: &str| kv.get(key).map(|v| Choice::from_legacy(v)).unwrap_or_default();
        let offable = |key: &str| {
            kv.get(key)
                .map(|v| Choice::from_legacy_offable(v))
                .unwrap_or_default()
        };
        let num = |key: &str| kv.get(key).and_then(|v| v.trim().parse::<u32>().ok());
        let said = |key: &str| kv.get(key).map(|v| v.trim()).filter(|v| !v.is_empty());
        // `!= "0"` and not a parse: that is what the old readers did,
        // so `SoundTyping=yes` went on meaning yes.
        let flag = |key: &str| said(key).map(|v| v != "0");
        DesktopConf {
            theme: text("Theme"),
            variant: offable("Variant"),
            layaut: text("Layaut"),
            screens: legacy_screen_choices(kv),
            sounds: text("Sounds"),
            term_font: FontConf {
                size: kv.get("TermFontSize").and_then(|v| v.trim().parse::<f32>().ok()),
                family: text("TermFontFamily"),
                weight: text("TermFontWeight"),
            },
            ui_font: FontConf {
                size: kv.get("UIFontSize").and_then(|v| v.trim().parse::<f32>().ok()),
                family: text("UIFontFamily"),
                weight: text("UIFontWeight"),
            },
            sound: SoundConf {
                volume: num("SoundVolume"),
                typing: flag("SoundTyping"),
                ambient: flag("SoundAmbient"),
            },
            grid: GridConf {
                snap: said("GridSnap").map(|v| v == "1" || v.eq_ignore_ascii_case("true")),
                cols: num("GridCols"),
                rows: num("GridRows"),
                padding: num("GridPadding"),
            },
            color: ColorConf {
                depth: num("ColorDepth"),
                space: text("ColorSpace"),
                lut: offable("ColorLut"),
                icc: offable("ColorIcc"),
            },
            blur: BlurConf {
                radius: num("BlurRadius"),
                opacity: num("BlurOpacity"),
            },
        }
    }
}

/// The `Layaut[<connector>]=` family of the old format, as choices.
/// `Layaut [DP-1]` reads as `Layaut[DP-1]` — it was a file people typed
/// into, and a space before the bracket was not a different intention.
fn legacy_screen_choices(
    kv: &std::collections::HashMap<String, String>,
) -> BTreeMap<String, Choice> {
    let mut out = BTreeMap::new();
    for (key, value) in kv {
        let Some(inner) = key
            .trim()
            .strip_prefix("Layaut")
            .map(str::trim_start)
            .and_then(|rest| rest.strip_prefix('['))
            .and_then(|rest| rest.strip_suffix(']'))
        else {
            continue;
        };
        out.insert(inner.trim().to_string(), Choice::from_legacy_offable(value));
    }
    out
}

/// One font section — the terminal's or the interface's.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontConf {
    /// Size as a PERCENTAGE of what the theme asks for.
    #[serde(skip_serializing_if = "is_default")]
    pub size: Option<f32>,
    /// A family installed on the machine. Absent = the theme's.
    #[serde(skip_serializing_if = "is_default")]
    pub family: Choice,
    /// `regular`, `bold`, … Absent = the theme's.
    #[serde(skip_serializing_if = "is_default")]
    pub weight: Choice,
}

impl Layered for FontConf {
    fn over(self, base: Self) -> Self {
        FontConf {
            size: self.size.over(base.size),
            family: self.family.over(base.family),
            weight: self.weight.over(base.weight),
        }
    }
}

/// The sounds the interface makes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SoundConf {
    /// Master volume, percent.
    #[serde(skip_serializing_if = "is_default")]
    pub volume: Option<u32>,
    /// The key clicks the terminal makes.
    #[serde(skip_serializing_if = "is_default")]
    pub typing: Option<bool>,
    /// The ambient bed under everything.
    #[serde(skip_serializing_if = "is_default")]
    pub ambient: Option<bool>,
}

impl Layered for SoundConf {
    fn over(self, base: Self) -> Self {
        SoundConf {
            volume: self.volume.over(base.volume),
            typing: self.typing.over(base.typing),
            ambient: self.ambient.over(base.ambient),
        }
    }
}

/// The grid editor's own settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GridConf {
    /// Snapping is opt-in.
    #[serde(skip_serializing_if = "is_default")]
    pub snap: Option<bool>,
    #[serde(skip_serializing_if = "is_default")]
    pub cols: Option<u32>,
    #[serde(skip_serializing_if = "is_default")]
    pub rows: Option<u32>,
    /// The band kept clear around every panel, in device pixels. No
    /// default here and there must not be one — an unset padding is the
    /// theme's `layout.panel_gutter`.
    #[serde(skip_serializing_if = "is_default")]
    pub padding: Option<u32>,
}

impl Layered for GridConf {
    fn over(self, base: Self) -> Self {
        GridConf {
            snap: self.snap.over(base.snap),
            cols: self.cols.over(base.cols),
            rows: self.rows.over(base.rows),
            padding: self.padding.over(base.padding),
        }
    }
}

/// The colour pipeline: a Wayland-session matter throughout.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorConf {
    /// 8, 10, 12 or 16 bits.
    #[serde(skip_serializing_if = "is_default")]
    pub depth: Option<u32>,
    /// A colour-space name from the desktop's own list.
    #[serde(skip_serializing_if = "is_default")]
    pub space: Choice,
    /// A grading LUT: a file name under an assets `lut/` directory.
    #[serde(skip_serializing_if = "is_default")]
    pub lut: Choice,
    /// An ICC profile, likewise under `icc/`.
    #[serde(skip_serializing_if = "is_default")]
    pub icc: Choice,
}

impl Layered for ColorConf {
    fn over(self, base: Self) -> Self {
        ColorConf {
            depth: self.depth.over(base.depth),
            space: self.space.over(base.space),
            lut: self.lut.over(base.lut),
            icc: self.icc.over(base.icc),
        }
    }
}

/// The frosted glass every panel is drawn on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BlurConf {
    /// How deep the renderer's pyramid goes, percent of the theme's own.
    #[serde(skip_serializing_if = "is_default")]
    pub radius: Option<u32>,
    /// The glass tint's alpha, percent.
    #[serde(skip_serializing_if = "is_default")]
    pub opacity: Option<u32>,
}

impl Layered for BlurConf {
    fn over(self, base: Self) -> Self {
        BlurConf {
            radius: self.radius.over(base.radius),
            opacity: self.opacity.over(base.opacity),
        }
    }
}
