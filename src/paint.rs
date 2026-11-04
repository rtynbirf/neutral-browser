//! neutral-browser — stage 7: display list + software paint (PAINT).
//!
//! Chain: URL -> Broker -> TCP -> HTTP/1.1 -> body -> HTML -> DOM -> CSS
//!        -> STYLE -> LAYOUT -> DISPLAY LIST -> PAINT.
//!
//! This module is pure: no I/O, no panics, no recursion. It consumes the
//! LAYOUT box tree (layout::LayoutOut) and the computed STYLE
//! (css::StyleApply) and produces, per the MASTER_PROMPT chain, first a
//! DISPLAY LIST (the canonical list of drawing commands) and then a raster
//! PAINT of that list into a fixed RGBA canvas, serialized as binary PPM
//! (P6) with a frozen `PAINT v1` text dump.
//!
//! Implemented (a documented subset of CSS 2.1 ch. 11/14):
//!   * colors: `#rgb`/`#rrggbb`, `rgb()`/`rgba()` with numbers 0-255 or
//!     percentages (3 or 4 arguments), the 16 CSS 2.1 basic named colors,
//!     `transparent`; alpha composites `over` the existing pixels
//!     (deterministic integer math, round half up on .5);
//!   * canvas: fixed VIEWPORT_W x VIEWPORT_H (layout constants), initial
//!     color white (CSS initial canvas color);
//!   * background propagation (CSS 2.1 §14.2 subset): a valid
//!     `background-color` on `html` paints the whole canvas and html emits
//!     no own fill; otherwise a valid `background-color` on `body`
//!     propagates the same way; otherwise the canvas stays white;
//!   * boxes: background-color (if present and supported) fills the box's
//!     border-box rectangle; borders paint only `border-*-style: solid`
//!     longhands (4 non-overlapping strips, top/bottom full width, left/
//!     right between them); missing border color falls back to the
//!     element's `color`, then to black (initial `currentcolor`
//!     approximated — documented deviation);
//!   * TEXT (stage 9): every laid-out word paints glyph bitmaps from the
//!     embedded font (font_data, nearest-neighbor integer scale to the
//!     run's font size) in the run element's `color` (own then inherited,
//!     black when absent); HUMAN_FIRST contrast norms are measured for
//!     every text command against the effective background under the
//!     sample point: WCAG 2.2 ratio (≥ 4.5) and APCA 0.1.9 Lc (≥ 75);
//!     minima and violations are aggregated on the DisplayList;
//!   * paint order: display-list order = document order (ancestors before
//!     descendants, earlier siblings first), later commands paint OVER
//!     earlier ones; display:none subtrees are absent from LAYOUT already;
//!   * clipping happens at raster time (the dump shows ideal geometry).
//!
//! Documented subset deviations (each a deliberate, frozen decision):
//!   * glyphs missing from the embedded font are skipped (already counted
//!     at layout time); no kerning/shaping/ligatures (mono advance);
//!   * images, gradients, box-shadow, outline, border-radius,
//!     background-image/repeat/position/attachment, opacity, transforms —
//!     NOT_IMPLEMENTED (unknown to paint, ignored);
//!   * border styles other than `solid` (dashed/dotted/double/...) count
//!     one PAINT error each and fall back to `none` (not painted);
//!   * `currentcolor` as an explicit value is unsupported (falls into the
//!     color fallback chain instead);
//!   * hex alpha forms `#rgba`/`#rrggbbaa` are unsupported (such a value
//!     is an error); space-separated rgb() syntax is unsupported;
//!   * shorthand `border`/`border-style`/`border-color` are not expanded
//!     (consistent with LAYOUT, which reads only longhands);
//!   * the PPM output is binary P6, no compression — the artifact is
//!     deterministic byte-for-byte; PNG is NOT_IMPLEMENTED.

use crate::css::StyleApply;
use crate::font_data;
use crate::layout::{LayoutOut, VIEWPORT_H, VIEWPORT_W};

// ---------------- colors ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const WHITE: Rgba = Rgba { r: 255, g: 255, b: 255, a: 255 };
    pub const BLACK: Rgba = Rgba { r: 0, g: 0, b: 0, a: 255 };
    pub const TRANSPARENT: Rgba = Rgba { r: 0, g: 0, b: 0, a: 0 };

    /// Frozen serialization: `#rrggbb` when opaque, `#rrggbbaa` otherwise.
    pub fn hex(self) -> String {
        if self.a == 255 {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }
}

/// The 16 CSS 2.1 basic named colors (frozen table).
const NAMED: &[(&str, Rgba)] = &[
    ("aqua", Rgba { r: 0, g: 255, b: 255, a: 255 }),
    ("black", Rgba::BLACK),
    ("blue", Rgba { r: 0, g: 0, b: 255, a: 255 }),
    ("fuchsia", Rgba { r: 255, g: 0, b: 255, a: 255 }),
    ("gray", Rgba { r: 128, g: 128, b: 128, a: 255 }),
    ("green", Rgba { r: 0, g: 128, b: 0, a: 255 }),
    ("lime", Rgba { r: 0, g: 255, b: 0, a: 255 }),
    ("maroon", Rgba { r: 128, g: 0, b: 0, a: 255 }),
    ("navy", Rgba { r: 0, g: 0, b: 128, a: 255 }),
    ("olive", Rgba { r: 128, g: 128, b: 0, a: 255 }),
    ("purple", Rgba { r: 128, g: 0, b: 128, a: 255 }),
    ("red", Rgba { r: 255, g: 0, b: 0, a: 255 }),
    ("silver", Rgba { r: 192, g: 192, b: 192, a: 255 }),
    ("teal", Rgba { r: 0, g: 128, b: 128, a: 255 }),
    ("white", Rgba::WHITE),
    ("yellow", Rgba { r: 255, g: 255, b: 0, a: 255 }),
];

/// f64 value -> u8 clamped to 0..=255, round half away from zero.
fn clamp_byte(f: f64) -> u8 {
    if f.is_nan() {
        return 0;
    }
    let r = f.round();
    if r <= 0.0 {
        0
    } else if r >= 255.0 {
        255
    } else {
        r as u8
    }
}

/// One rgb()/rgba() component. Numbers are 0-255 for color channels and
/// 0-1 for the alpha channel; percentages are always of 255 (50% = 127.5).
fn parse_component(s: &str, is_alpha: bool) -> Option<u8> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(pc) = t.strip_suffix('%') {
        let v: f64 = pc.trim().parse().ok()?;
        return Some(clamp_byte(v * 255.0 / 100.0));
    }
    let v: f64 = t.parse().ok()?;
    Some(clamp_byte(if is_alpha { v * 255.0 } else { v }))
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Parse the supported color subset. `None` = unsupported (the caller
/// counts an error). Case-insensitive; whitespace tolerated.
pub fn parse_color(raw: &str) -> Option<Rgba> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    if lower == "transparent" {
        return Some(Rgba::TRANSPARENT);
    }
    for (name, c) in NAMED {
        if *name == lower {
            return Some(*c);
        }
    }
    if let Some(hex) = s.strip_prefix('#') {
        let hb = hex.as_bytes();
        match hb.len() {
            3 => {
                let (a, b, c) = (hex_digit(hb[0])?, hex_digit(hb[1])?, hex_digit(hb[2])?);
                return Some(Rgba { r: a * 17, g: b * 17, b: c * 17, a: 255 });
            }
            6 => {
                let (r1, r2, g1, g2, b1, b2) = (
                    hex_digit(hb[0])?, hex_digit(hb[1])?, hex_digit(hb[2])?,
                    hex_digit(hb[3])?, hex_digit(hb[4])?, hex_digit(hb[5])?,
                );
                return Some(Rgba {
                    r: r1 * 16 + r2,
                    g: g1 * 16 + g2,
                    b: b1 * 16 + b2,
                    a: 255,
                });
            }
            _ => return None, // #rgba/#rrggbbaa NOT_IMPLEMENTED
        }
    }
    // rgb(...) / rgba(...), 3 or 4 comma-separated components
    let (head, rest) = match lower.find('(') {
        Some(i) if s.ends_with(')') => (&s[..i], &s[i + 1..s.len() - 1]),
        _ => return None,
    };
    let head = head.trim();
    if head != "rgb" && head != "rgba" {
        return None;
    }
    let parts: Vec<&str> = rest.split(',').collect();
    if parts.len() != 3 && parts.len() != 4 {
        return None;
    }
    let r = parse_component(parts[0], false)?;
    let g = parse_component(parts[1], false)?;
    let b = parse_component(parts[2], false)?;
    let a = if parts.len() == 4 { parse_component(parts[3], true)? } else { 255 };
    Some(Rgba { r, g, b, a })
}

/// Deterministic integer `over` composite of one pixel channel pair.
#[inline]
fn over(src: u8, dst: u8, a: u32) -> u8 {
    // a in 1..=254 here (0 and 255 are shortcut by the caller);
    // round((src*a + dst*(255-a)) / 255) with round-half-up
    let v = (src as u32 * a + dst as u32 * (255 - a) + 127) / 255;
    v as u8
}

// ---------------- display list ----------------

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    /// Fill an axis-aligned rectangle with a color (alpha `over`-blends).
    Fill { x: i64, y: i64, w: i64, h: i64, c: Rgba },
    /// Draw one laid-out word: baseline origin (x, y), the run element's
    /// color, the font size used by LAYOUT, the effective background under
    /// the sample point, and the frozen contrast measurements (WCAG ratio
    /// x100, APCA Lc x100) computed when the display list was built.
    Text {
        x: i64,
        y: i64,
        text: String,
        c: Rgba,
        fs: i64,
        bg: Rgba,
        wcag_x100: i64,
        apca_x100: i64,
    },
}

#[derive(Debug, PartialEq)]
pub struct DisplayList {
    pub canvas: Rgba,        // initial canvas color (after §14.2 propagation)
    pub cmds: Vec<Cmd>,      // paint order: later paints over earlier
    pub boxes_painted: usize, // boxes that contributed >= 1 command
    pub errors: usize,       // unsupported values of known paint properties
    // ---- stage 9: text paint + contrast norms ----
    pub text_cmds: usize,          // Text commands
    pub text_chars: usize,         // characters handed to the raster
    pub wcag_min_x100: i64,        // min WCAG ratio x100 over texts
    pub apca_min_x100: i64,        // min APCA Lc x100 over texts
    pub contrast_violations: usize, // WCAG ratio < 4.5 (x100 < 450)
    pub apca_violations: usize,     // APCA Lc < 75 (x100 < 7500)
}

/// Own cascaded winners of the element with document-order `index`
/// (css::apply pushes elements in document order -> binary search).
fn lookup_decls<'a>(ap: &'a StyleApply, index: usize) -> &'a [(String, String)] {
    match ap.elements.binary_search_by_key(&index, |e| e.index) {
        Ok(pos) => &ap.elements[pos].decls,
        Err(_) => &[],
    }
}

/// First match for `name` among the element's own cascaded decls, then its
/// inherited values (css::apply materializes inherited computed values,
/// including color/font-size/line-height — stage 5). Plain names in both.
fn find_value<'a>(ap: &'a StyleApply, index: usize, name: &str) -> Option<&'a str> {
    let pos = ap.elements.binary_search_by_key(&index, |e| e.index).ok()?;
    let se = &ap.elements[pos];
    se.decls
        .iter()
        .find(|(k, _)| k == name)
        .or_else(|| se.inherited.iter().find(|(k, _)| k == name))
        .map(|(_, v)| v.as_str())
}

// ---------------- contrast norms (HUMAN_FIRST, stage 9) ----------------

/// WCAG 2.2 relative luminance of an opaque sRGB color.
fn wcag_lum(c: Rgba) -> f64 {
    fn lin(ch: u8) -> f64 {
        let s = ch as f64 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG 2.2 contrast ratio x100 (4.5:1 -> 450), round half away from zero.
fn wcag_ratio_x100(a: Rgba, b: Rgba) -> i64 {
    let (l1, l2) = (wcag_lum(a), wcag_lum(b));
    let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    ((hi + 0.05) / (lo + 0.05) * 100.0).round() as i64
}

/// APCA 0.1.9 perceptual lightness of an sRGB color (Y with exponent 2.4).
fn apca_y(c: Rgba) -> f64 {
    let r = (c.r as f64 / 255.0).powf(2.4);
    let g = (c.g as f64 / 255.0).powf(2.4);
    let b = (c.b as f64 / 255.0).powf(2.4);
    0.2126729 * r + 0.7151522 * g + 0.0721750 * b
}

/// APCA 0.1.9 soft black clamp (blkThrs 0.022, blkClmp 1.414).
fn apca_soft(y: f64) -> f64 {
    if y >= 0.022 {
        y
    } else {
        y + (0.022f64 - y).powf(1.414)
    }
}

/// APCA 0.1.9 Lc x100 for text `fg` on background `bg` (polarity picked by
/// the Y comparison; loClip 0.1, loOffset 0.027; constants frozen from the
/// Myndex/SAPC-APCA apca_reference 0.1.9). Lc is reported x100 like the
/// WCAG ratio (field `apca_x100`, HEALTH threshold 7500 = Lc 75):
/// black on white yields 10604, white on black 10788 (the 0.62/0.65
/// exponent pair makes light-on-dark slightly higher — the documented
/// APCA asymmetry).
fn apca_lc_x100(fg: Rgba, bg: Rgba) -> i64 {
    let yt = apca_soft(apca_y(fg));
    let yb = apca_soft(apca_y(bg));
    let sapc = if yb > yt {
        // dark text on light background (BoW)
        (yb.powf(0.56) - yt.powf(0.57)) * 1.14
    } else {
        // light text on dark background (WoB): text minus background —
        // the result must stay positive like the BoW branch; subtracting
        // the other way silently reported Lc 0 for EVERY light-on-dark
        // text (caught by the stage 9 text fixture e2e)
        (yt.powf(0.62) - yb.powf(0.65)) * 1.14
    };
    if sapc < 0.1 {
        0
    } else {
        // Lc = (sapc - 0.027) * 100 on the APCA scale; x100 for the frozen
        // contract -> * 10000
        ((sapc - 0.027) * 10000.0).round() as i64
    }
}

// HUMAN_FIRST health norms (docs/HUMAN_FIRST.md) — frozen as named
// constants so the architecture gate can pin NAME + VALUE like LIMITS.
// WCAG 2.2 1.4.3 AA floor for normal text (4.5:1 -> x100); APCA Lc 75
// (the charter's floor for body text) -> x100.
pub const WCAG_FLOOR_X100: i64 = 450;
pub const APCA_FLOOR_X100: i64 = 7500;

/// Effective opaque color under the sample point (x, y): the canvas color
/// with every Fill command covering the point composited over it, in paint
/// order. Text commands never contribute (they are the foreground).
fn effective_bg(cmds: &[Cmd], canvas: Rgba, x: i64, y: i64) -> Rgba {
    let mut acc = canvas;
    for cmd in cmds {
        if let Cmd::Fill { x: fx, y: fy, w, h, c } = *cmd {
            if c.a > 0 && x >= fx && x < fx.saturating_add(w) && y >= fy && y < fy.saturating_add(h)
            {
                acc = if c.a == 255 {
                    c
                } else {
                    let a = c.a as u32;
                    Rgba {
                        r: over(c.r, acc.r, a),
                        g: over(c.g, acc.g, a),
                        b: over(c.b, acc.b, a),
                        a: 255,
                    }
                };
            }
        }
    }
    acc
}

fn get_decl<'a>(decls: &'a [(String, String)], name: &str) -> Option<&'a str> {
    decls
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// Background color of one box: parse `background-color`; a declared but
/// unsupported value counts one error and falls back to transparent.
fn box_bg(decls: &[(String, String)], errors: &mut usize) -> Rgba {
    match get_decl(decls, "background-color") {
        None => Rgba::TRANSPARENT,
        Some(v) => parse_color(v).unwrap_or_else(|| {
            *errors += 1;
            Rgba::TRANSPARENT
        }),
    }
}

/// Border style of one side (longhand only; shorthand is not expanded).
/// `Some(true)` = paint solid; `Some(false)` = none/hidden (not painted);
/// `None` = declared but unsupported (caller counts an error).
fn border_style_side(decls: &[(String, String)], side: &str) -> Option<bool> {
    match get_decl(decls, &format!("border-{}-style", side)) {
        None => Some(false), // initial border-style is none (no UA sheet)
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "solid" => Some(true),
            "none" | "hidden" => Some(false),
            _ => None, // dashed/dotted/double/... NOT_IMPLEMENTED
        },
    }
}

/// Border color of one side: border-X-color, else the element `color`,
/// else black (initial currentcolor approximated — documented deviation).
fn border_color_side(decls: &[(String, String)], side: &str, errors: &mut usize) -> Rgba {
    match get_decl(decls, &format!("border-{}-color", side)) {
        Some(v) => match parse_color(v) {
            Some(c) => c,
            None => {
                *errors += 1;
                Rgba::BLACK
            }
        },
        None => match get_decl(decls, "color") {
            Some(v) => match parse_color(v) {
                Some(c) => c,
                None => {
                    *errors += 1;
                    Rgba::BLACK
                }
            },
            None => Rgba::BLACK,
        },
    }
}

/// Build the display list. Iterative: one pass over LAYOUT boxes (already
/// document order = ancestors before descendants).
pub fn display_list(lo: &LayoutOut, ap: &StyleApply) -> DisplayList {
    let mut errors = 0usize;
    let mut bg_cache: Vec<(usize, Rgba)> = Vec::new();

    // ---- canvas color: §14.2 propagation subset ----
    let find_index = |path: &str| lo.boxes.iter().find(|b| b.path == path).map(|b| b.index);
    let html_idx = find_index("html");
    let body_idx = find_index("html>body");
    let mut canvas = Rgba::WHITE;
    let mut propagated: Option<usize> = None; // element index whose bg owns the canvas
    for idx in [html_idx, body_idx].into_iter().flatten() {
        let c = box_bg(lookup_decls(ap, idx), &mut errors);
        bg_cache.push((idx, c));
        if c.a > 0 {
            canvas = c;
            propagated = Some(idx);
            break; // html wins when present; body only if html is transparent
        }
    }

    let mut cmds: Vec<Cmd> = Vec::new();
    let mut boxes_painted = 0usize;
    // bg_cache from the propagation pass above keeps html/body colors
    // (a declared-but-unsupported value is counted ONCE there)
    let mut text_cmds = 0usize;
    let mut text_chars = 0usize;
    let mut wcag_min_x100 = i64::MAX;
    let mut apca_min_x100 = i64::MAX;
    let mut contrast_violations = 0usize;
    let mut apca_violations = 0usize;
    for b in &lo.boxes {
        // anonymous lines boxes paint no background/borders; their words
        // become Text commands below
        if b.kind == crate::layout::BoxKind::Lines {
            let mut contributed = 0usize;
            let content_left = b.x + b.bl + b.pl;
            let content_top = b.y + b.bt + b.pt;
            for line in &b.lines {
                for word in &line.words {
                    let color = match find_value(ap, word.elem, "color") {
                        Some(v) => match parse_color(v) {
                            Some(c) => c,
                            None => {
                                errors += 1;
                                Rgba::BLACK
                            }
                        },
                        None => Rgba::BLACK, // CSS initial color
                    };
                    let (bx, by) = (
                        content_left + word.x,
                        content_top + line.y + line.baseline,
                    );
                    // contrast sample: middle of the word's x-height zone
                    let asc = font_data::ascent_px(word.fs);
                    let width: i64 = word
                        .text
                        .chars()
                        .map(|ch| {
                            font_data::lookup(ch as u32)
                                .map(|g| font_data::advance_of(g, word.fs))
                                .unwrap_or_else(|| {
                                    font_data::lookup(0x20)
                                        .map(|g| font_data::advance_of(g, word.fs))
                                        .unwrap_or(0)
                                })
                        })
                        .sum();
                    let (sx, sy) = (bx + width / 2, by - asc / 2);
                    let bg = effective_bg(&cmds, canvas, sx, sy);
                    // composite the text color over the bg if translucent
                    let fg = if color.a == 255 {
                        color
                    } else {
                        let a = color.a as u32;
                        Rgba {
                            r: over(color.r, bg.r, a),
                            g: over(color.g, bg.g, a),
                            b: over(color.b, bg.b, a),
                            a: 255,
                        }
                    };
                    let wcag = wcag_ratio_x100(fg, bg);
                    let apca = apca_lc_x100(fg, bg);
                    wcag_min_x100 = wcag_min_x100.min(wcag);
                    apca_min_x100 = apca_min_x100.min(apca);
                    if wcag < WCAG_FLOOR_X100 {
                        contrast_violations += 1;
                    }
                    if apca < APCA_FLOOR_X100 {
                        apca_violations += 1;
                    }
                    text_chars += word.text.chars().count();
                    cmds.push(Cmd::Text {
                        x: bx,
                        y: by,
                        text: word.text.clone(),
                        c: color,
                        fs: word.fs,
                        bg,
                        wcag_x100: wcag,
                        apca_x100: apca,
                    });
                    contributed += 1;
                }
            }
            if contributed > 0 {
                boxes_painted += 1;
            }
            text_cmds += contributed;
            continue;
        }
        let decls = lookup_decls(ap, b.index);
        let mut contributed = 0usize;

        // background fill over the whole border-box (borders paint on top)
        let bg = if Some(b.index) == propagated {
            Rgba::TRANSPARENT // the canvas itself carries this color
        } else if let Some((_, c)) = bg_cache.iter().find(|(i, _)| *i == b.index) {
            *c
        } else {
            box_bg(decls, &mut errors)
        };
        if bg.a > 0 {
            cmds.push(Cmd::Fill {
                x: b.x,
                y: b.y,
                w: b.w(),
                h: b.border_h(),
                c: bg,
            });
            contributed += 1;
        }

        // solid border longhands -> 4 non-overlapping strips
        let (x, y, w, h) = (b.x, b.y, b.w(), b.border_h());
        let sides: [(&str, i64, i64, i64, i64); 4] = [
            // (side, sx, sy, sw, sh) — strips below are clamped to the box
            ("top", x, y, w, b.bt),
            ("bottom", x, y + h - b.bb, w, b.bb),
            ("left", x, y + b.bt, b.bl, h - b.bt - b.bb),
            ("right", x + w - b.br, y + b.bt, b.br, h - b.bt - b.bb),
        ];
        for (side, sx, sy, sw, sh) in sides {
            match border_style_side(decls, side) {
                Some(true) => {
                    if sw > 0 && sh > 0 {
                        let c = border_color_side(decls, side, &mut errors);
                        cmds.push(Cmd::Fill { x: sx, y: sy, w: sw, h: sh, c });
                        contributed += 1;
                    }
                }
                Some(false) => {}
                None => errors += 1, // unsupported border style value
            }
        }
        if contributed > 0 {
            boxes_painted += 1;
        }
    }

    DisplayList {
        canvas,
        cmds,
        boxes_painted,
        errors,
        text_cmds,
        text_chars,
        wcag_min_x100: if wcag_min_x100 == i64::MAX { 0 } else { wcag_min_x100 },
        apca_min_x100: if apca_min_x100 == i64::MAX { 0 } else { apca_min_x100 },
        contrast_violations,
        apca_violations,
    }
}

// ---------------- raster (PAINT) ----------------

#[derive(Debug)]
pub struct Canvas {
    pub w: i64,
    pub h: i64,
    /// RGBA, 4 bytes per pixel, row-major, top-left origin.
    pub px: Vec<u8>,
}

impl Canvas {
    fn new(w: i64, h: i64, c: Rgba) -> Canvas {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            px.extend_from_slice(&[c.r, c.g, c.b, c.a]);
        }
        Canvas { w, h, px }
    }
}

/// Paint the display list into the fixed viewport canvas. Later commands
/// paint over earlier ones; rectangles and glyph bitmaps are clipped at
/// raster time.
pub fn raster(dl: &DisplayList) -> Canvas {
    let mut canvas = Canvas::new(VIEWPORT_W, VIEWPORT_H, dl.canvas);
    for cmd in &dl.cmds {
        match cmd {
            Cmd::Fill { x, y, w, h, c } => {
                fill_rect(&mut canvas, *x, *y, *w, *h, *c);
            }
            Cmd::Text {
                x,
                y,
                text,
                c,
                fs,
                ..
            } => {
                draw_text(&mut canvas, *x, *y, text, *c, *fs);
            }
        }
    }
    canvas
}

/// Fill one clipped rectangle (alpha `over`-blend, round half up).
fn fill_rect(canvas: &mut Canvas, x: i64, y: i64, w: i64, h: i64, c: Rgba) {
    if w <= 0 || h <= 0 {
        return;
    }
    // clip to the canvas in i64 (coordinates may be negative or huge)
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x.saturating_add(w)).min(canvas.w);
    let y1 = (y.saturating_add(h)).min(canvas.h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for py in y0..y1 {
        let row = (py * canvas.w * 4) as usize;
        for px_i in x0..x1 {
            let off = row + (px_i * 4) as usize;
            if c.a == 255 {
                canvas.px[off] = c.r;
                canvas.px[off + 1] = c.g;
                canvas.px[off + 2] = c.b;
                canvas.px[off + 3] = 255;
            } else {
                // over-blend (a == 0 was never emitted as a command)
                let a = c.a as u32;
                canvas.px[off] = over(c.r, canvas.px[off], a);
                canvas.px[off + 1] = over(c.g, canvas.px[off + 1], a);
                canvas.px[off + 2] = over(c.b, canvas.px[off + 2], a);
            }
        }
    }
}

/// Draw one word: glyph bitmaps from the embedded font, nearest-neighbor
/// integer scale from the 16px base to `fs`, pen advanced by the same
/// deterministic advance LAYOUT used. Missing glyphs skip (counted at
/// layout time) but still advance (as a space) to keep positions equal.
fn draw_text(canvas: &mut Canvas, x: i64, y: i64, text: &str, c: Rgba, fs: i64) {
    let em = font_data::EM;
    let space = font_data::lookup(0x20).map(|g| font_data::advance_of(g, fs)).unwrap_or(0);
    let mut pen = x;
    for ch in text.chars() {
        match font_data::lookup(ch as u32) {
            None => {
                pen += space;
            }
            Some(g) => {
                if fs > 0 && g.gw > 0 && g.gh > 0 {
                    let em_i = em as i64;
                    // scaled bitmap dims (floor) and offsets (round half up)
                    let gw = (g.gw as i64 * fs) / em_i;
                    let gh = (g.gh as i64 * fs) / em_i;
                    if gw > 0 && gh > 0 {
                        let gx0 = pen + (g.gx as i64 * fs + em_i / 2) / em_i;
                        let gy0 = y - font_data::ascent_px(fs) + (g.gy as i64 * fs + em_i / 2) / em_i;
                        for py in 0..gh {
                            let ay = gy0 + py;
                            if ay < 0 || ay >= canvas.h {
                                continue;
                            }
                            let sy = (py * em_i) / fs;
                            for pxx in 0..gw {
                                let ax = gx0 + pxx;
                                if ax < 0 || ax >= canvas.w {
                                    continue;
                                }
                                let sx = (pxx * em_i) / fs;
                                let idx = g.off as usize + (sy * g.gw as i64 + sx) as usize;
                                let a = font_data::BITMAPS[idx] as u32;
                                if a == 0 {
                                    continue;
                                }
                                let off = ((ay * canvas.w + ax) * 4) as usize;
                                if a == 255 {
                                    canvas.px[off] = c.r;
                                    canvas.px[off + 1] = c.g;
                                    canvas.px[off + 2] = c.b;
                                    canvas.px[off + 3] = 255;
                                } else {
                                    canvas.px[off] = over(c.r, canvas.px[off], a);
                                    canvas.px[off + 1] = over(c.g, canvas.px[off + 1], a);
                                    canvas.px[off + 2] = over(c.b, canvas.px[off + 2], a);
                                }
                            }
                        }
                    }
                }
                pen += font_data::advance_of(g, fs);
            }
        }
    }
}

/// Serialize as binary PPM (P6): header + raw RGB rows. Alpha is already
/// composited into the pixels (the canvas starts opaque), so it is dropped.
pub fn to_ppm(c: &Canvas) -> Vec<u8> {
    let mut out = Vec::with_capacity(15 + (c.w * c.h * 3) as usize);
    out.extend_from_slice(format!("P6\n{} {}\n255\n", c.w, c.h).as_bytes());
    for i in 0..(c.w * c.h) as usize {
        out.push(c.px[i * 4]);
        out.push(c.px[i * 4 + 1]);
        out.push(c.px[i * 4 + 2]);
    }
    out
}

/// Frozen dump format (machine-verified by tests):
/// `PAINT v2` header; `CANVAS <hex>` initial canvas color; one line per
/// command in paint order with the IDEAL (unclipped) geometry/colors:
///   F x y w h <hex>                      filled rectangle
///   T x y <color> <bg> <wcag> <apca> <text>   word at baseline (x, y);
///                                        wcag/apca are the frozen
///                                        contrast measurements x100
/// colors serialize as `#rrggbb` when opaque, `#rrggbbaa` otherwise; text
/// runs to the end of the line.
pub fn dump(dl: &DisplayList) -> String {
    let mut s = String::from("PAINT v2\n");
    s.push_str(&format!("CANVAS {}\n", dl.canvas.hex()));
    for cmd in &dl.cmds {
        match cmd {
            Cmd::Fill { x, y, w, h, c } => {
                s.push_str(&format!("F {} {} {} {} {}\n", x, y, w, h, c.hex()));
            }
            Cmd::Text {
                x,
                y,
                text,
                c,
                bg,
                wcag_x100,
                apca_x100,
                ..
            } => {
                s.push_str(&format!(
                    "T {} {} {} {} {} {} {}\n",
                    x,
                    y,
                    c.hex(),
                    bg.hex(),
                    wcag_x100,
                    apca_x100,
                    text
                ));
            }
        }
    }
    s
}

// ---------------- tests ----------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Заглушка: Text-команды не ожидаются в проверках границ/рамок.
    fn unreachable_text_cmd() {
        panic!("unexpected Text command in a border/geometry test");
    }
    use crate::css;
    use crate::dom;
    use crate::layout;

    fn painted(html: &str, css_src: &str) -> (DisplayList, Canvas, layout::LayoutOut) {
        let doc = dom::build(html);
        let o = css::parse(css_src);
        let ap = css::apply(&doc.dom, &o.rules);
        let lo = layout::run(&doc.dom, &ap);
        let dl = display_list(&lo, &ap);
        let cv = raster(&dl);
        (dl, cv, lo)
    }

    fn pixel(cv: &Canvas, x: i64, y: i64) -> Rgba {
        let off = ((y * cv.w + x) * 4) as usize;
        Rgba {
            r: cv.px[off],
            g: cv.px[off + 1],
            b: cv.px[off + 2],
            a: cv.px[off + 3],
        }
    }

    #[test]
    fn color_parsing() {
        // hex
        assert_eq!(parse_color("#fff"), Some(Rgba { r: 255, g: 255, b: 255, a: 255 }));
        assert_eq!(parse_color("#000"), Some(Rgba::BLACK));
        assert_eq!(parse_color("#AbC"), Some(Rgba { r: 0xaa, g: 0xbb, b: 0xcc, a: 255 }));
        assert_eq!(parse_color("#ff0000"), Some(Rgba { r: 255, g: 0, b: 0, a: 255 }));
        assert_eq!(parse_color("#FF0000"), Some(Rgba { r: 255, g: 0, b: 0, a: 255 }));
        assert_eq!(parse_color("#ff00"), None); // 4 digits unsupported
        assert_eq!(parse_color("#rrggbbaa"), None);
        assert_eq!(parse_color("#f"), None);
        assert_eq!(parse_color("#ggg"), None);
        assert_eq!(parse_color("ff0000"), None); // missing #
        // named (case-insensitive)
        assert_eq!(parse_color("red"), parse_color("RED"));
        assert_eq!(parse_color("Red"), Some(Rgba { r: 255, g: 0, b: 0, a: 255 }));
        assert_eq!(parse_color("teal"), Some(Rgba { r: 0, g: 128, b: 128, a: 255 }));
        assert_eq!(parse_color("rebeccapurple"), None); // not in the 16
        assert_eq!(parse_color("transparent"), Some(Rgba::TRANSPARENT));
        // rgb() numbers, clamped
        assert_eq!(
            parse_color("rgb(1,2,3)"),
            Some(Rgba { r: 1, g: 2, b: 3, a: 255 })
        );
        assert_eq!(
            parse_color("rgb( 300 , -5 , 0 )"),
            Some(Rgba { r: 255, g: 0, b: 0, a: 255 })
        );
        assert_eq!(parse_color("rgb(1,2)"), None);
        assert_eq!(parse_color("rgb(1,2,3,4,5)"), None);
        assert_eq!(parse_color("rgb(1,,3)"), None);
        assert_eq!(parse_color("rgb(1 2 3)"), None); // space syntax unsupported
        // percentages
        assert_eq!(
            parse_color("rgb(100%,0%,0%)"),
            Some(Rgba { r: 255, g: 0, b: 0, a: 255 })
        );
        assert_eq!(parse_color("rgb(50%,0%,0%)").map(|c| c.r), Some(128)); // 127.5 rounds away
        // alpha
        assert_eq!(
            parse_color("rgba(255,0,0,0.5)").map(|c| c.a),
            Some(128) // 127.5 -> 128
        );
        assert_eq!(
            parse_color("rgba(255,0,0,50%)").map(|c| c.a),
            Some(128)
        );
        assert_eq!(
            parse_color("rgb(255,0,0,0)"), // legacy 4-arg rgb
            Some(Rgba { r: 255, g: 0, b: 0, a: 0 })
        );
        assert_eq!(
            parse_color("rgba(0,0,0,1.0)"),
            Some(Rgba { r: 0, g: 0, b: 0, a: 255 })
        );
        assert_eq!(parse_color("rgba(255,0,0,2)").map(|c| c.a), Some(255)); // clamp
        // garbage
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("   "), None);
        assert_eq!(parse_color("12px"), None);
        assert_eq!(parse_color("currentcolor"), None); // NOT_IMPLEMENTED
        assert_eq!(parse_color("inherit"), None);
        // serialization
        assert_eq!(Rgba::BLACK.hex(), "#000000");
        assert_eq!(Rgba::WHITE.hex(), "#ffffff");
        assert_eq!(Rgba::TRANSPARENT.hex(), "#00000000");
    }

    #[test]
    fn over_blending_math() {
        // 50% red over white: R = round((255*128 + 255*127)/255) = 255,
        // G/B = round((0*128 + 255*127)/255) = 127
        assert_eq!(over(255, 255, 128), 255);
        assert_eq!(over(0, 255, 128), 127);
        // alpha 254: mostly source
        assert_eq!(u32::from(over(10, 200, 254)), (10u32 * 254 + 200 + 127) / 255);
    }

    #[test]
    fn canvas_propagation_14_2() {
        // html background paints the whole canvas; html emits no fill
        let (dl, cv, _lo) = painted("<html><body><div>x</div></body></html>", "html { background-color: #123456 }");
        assert_eq!(dl.canvas, Rgba { r: 0x12, g: 0x34, b: 0x56, a: 255 });
        // stage 9: html paints no fill (canvas carries the color), but the
        // 'x' text paints: exactly one Text command
        assert_eq!(dl.cmds.len(), 1);
        assert!(matches!(dl.cmds[0], Cmd::Text { .. }));
        assert_eq!(pixel(&cv, 0, 0), dl.canvas);
        assert_eq!(pixel(&cv, 799, 599), dl.canvas);

        // body propagates only when html is transparent; body then has no fill
        let (dl, _cv, _l) = painted("<body><div>x</div></body>", "body { background-color: navy }");
        assert_eq!(dl.canvas, Rgba { r: 0, g: 0, b: 128, a: 255 });
        assert_eq!(dl.cmds.len(), 1); // only the 'x' Text command

        // html invalid + body valid: error counted, body still propagates
        let (dl, _cv, _l) = painted(
            "<html><body><div>x</div></body></html>",
            "html { background-color: 12px } body { background-color: red }",
        );
        assert_eq!(dl.errors, 1);
        assert_eq!(dl.canvas, Rgba { r: 255, g: 0, b: 0, a: 255 });

        // neither set: white canvas
        let (dl, cv, _l) = painted("<body><div>x</div></body>", "div { width: 10px; height: 10px }");
        assert_eq!(dl.canvas, Rgba::WHITE);
        assert_eq!(pixel(&cv, 0, 0), Rgba::WHITE);

        // body display:none: no box -> no propagation
        let (dl, _cv, _l) = painted(
            "<html><body style=\"display:none\"><div>x</div></body></html>",
            "body { background-color: red }",
        );
        assert_eq!(dl.canvas, Rgba::WHITE);

        // html transparent + body transparent: white
        let (dl, _cv, _l) = painted(
            "<html><body></body></html>",
            "html { background-color: transparent } body { background-color: transparent }",
        );
        assert_eq!(dl.canvas, Rgba::WHITE);
    }

    #[test]
    fn background_fill_and_order() {
        // boxes paint in document order; later boxes paint OVER earlier
        let (_dl, cv, _l) = painted(
            "<div id=\"a\"><div id=\"b\">x</div></div>",
            "#a { width: 100px; height: 100px; background-color: red }
             #b { width: 50px; height: 50px; background-color: blue }",
        );
        // #b paints over #a in the overlap (later command wins)
        assert_eq!(pixel(&cv, 10, 10), Rgba { r: 0, g: 0, b: 255, a: 255 });
        assert_eq!(pixel(&cv, 75, 75), Rgba { r: 255, g: 0, b: 0, a: 255 }); // #a only
        assert_eq!(pixel(&cv, 400, 400), Rgba::WHITE); // outside both
        assert_eq!(pixel(&cv, 25, 25), Rgba { r: 0, g: 0, b: 255, a: 255 });

        // alpha composites over the underlying box
        let (_dl, cv2, _l) = painted(
            "<div id=\"a\"><div id=\"b\">x</div></div>",
            "#a { width: 100px; height: 100px; background-color: rgb(0,0,255) }
             #b { width: 50px; height: 50px; background-color: rgba(255,0,0,0.5) }",
        );
        let p = pixel(&cv2, 25, 25);
        assert_eq!((p.r, p.g, p.b), (128, 0, 127)); // 50% red over blue
    }

    #[test]
    fn borders_solid_strips() {
        // 2px solid black border around a 50x40 red box
        let (_dl, cv, _l) = painted(
            "<div>x</div>",
            "div { width: 50px; height: 40px; background-color: red;
                   border-top-width: 2px; border-right-width: 2px;
                   border-bottom-width: 2px; border-left-width: 2px;
                   border-top-style: solid; border-right-style: solid;
                   border-bottom-style: solid; border-left-style: solid }",
        );
        // border box is 54x44; strips: top (0,0,54,2), bottom (0,42,54,2),
        // left (0,2,2,40), right (52,2,2,40)
        let black = Rgba::BLACK;
        assert_eq!(pixel(&cv, 0, 0), black); // top-left corner (top strip)
        assert_eq!(pixel(&cv, 25, 0), black); // top strip
        assert_eq!(pixel(&cv, 53, 20), black); // right strip
        assert_eq!(pixel(&cv, 25, 43), black); // bottom strip
        assert_eq!(pixel(&cv, 0, 20), black); // left strip
        assert_eq!(pixel(&cv, 25, 25), Rgba { r: 255, g: 0, b: 0, a: 255 }); // content bg
        assert_eq!(pixel(&cv, 25, 41), Rgba { r: 255, g: 0, b: 0, a: 255 }); // below content, above bottom strip

        // border color falls back to the element color, then black
        let (dl, _cv, _l) = painted(
            "<div>x</div>",
            "div { width: 10px; height: 10px; color: yellow;
                   border-top-width: 1px; border-top-style: solid }",
        );
        match dl.cmds[0] {
            Cmd::Fill { c, .. } => assert_eq!(c, Rgba { r: 255, g: 255, b: 0, a: 255 }),
            Cmd::Text { .. } => unreachable_text_cmd(),
        }
        let (dl2, _cv2, _l2) = painted(
            "<div>x</div>",
            "div { width: 10px; height: 10px; border-left-width: 1px; border-left-style: solid }",
        );
        match dl2.cmds[0] {
            Cmd::Fill { c, .. } => assert_eq!(c, Rgba::BLACK),
            Cmd::Text { .. } => unreachable_text_cmd(),
        }

        // unsupported border style: error + no border painted
        let (dl3, _cv3, _l3) = painted(
            "<div>x</div>",
            "div { width: 10px; height: 10px; border-top-width: 3px; border-top-style: dashed }",
        );
        assert_eq!(dl3.errors, 1);
        assert_eq!(dl3.cmds.len(), 1); // no fill paints, but 'x' still does

        // style none: width declared but no paint, no error
        let (dl4, _cv4, _l4) = painted(
            "<div>x</div>",
            "div { width: 10px; height: 10px; border-top-width: 3px }",
        );
        assert_eq!(dl4.errors, 0);
        assert_eq!(dl4.cmds.len(), 1); // 'x' Text command (border none)
    }

    #[test]
    fn clipping_at_raster() {
        // box partially off-canvas: dump shows ideal geometry, raster clips
        let (dl, cv, _l) = painted(
            "<div>x</div>",
            "div { width: 100px; height: 100px; background-color: lime; margin-left: -50px; margin-top: -50px }",
        );
        assert_eq!(dl.cmds.len(), 2); // fill + 'x' text
        match dl.cmds[0] {
            Cmd::Fill { x, y, .. } => {
                assert_eq!((x, y), (-50, -50)); // ideal geometry in the dump
            }
            Cmd::Text { .. } => unreachable_text_cmd(),
        }
        assert_eq!(pixel(&cv, 0, 0), Rgba { r: 0, g: 255, b: 0, a: 255 }); // clipped in
        assert_eq!(pixel(&cv, 49, 49), Rgba { r: 0, g: 255, b: 0, a: 255 });
        assert_eq!(pixel(&cv, 50, 50), Rgba::WHITE); // box ended at 50x50

        // fully off-canvas: no pixels touched
        let (_dl, cv2, _l2) = painted(
            "<div>x</div>",
            "div { width: 10px; height: 10px; background-color: lime; margin-left: 795px; margin-top: 595px }",
        );
        // box at x=795..805, y=595..605: only (795..800)x(595..600) is visible
        assert_eq!(pixel(&cv2, 796, 596), Rgba { r: 0, g: 255, b: 0, a: 255 });
        assert_eq!(pixel(&cv2, 794, 596), Rgba::WHITE);
        let (_dl3, cv3, _l3) = painted(
            "<div>x</div>",
            "div { width: 10px; height: 10px; background-color: lime; margin-left: 1000px; margin-top: 1000px }",
        );
        assert_eq!(pixel(&cv3, 799, 599), Rgba::WHITE);
    }

    #[test]
    fn frozen_paint_dump_and_ppm() {
        let (dl, cv, _l) = painted(
            "<div id=\"a\"><p>x</p></div>",
            "html { background-color: #112233 }
             #a { width: 100px; height: 60px; background-color: red;
                  border-left-width: 2px; border-left-style: solid }
             p { width: 10px; height: 5px; background-color: rgba(0, 255, 0, 0.5) }",
        );
        let d = dump(&dl);
        // boxes: html(0, propagated canvas), head hidden, body(2), div#a(3),
        // p(4). #a border box: x=0 y=0 w=2+100=102 h=60; left strip (0,0,2,60);
        // p sits at #a's content origin (x=0+2, y=0).
        // stage 9: 'x' paints at p's content origin (x=2), baseline
        // y=15 (ascent of 16px), black on the red box; WCAG 525 (5.25:1),
        // APCA 3995 = Lc 39.95 — honest measurement (this pair violates
        // the Lc 75 norm, 3995 < 7500); the frozen value is what the
        // verified apca_lc_x100 emits, not a hand-rounded number
        let expected = "PAINT v2\n\
                        CANVAS #112233\n\
                        F 0 0 102 60 #ff0000\n\
                        F 0 0 2 60 #000000\n\
                        F 2 0 10 5 #00ff0080\n\
                        T 2 15 #000000 #ff0000 525 3995 x\n";
        assert_eq!(d, expected);
        assert_eq!(dl.text_cmds, 1);
        assert_eq!(dl.text_chars, 1);
        assert_eq!(dl.wcag_min_x100, 525);
        assert_eq!(dl.apca_min_x100, 3995);
        assert_eq!(dl.contrast_violations, 0);
        assert_eq!(dl.apca_violations, 1);

        // PPM: exact header, exact size, deterministic bytes
        let ppm = to_ppm(&cv);
        assert_eq!(&ppm[..15], b"P6\n800 600\n255\n");
        assert_eq!(ppm.len(), 15 + 800 * 600 * 3);
        // canvas color shows at a pixel no command covers: (500,500)
        let at = |x: usize, y: usize| &ppm[15 + (y * 800 + x) * 3..15 + (y * 800 + x) * 3 + 3];
        assert_eq!(at(500, 500), &[0x11, 0x22, 0x33]);
        assert_eq!(at(0, 0), &[0x00, 0x00, 0x00]); // left border strip of #a
        let digest = crate::sha256::sha256(&ppm);
        let hex = crate::sha256::hex(&digest);
        // frozen: same input -> same bytes -> same digest (regression guard)
        let (dl2, cv2, _l2) = painted(
            "<div id=\"a\"><p>x</p></div>",
            "html { background-color: #112233 }
             #a { width: 100px; height: 60px; background-color: red;
                  border-left-width: 2px; border-left-style: solid }
             p { width: 10px; height: 5px; background-color: rgba(0, 255, 0, 0.5) }",
        );
        assert_eq!(hex, crate::sha256::hex(&crate::sha256::sha256(&to_ppm(&cv2))));
        assert_eq!(dl, dl2);
    }

    #[test]
    fn apca_both_polarities_positive() {
        // the WoB branch must subtract background from text: light-on-dark
        // stays positive (white/black = 10788) and keeps the documented
        // asymmetry (slightly above black-on-white 10604). Regression guard:
        // the swapped expression silently reported Lc 0 for every
        // light-on-dark pair, which starved HEALTH_APCA_VIOLATIONS of truth.
        let wb = apca_lc_x100(Rgba::WHITE, Rgba::BLACK);
        let bw = apca_lc_x100(Rgba::BLACK, Rgba::WHITE);
        assert_eq!(wb, 10788);
        assert_eq!(bw, 10604);
        assert!(wb > bw);
        // a dim gray on white is honestly below the Lc 75 floor
        let dim = apca_lc_x100(Rgba { r: 204, g: 204, b: 204, a: 255 }, Rgba::WHITE);
        assert_eq!(dim, 2729);
    }

    #[test]
    fn deep_nesting_10000_paint_no_overflow() {
        // iterative display-list build: 10k nested divs cannot overflow
        let mut html = String::new();
        for _ in 0..10000 {
            html.push_str("<div>");
        }
        html.push('x');
        for _ in 0..10000 {
            html.push_str("</div>");
        }
        let doc = dom::build(&html);
        let o = css::parse("div { background-color: red; height: 1px }");
        let ap = css::apply(&doc.dom, &o.rules);
        let lo = layout::run(&doc.dom, &ap);
        let dl = display_list(&lo, &ap);
        // html/body have no background -> no fills; each of the 10k divs
        // fills; the innermost 'x' adds one Text command
        assert_eq!(dl.cmds.len(), 10001);
        let cv = raster(&dl);
        assert_eq!(pixel(&cv, 0, 0), Rgba { r: 255, g: 0, b: 0, a: 255 });
    }
}
