//! neutral-browser — stage 9: block layout + line layout (LAYOUT).
//!
//! Chain: URL -> Broker -> TCP -> HTTP/1.1 -> body -> HTML -> DOM -> CSS
//!        -> STYLE -> LAYOUT.
//!
//! This module is pure: no I/O, no panics, no recursion (all tree passes
//! are iterative, deep documents cannot overflow the stack). It consumes
//! the DOM and the computed STYLE (css::StyleApply) and produces a
//! block-flow box tree per CSS 2.1 chapter 10 (a documented subset).
//!
//! Implemented:
//!   * block-level flow: children stack vertically inside the parent's
//!     content box; the root's containing block is the initial containing
//!     block (ICB) with the frozen definite size VIEWPORT_W x VIEWPORT_H;
//!   * LINE LAYOUT (stage 9): text nodes and inline elements participate in
//!     inline flow; contiguous inline content of a block is collected into
//!     an anonymous lines box (CSS 2.1 §9.2.1.1 subset) and broken into
//!     line boxes with word wrap at separators; embedded bitmap font
//!     metrics (font_data, DejaVu Sans Mono) measure advances;
//!   * HUMAN_FIRST health norms measured from day one: the UA line-height
//!     default is 1.5 (not CSS `normal`); a used line height below 1.5 of
//!     the max font size on the line counts HEALTH_LINE_HEIGHT_VIOLATIONS;
//!     a line over 75 characters counts HEALTH_LINE_LENGTH_VIOLATIONS
//!     (Baymard 50–75); glyphs missing from the embedded font are counted
//!     (LAYOUT_MISSING_GLYPHS), never silently dropped;
//!   * width resolution per CSS 2.1 §10.3.3 (non-replaced block): definite
//!     px/% width, auto width fills the remaining space, over-constrained
//!     equations resolve in LTR (margin-right absorbs the remainder), both
//!     margins auto center the box, width auto makes any auto margins zero;
//!   * height per §10.6.3: definite px height, percentage height resolves
//!     only against a definite containing-block height (§10.5 — otherwise
//!     auto), auto height = sum of the in-flow children's margin-box
//!     heights;
//!   * margins/paddings/border-widths: px, % (resolved against the
//!     containing block WIDTH — including vertical ones, per spec), auto
//!     (margins only);
//!   * box-sizing: content-box (default) / border-box;
//!   * display: none removes the box and its subtree from the flow.
//!
//! Documented subset deviations (each a deliberate, frozen decision):
//!   * inline elements carry no own box: their text joins the nearest
//!     block ancestor's inline flow with their own element ordinal as the
//!     style owner (per-run color/font-size still resolve per element);
//!     block-in-inline SPLITTING of a block child inside inline runs is
//!     NOT_IMPLEMENTED (the block child just closes the segment);
//!   * `br` keeps its frozen v0.x treatment (counted in LAYOUT_HIDDEN)
//!     and additionally acts as a line-break marker in the inline flow;
//!     a line break on an empty line emits a strut line box;
//!   * replaced elements (img) contribute no text and no box;
//!   * font-size parses px only (other units/values are counted errors,
//!     default 16); line-height parses px/%/unitless, `normal` maps to
//!     the HUMAN_FIRST default 1.5; glyph scaling is integer round-half-up;
//!   * whitespace: ASCII space/tab/CR/LF/FF collapse to single separators;
//!     leading separators at line start and trailing separators at wrap
//!     are dropped; NBSP (U+00A0) is a word glyph, never a separator;
//!   * a frozen UA-subset table hides non-rendered elements (head, title,
//!     meta, link, style, script, ...): display:none by default;
//!   * no margin collapsing: vertical margins simply add up;
//!   * negative definite widths/heights clamp to 0 (CSS: invalid value);
//!     paddings and border-widths clamp to >= 0 (CSS forbids negatives);
//!   * shorthand/longhand conflicts (margin vs margin-left) resolve in
//!     the alphabetical property order the STYLE dump carries — the
//!     longhand always wins, regardless of source order (subset);
//!   * layout reads a frozen property set; every other declaration
//!     (color, font-*, border shorthand, ...) is ignored here;
//!     values present but unsupported (display:flex, width:red, ...) are
//!     counted in LAYOUT_ERRORS and fall back to the property default;
//!   * no float/position/flex/grid/table/min-max/transforms
//!     (NOT_IMPLEMENTED, see README).

use crate::css::StyleApply;
use crate::dom::{Dom, NodeData};
use crate::font_data;

/// Initial containing block (viewport) of this engine — frozen constants.
pub const VIEWPORT_W: i64 = 800;
pub const VIEWPORT_H: i64 = 600;

/// Parse clamp: |length| <= 10^12 keeps every downstream i64 computation
/// far from overflow (sums stay linear in depth even for 10^4 nesting).
const LEN_LIMIT: i64 = 1_000_000_000_000;

const ROOT: usize = usize::MAX; // parent id of top-level boxes (the ICB)

// ---------------- display ----------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disp {
    None,
    Block,
    Inline,
}

/// UA-subset: elements that never render. `br` is here frozen from v0.x
/// (its own non-box counts in LAYOUT_HIDDEN); in the inline flow it still
/// acts as a line-break marker (see phase A).
const DEFAULT_NONE: &[&str] = &[
    "head", "title", "meta", "link", "style", "script", "noscript", "template",
    "base", "param", "source", "track", "datalist", "br", "area", "option",
];

/// UA-subset: inline-by-default element names. Everything else is a block.
const DEFAULT_INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "big", "button", "cite", "code", "data",
    "dfn", "em", "i", "img", "input", "kbd", "label", "mark", "nobr",
    "output", "q", "s", "samp", "select", "small", "span", "strong", "sub",
    "sup", "textarea", "time", "tt", "u", "var", "wbr",
];

fn default_display(name: &str) -> Disp {
    if DEFAULT_NONE.iter().any(|n| *n == name) {
        return Disp::None;
    }
    if DEFAULT_INLINE.iter().any(|n| *n == name) {
        return Disp::Inline;
    }
    Disp::Block
}

// ---------------- lengths ----------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Len {
    Px(i64),
    Pct(i64),
    Auto,
}

/// Number part of a length: f64 parse, round half away from zero, clamp
/// to +-LEN_LIMIT. Deterministic (IEEE 754, no locale).
fn parse_num(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let f: f64 = s.parse().ok()?;
    if !f.is_finite() {
        return None;
    }
    let r = f.round();
    if r >= LEN_LIMIT as f64 {
        return Some(LEN_LIMIT);
    }
    if r <= -(LEN_LIMIT as f64) {
        return Some(-LEN_LIMIT);
    }
    Some(r as i64)
}

/// `auto` | `<n>px` | `<n>%` | unitless zero. Unitless non-zero is NOT a
/// length in CSS and is rejected.
pub fn parse_len(raw: &str) -> Option<Len> {
    let s = raw.trim();
    if s.eq_ignore_ascii_case("auto") {
        return Some(Len::Auto);
    }
    if s == "0" {
        return Some(Len::Px(0));
    }
    if let Some(num) = s.strip_suffix('%') {
        return parse_num(num).map(Len::Pct);
    }
    if let Some(num) = s.strip_suffix("px") {
        return parse_num(num).map(Len::Px);
    }
    None
}

fn pct_of(pc: i64, base: i64) -> i64 {
    // i128 intermediate: LEN_LIMIT^2 = 10^24 fits far below i128 max.
    let v = (pc as i128 * base as i128) / 100; // trunc toward zero
    v.clamp(-(LEN_LIMIT as i128), LEN_LIMIT as i128) as i64
}

fn resolve_len(l: Len, base: i64) -> i64 {
    match l {
        Len::Px(v) => v,
        Len::Pct(pc) => pct_of(pc, base),
        Len::Auto => 0,
    }
}

// ---------------- text metrics (stage 9) ----------------

/// line-height value: Def = the HUMAN_FIRST UA default (1.5, a deliberate
/// health norm stricter than CSS `normal`); Mult = unitless multiplier;
/// Px/Pct = definite lengths (% resolves against the font size).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LH {
    Def,
    Mult(f64),
    Px(i64),
    Pct(i64),
}

/// Used line height in px (deterministic: f64 only for the multiplier,
/// IEEE 754 round half away from zero; integers otherwise).
pub fn line_h_px(lh: LH, fs: i64) -> i64 {
    match lh {
        LH::Def => (LH_DEFAULT * fs as f64).round() as i64,
        LH::Mult(m) => (m * fs as f64).round() as i64,
        LH::Px(v) => v.max(0),
        LH::Pct(p) => pct_of(p, fs).max(0),
    }
}

/// Font-size clamp: 0..=512 px. Zero is a legal CSS value (hidden text);
/// the clamp keeps every downstream integer scale far from overflow.
pub const FS_MAX: i64 = 512;

// HUMAN_FIRST health norms (docs/HUMAN_FIRST.md) — frozen as named
// constants so the architecture gate can pin NAME + VALUE like LIMITS:
pub const LH_DEFAULT: f64 = 1.5;
pub const MEASURE_MAX_CHARS: usize = 75;

/// Advance width of one codepoint at font size `fs` (px, round half up).
/// A codepoint missing from the embedded font measures as a space and is
/// reported by the caller (honest missing-glyph counting).
fn adv_cp(cp: u32, fs: i64) -> i64 {
    match font_data::lookup(cp) {
        Some(g) => font_data::advance_of(g, fs),
        None => font_data::lookup(0x20)
            .map(|g| font_data::advance_of(g, fs))
            .unwrap_or(0),
    }
}

fn is_sep(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0c}')
}

// ---------------- property subset ----------------

#[derive(Debug)]
struct Props {
    disp: Disp,
    border_box: bool,
    width: Len,
    height: Len,
    mt: Len,
    mr: Len,
    mb: Len,
    ml: Len,
    pt: Len,
    pr: Len,
    pb: Len,
    pl: Len,
    bt: Len,
    br: Len,
    bb: Len,
    bl: Len,
    font_size: i64,
    line_height: LH,
    errors: usize,
}

impl Props {
    fn new(name: &str) -> Self {
        Props {
            disp: default_display(name),
            border_box: false,
            width: Len::Auto,
            height: Len::Auto,
            // no UA stylesheet exists in this engine: default margin 0
            mt: Len::Px(0),
            mr: Len::Px(0),
            mb: Len::Px(0),
            ml: Len::Px(0),
            pt: Len::Px(0),
            pr: Len::Px(0),
            pb: Len::Px(0),
            pl: Len::Px(0),
            bt: Len::Px(0),
            br: Len::Px(0),
            bb: Len::Px(0),
            bl: Len::Px(0),
            font_size: 16,
            line_height: LH::Def,
            errors: 0,
        }
    }
}

/// 1-4 component box shorthand (margin/padding/border-width families).
/// `allow_auto`: margins accept `auto`, paddings/borders do not (CSS:
/// padding/border-width never take auto — such a value is invalid).
fn parse_box4(value: &str, allow_auto: bool) -> Option<[Len; 4]> {
    let parts: Vec<&str> = value.split_ascii_whitespace().collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut ls = [Len::Px(0); 4];
    for (i, part) in parts.iter().enumerate() {
        match parse_len(part) {
            Some(Len::Auto) if !allow_auto => return None,
            Some(l) => ls[i] = l,
            None => return None,
        }
    }
    Some(match parts.len() {
        1 => [ls[0], ls[0], ls[0], ls[0]],
        2 => [ls[0], ls[1], ls[0], ls[1]],
        3 => [ls[0], ls[1], ls[2], ls[1]],
        _ => [ls[0], ls[1], ls[2], ls[3]],
    })
}

/// Parse the layout property subset from one element's cascaded winners.
/// Unknown properties are ignored (not errors); unsupported VALUES of
/// known properties count one error each and fall back to the default.
fn parse_props(name: &str, decls: &[(String, String)]) -> Props {
    let mut p = Props::new(name);
    for (k, v) in decls {
        match k.as_str() {
            "display" => {
                let t = v.trim();
                if t.eq_ignore_ascii_case("none") {
                    p.disp = Disp::None;
                } else if t.eq_ignore_ascii_case("block") {
                    p.disp = Disp::Block;
                } else if t.eq_ignore_ascii_case("inline")
                    || t.eq_ignore_ascii_case("inline-block")
                    || t.eq_ignore_ascii_case("list-item")
                {
                    // documented deviation: inline-level lays out as block
                    p.disp = Disp::Block;
                } else {
                    p.errors += 1; // flex/grid/table/... unsupported
                }
            }
            "box-sizing" => {
                let t = v.trim();
                if t.eq_ignore_ascii_case("border-box") {
                    p.border_box = true;
                } else if t.eq_ignore_ascii_case("content-box") {
                    p.border_box = false;
                } else {
                    p.errors += 1;
                }
            }
            "width" => match parse_len(v) {
                Some(l) => p.width = l,
                None => p.errors += 1,
            },
            "height" => match parse_len(v) {
                Some(l) => p.height = l,
                None => p.errors += 1,
            },
            "margin" => match parse_box4(v, true) {
                Some([t, r, b, l]) => {
                    p.mt = t;
                    p.mr = r;
                    p.mb = b;
                    p.ml = l;
                }
                None => p.errors += 1,
            },
            "margin-top" => match parse_len(v) {
                Some(l) => p.mt = l,
                None => p.errors += 1,
            },
            "margin-right" => match parse_len(v) {
                Some(l) => p.mr = l,
                None => p.errors += 1,
            },
            "margin-bottom" => match parse_len(v) {
                Some(l) => p.mb = l,
                None => p.errors += 1,
            },
            "margin-left" => match parse_len(v) {
                Some(l) => p.ml = l,
                None => p.errors += 1,
            },
            "padding" => match parse_box4(v, false) {
                Some([t, r, b, l]) => {
                    p.pt = t;
                    p.pr = r;
                    p.pb = b;
                    p.pl = l;
                }
                None => p.errors += 1,
            },
            "padding-top" => match parse_len(v) {
                Some(Len::Auto) => p.errors += 1,
                Some(l) => p.pt = l,
                None => p.errors += 1,
            },
            "padding-right" => match parse_len(v) {
                Some(Len::Auto) => p.errors += 1,
                Some(l) => p.pr = l,
                None => p.errors += 1,
            },
            "padding-bottom" => match parse_len(v) {
                Some(Len::Auto) => p.errors += 1,
                Some(l) => p.pb = l,
                None => p.errors += 1,
            },
            "padding-left" => match parse_len(v) {
                Some(Len::Auto) => p.errors += 1,
                Some(l) => p.pl = l,
                None => p.errors += 1,
            },
            "border-width" => match parse_box4(v, false) {
                Some([t, r, b, l]) => {
                    p.bt = t;
                    p.br = r;
                    p.bb = b;
                    p.bl = l;
                }
                None => p.errors += 1,
            },
            "border-top-width" => match parse_len(v) {
                Some(Len::Auto) => p.errors += 1,
                Some(l) => p.bt = l,
                None => p.errors += 1,
            },
            "border-right-width" => match parse_len(v) {
                Some(Len::Auto) => p.errors += 1,
                Some(l) => p.br = l,
                None => p.errors += 1,
            },
            "border-bottom-width" => match parse_len(v) {
                Some(Len::Auto) => p.errors += 1,
                Some(l) => p.bb = l,
                None => p.errors += 1,
            },
            "border-left-width" => match parse_len(v) {
                Some(Len::Auto) => p.errors += 1,
                Some(l) => p.bl = l,
                None => p.errors += 1,
            },
            "font-size" => {
                // px subset: other units/values are errors (default 16)
                match v.trim().strip_suffix("px").and_then(parse_num) {
                    Some(n) if n >= 0 => p.font_size = n.min(FS_MAX),
                    _ => p.errors += 1,
                }
            }
            "line-height" => {
                let t = v.trim();
                if t.eq_ignore_ascii_case("normal") {
                    p.line_height = LH::Def;
                } else if let Some(n) = t.strip_suffix("px").and_then(parse_num) {
                    p.line_height = LH::Px(n);
                } else if let Some(n) = t.strip_suffix('%').and_then(parse_num) {
                    p.line_height = LH::Pct(n);
                } else {
                    // unitless multiplier (>= 0)
                    match t.parse::<f64>() {
                        Ok(m) if m.is_finite() && m >= 0.0 => p.line_height = LH::Mult(m),
                        _ => p.errors += 1,
                    }
                }
            }
            // color/font/border-style/border-color/outline/... are not
            // layout properties in this subset: ignored, not errors
            _ => {}
        }
    }
    p
}

// ---------------- box arena ----------------

/// Kind of an arena box: a real element box, or an anonymous lines box
/// holding the inline flow (CSS 2.1 §9.2.1.1 subset) of one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxKind {
    Block,
    Lines,
}

/// One item of an inline flow: a text run owned by an element ordinal
/// (for per-run style lookup), or a `br` line-break marker.
#[derive(Debug, Clone)]
pub enum SegItem {
    Run { elem: usize, text: String },
    Break,
}

/// One laid-out word inside a line box.
#[derive(Debug, Clone)]
pub struct WordOut {
    pub x: i64,      // offset from the lines box content left edge
    pub elem: usize, // style owner (color/font-size resolve here)
    pub text: String,
    pub fs: i64, // font size used for measurement (px)
}

/// One line box: geometry relative to the lines box content box.
#[derive(Debug, Clone)]
pub struct Line {
    pub y: i64,       // offset from the lines box content top edge
    pub h: i64,       // used line height (max of member runs)
    pub baseline: i64, // baseline offset from the line top
    pub chars: usize, // characters including collapsed separators
    pub w: i64,       // ink width (words + separators)
    pub words: Vec<WordOut>,
}

#[derive(Debug)]
pub struct LBox {
    pub index: usize,  // document-order element ordinal (matches STYLE);
                       // usize::MAX for anonymous lines boxes
    pub kind: BoxKind,
    pub path: String,  // element names from the root, '>'-joined;
                       // lines boxes carry a "::lines" suffix
    pub parent: usize, // box arena id, ROOT for top-level boxes
    pub children: Vec<usize>,
    pub items: Vec<SegItem>, // inline content (Lines boxes only)
    pub lines: Vec<Line>,    // laid out lines (Lines boxes only)
    pub cb_w: i64, // containing-block content width used for % resolution
    pub x: i64,    // border-box top-left in page coordinates
    pub y: i64,
    pub ml: i64,
    pub mr: i64,
    pub mt: i64,
    pub mb: i64,
    pub pl: i64,
    pub pr: i64,
    pub pt: i64,
    pub pb: i64,
    pub bl: i64,
    pub br: i64,
    pub bt: i64,
    pub bb: i64,
    pub content_w: i64,
    pub content_h: i64,
    pub height_definite: bool, // explicit/%-of-definite height (for children %)
}

impl LBox {
    /// Border-box width: border + padding + content (saturating).
    pub fn w(&self) -> i64 {
        self.bl
            .saturating_add(self.pl)
            .saturating_add(self.content_w)
            .saturating_add(self.pr)
            .saturating_add(self.br)
    }

    /// Border-box height: border + padding + content (saturating).
    pub fn border_h(&self) -> i64 {
        self.bt
            .saturating_add(self.pt)
            .saturating_add(self.content_h)
            .saturating_add(self.pb)
            .saturating_add(self.bb)
    }

    /// Margin-box height: what the next sibling stacks on (saturating).
    fn margin_h(&self) -> i64 {
        self.mt
            .saturating_add(self.border_h())
            .saturating_add(self.mb)
    }
}

#[derive(Debug, Default)]
pub struct LayoutOut {
    pub boxes: Vec<LBox>, // document order (display:none subtrees absent)
    pub errors: usize,    // unsupported values of known layout properties
    pub hidden: usize,    // elements whose own display:none removed their box
    // ---- stage 9: line layout counters ----
    pub lines_total: usize,   // line boxes produced
    pub text_chars: usize,    // characters fed into lines (post-collapse)
    pub breaks: usize,        // `br` markers acted upon
    pub missing_glyphs: usize, // occurrences of codepoints without a glyph
    pub lh_violations: usize, // lines below the 1.5 HUMAN_FIRST norm
    pub len_violations: usize, // lines over 75 characters (Baymard)
    pub line_chars_max: usize, // longest line, characters
}

/// Own cascaded winners of the element with document-order `index`.
/// `ap.elements` is pushed in document order by css::apply, so binary
/// search is safe. Elements with no own values are absent -> empty.
fn lookup_decls<'a>(ap: &'a StyleApply, index: usize) -> &'a [(String, String)] {
    match ap.elements.binary_search_by_key(&index, |e| e.index) {
        Ok(pos) => &ap.elements[pos].decls,
        Err(_) => &[],
    }
}

/// Drain `items` into a new anonymous lines box under `parent_bid`
/// (CSS 2.1 §9.2.1.1 subset). Whitespace-only segments without a break
/// produce no box. Returns the new arena id, or None (no box emitted).
fn flush_lines(
    out: &mut LayoutOut,
    props_of: &mut Vec<Props>,
    parent_bid: usize,
    items: Vec<SegItem>,
) -> Option<usize> {
    let meaningful = items.iter().any(|it| match it {
        SegItem::Break => true,
        SegItem::Run { text, .. } => text.chars().any(|c| !is_sep(c)),
    });
    if !meaningful {
        return None;
    }
    let bid = out.boxes.len();
    let path = format!("{}::lines", out.boxes[parent_bid].path);
    out.boxes.push(LBox {
        index: usize::MAX,
        kind: BoxKind::Lines,
        path,
        parent: parent_bid,
        children: Vec::new(),
        items,
        lines: Vec::new(),
        cb_w: 0,
        x: 0,
        y: 0,
        ml: 0,
        mr: 0,
        mt: 0,
        mb: 0,
        pl: 0,
        pr: 0,
        pt: 0,
        pb: 0,
        bl: 0,
        br: 0,
        bt: 0,
        bb: 0,
        content_w: 0,
        content_h: 0,
        height_definite: false,
    });
    props_of.push(Props::new("::lines"));
    out.boxes[parent_bid].children.push(bid);
    Some(bid)
}

/// Text-relevant computed values of one element (stage 9 subset).
#[derive(Debug, Clone, Copy)]
struct TextProps {
    fs: i64,
    lh: LH,
}

/// TextProps of an element by ordinal (cached; None entry = not parsed yet).
/// font-size/line-height INHERIT per CSS: the merge of the element's own
/// cascaded winners with the `inherited` values css::apply propagated from
/// ancestors (every element defaults to fs 16 / line-height 1.5 — the
/// HUMAN_FIRST UA default, not CSS `normal`).
fn text_props_of(ap: &StyleApply, idx: usize, cache: &mut [Option<TextProps>]) -> TextProps {
    if idx >= cache.len() {
        return TextProps { fs: 16, lh: LH::Def };
    }
    if let Some(tp) = cache[idx] {
        return tp;
    }
    // stage 9: font-size/line-height are inherited properties (css.rs
    // is_inherited); css::apply stores ancestor values in a SEPARATE field
    // `inherited`, so merge it here — the run/strut must see the COMPUTED
    // value: own winners last (they override), inherited first. By
    // construction the two maps are disjoint (apply filters own keys out
    // of inherited), the concat order is only a safety net.
    let mut merged: Vec<(String, String)> = Vec::new();
    if let Ok(pos) = ap.elements.binary_search_by_key(&idx, |e| e.index) {
        let e = &ap.elements[pos];
        merged.extend(e.inherited.iter().cloned());
        merged.extend(e.decls.iter().cloned());
    }
    let p = parse_props("", &merged);
    let tp = TextProps {
        fs: p.font_size,
        lh: p.line_height,
    };
    cache[idx] = Some(tp);
    tp
}

/// Lay out the inline content of lines box `bid` into line boxes:
/// collapse whitespace, split into words, wrap at separators, honor `br`
/// markers, measure with the embedded font. Fills `lines`, `content_h`,
/// and the health/norm counters of the LayoutOut.
///
/// Two passes: (1) items -> flat token list (Word/Sep/Break), so a
/// separator at the edge of two runs (e.g. "one " + "two") is never lost;
/// (2) greedy wrap over tokens. Sep carries the font size of the run that
/// owned the whitespace (its width follows that run's space advance).
fn layout_lines(
    out: &mut LayoutOut,
    bid: usize,
    ap: &StyleApply,
    cache: &mut [Option<TextProps>],
) {
    let avail = out.boxes[bid].content_w;
    // owner of empty/strut lines = the parent block's element
    let owner_idx = out.boxes[out.boxes[bid].parent].index;
    let owner_tp = text_props_of(ap, owner_idx.min(usize::MAX - 1), cache);

    enum Tok {
        W { elem: usize, text: String, w: i64, fs: i64, lh_h: i64 },
        S { w: i64 },
        B,
    }

    let items = std::mem::take(&mut out.boxes[bid].items);
    let mut toks: Vec<Tok> = Vec::new();
    for item in items.into_iter() {
        match item {
            SegItem::Break => toks.push(Tok::B),
            SegItem::Run { elem, text } => {
                let tp = text_props_of(ap, elem, cache);
                let fs = tp.fs;
                let lh_h = line_h_px(tp.lh, fs);
                let mut word = String::new();
                let mut ww: i64 = 0;
                for ch in text.chars() {
                    if is_sep(ch) {
                        if !word.is_empty() {
                            toks.push(Tok::W { elem, text: std::mem::take(&mut word), w: ww, fs, lh_h });
                            ww = 0;
                        }
                        toks.push(Tok::S { w: adv_cp(0x20, fs) });
                    } else {
                        let cp = ch as u32;
                        if font_data::lookup(cp).is_none() {
                            out.missing_glyphs += 1;
                        }
                        ww += adv_cp(cp, fs);
                        word.push(ch);
                    }
                }
                if !word.is_empty() {
                    toks.push(Tok::W { elem, text: word, w: ww, fs, lh_h });
                }
            }
        }
    }

    struct Cur {
        words: Vec<WordOut>, w: i64, chars: usize, max_fs: i64, max_h: i64, max_asc: i64,
    }
    let mut cur = Cur {
        words: Vec::new(), w: 0, chars: 0, max_fs: 0, max_h: 0, max_asc: 0,
    };
    let mut lines: Vec<Line> = Vec::new();
    let mut cursor: i64 = 0;
    let mut pending_sep: Option<i64> = None;

    macro_rules! flush {
        ($forced:expr) => { {
            let forced = $forced;
            if !cur.words.is_empty() || forced {
                let (h, asc, mfs) = if cur.words.is_empty() {
                    (
                        line_h_px(owner_tp.lh, owner_tp.fs),
                        font_data::ascent_px(owner_tp.fs),
                        owner_tp.fs,
                    )
                } else {
                    (cur.max_h, cur.max_asc, cur.max_fs)
                };
                // HUMAN_FIRST norms, measured per line box
                // (h*2 < mfs*3 is the integer form of h < LH_DEFAULT * mfs)
                if h * 2 < mfs * 3 {
                    out.lh_violations += 1;
                }
                if cur.chars > MEASURE_MAX_CHARS {
                    out.len_violations += 1;
                }
                out.line_chars_max = out.line_chars_max.max(cur.chars);
                out.lines_total += 1;
                out.text_chars += cur.chars;
                lines.push(Line {
                    y: cursor,
                    h,
                    baseline: asc,
                    chars: cur.chars,
                    w: cur.w,
                    words: std::mem::take(&mut cur.words),
                });
                cursor += h;
                cur.w = 0;
                cur.chars = 0;
                cur.max_fs = 0;
                cur.max_h = 0;
                cur.max_asc = 0;
            }
        } };
    }

    for tok in toks.into_iter() {
        match tok {
            Tok::B => {
                // a break ends the current line; on an empty line it
                // emits a strut (CSS: each br owns one line box)
                flush!(true);
                pending_sep = None;
            }
            Tok::S { w } => {
                // collapse: a single pending separator between words
                pending_sep = Some(w);
            }
            Tok::W { elem, text, w, fs, lh_h } => {
                let wchars = text.chars().count();
                let sep_w = pending_sep.take().unwrap_or(0);
                let had = !cur.words.is_empty();
                let fits = avail <= 0 || cur.w + sep_w + w <= avail;
                if had && !fits {
                    flush!(false); // wrap: the separator is dropped
                    cur.words.push(WordOut { x: 0, elem, text, fs });
                    cur.w = w;
                } else {
                    let gap = if had { sep_w } else { 0 };
                    cur.words.push(WordOut { x: cur.w + gap, elem, text, fs });
                    cur.w += gap + w;
                    if had {
                        cur.chars += 1; // the collapsed separator
                    }
                }
                cur.chars += wchars;
                cur.max_fs = cur.max_fs.max(fs);
                cur.max_h = cur.max_h.max(lh_h);
                cur.max_asc = cur.max_asc.max(font_data::ascent_px(fs));
            }
        }
    }
    flush!(false); // segment end: no trailing phantom strut

    let b = &mut out.boxes[bid];
    b.lines = lines;
    b.content_h = cursor;
}

/// Run the block layout over the DOM with the computed STYLE.
/// Iterative in every pass: build + segment collection (pre-order walk),
/// widths (parents first), lines (flat), heights (children first),
/// positions (parents first).
pub fn run(dom: &Dom, ap: &StyleApply) -> LayoutOut {
    let mut out = LayoutOut::default();
    let mut props_of: Vec<Props> = Vec::new(); // parallel to out.boxes

    // ---- phase A: build the box arena + collect inline segments ----
    let mut path: Vec<String> = Vec::new();
    let mut open: Vec<usize> = Vec::new(); // arena ids of open boxes
    // inline content accumulated per open block (parallel to `open`;
    // entry 0 is the ICB: top-level text outside any block is dropped)
    let mut pending: Vec<Vec<SegItem>> = vec![Vec::new()];
    let mut actions: Vec<u8> = Vec::new();
    // per open element: 0 suppressed, 1 hidden-root, 2 block box,
    // 3 inline (no box), 4 br (hidden + break marker)
    let mut style_owner: Vec<usize> = Vec::new(); // element ordinals
    let mut hidden_depth = 0usize;
    let mut elem_index = 0usize;
    let mut stack: Vec<(usize, bool)> = dom.nodes[0]
        .children
        .iter()
        .rev()
        .map(|&c| (c, true))
        .collect();

    while let Some((id, entering)) = stack.pop() {
        if entering {
            match &dom.nodes[id].data {
                NodeData::Text(text) => {
                    // text joins the inline flow of the nearest open block,
                    // owned by the nearest entered element
                    if hidden_depth == 0 {
                        if let Some(items) = pending.last_mut() {
                            if let Some(&owner) = style_owner.last() {
                                items.push(SegItem::Run {
                                    elem: owner,
                                    text: text.clone(),
                                });
                            }
                        }
                    }
                    continue; // text nodes have no children
                }
                NodeData::Element { .. } => {
                    // handled below (name extracted without unreachable!)
                }
                _ => {
                    // comments/doctype: traverse, no bookkeeping
                    for &c in dom.nodes[id].children.iter().rev() {
                        stack.push((c, false));
                        stack.push((c, true));
                    }
                    continue;
                }
            }
            let name = match &dom.nodes[id].data {
                NodeData::Element { name, .. } => name,
                _ => {
                    // not an element after all: nothing to do
                    continue;
                }
            };
            let idx = elem_index;
            elem_index += 1;
            path.push(name.clone());
            if hidden_depth > 0 {
                // inside a display:none subtree: no box, no runs, no counting
                actions.push(0);
            } else if name == "br" {
                // frozen v0.x: br counts as hidden (no box); stage 9: it
                // also acts as a line-break marker in the inline flow
                out.hidden += 1;
                hidden_depth += 1;
                if let Some(items) = pending.last_mut() {
                    items.push(SegItem::Break);
                }
                out.breaks += 1;
                actions.push(4);
            } else {
                let decls = lookup_decls(ap, idx);
                let props = parse_props(name, decls);
                out.errors += props.errors;
                match props.disp {
                    Disp::None => {
                        out.hidden += 1;
                        hidden_depth += 1;
                        actions.push(1);
                    }
                    Disp::Inline => {
                        // no own box: the text flows into the nearest
                        // ancestor block's segment with this ordinal as
                        // the style owner (stage 9 deviation)
                        style_owner.push(idx);
                        actions.push(3);
                    }
                    Disp::Block => {
                        // close the parent's pending segment first: the
                        // anonymous lines box precedes this child box
                        if let Some(&p) = open.last() {
                            if let Some(slot) = pending.last_mut() {
                                let items = std::mem::take(slot);
                                flush_lines(&mut out, &mut props_of, p, items);
                            }
                        }
                        let parent = open.last().copied().unwrap_or(ROOT);
                        let bid = out.boxes.len();
                        out.boxes.push(LBox {
                            index: idx,
                            kind: BoxKind::Block,
                            path: path.join(">"),
                            parent,
                            children: Vec::new(),
                            items: Vec::new(),
                            lines: Vec::new(),
                            cb_w: 0,
                            x: 0,
                            y: 0,
                            ml: 0,
                            mr: 0,
                            mt: 0,
                            mb: 0,
                            pl: 0,
                            pr: 0,
                            pt: 0,
                            pb: 0,
                            bl: 0,
                            br: 0,
                            bt: 0,
                            bb: 0,
                            content_w: 0,
                            content_h: 0,
                            height_definite: false,
                        });
                        props_of.push(props);
                        if let Some(&p) = open.last() {
                            out.boxes[p].children.push(bid);
                        }
                        open.push(bid);
                        pending.push(Vec::new());
                        style_owner.push(idx);
                        actions.push(2);
                    }
                }
            }
            for &c in dom.nodes[id].children.iter().rev() {
                stack.push((c, false));
                stack.push((c, true));
            }
        } else if matches!(dom.nodes[id].data, NodeData::Element { .. }) {
            path.pop();
            match actions.pop() {
                Some(1) | Some(4) => hidden_depth -= 1,
                Some(2) => {
                    // flush the block's own trailing segment (after its
                    // last block child, keeping document order)
                    if let (Some(&bid), Some(slot)) = (open.last(), pending.last_mut()) {
                        let items = std::mem::take(slot);
                        flush_lines(&mut out, &mut props_of, bid, items);
                    }
                    open.pop();
                    pending.pop();
                    style_owner.pop();
                }
                Some(3) => {
                    style_owner.pop();
                }
                _ => {}
            }
        }
    }

    // ---- phase B1: used widths, parents before children (§10.3.3) ----
    for i in 0..out.boxes.len() {
        // anonymous lines boxes: no margins/padding/border, width fills
        // the parent content box (they carry no element declarations)
        if out.boxes[i].kind == BoxKind::Lines {
            let avail = if out.boxes[i].parent == ROOT {
                VIEWPORT_W
            } else {
                out.boxes[out.boxes[i].parent].content_w
            };
            let b = &mut out.boxes[i];
            b.cb_w = avail;
            b.content_w = avail.max(0);
            continue;
        }
        let p = &props_of[i];
        let avail = if out.boxes[i].parent == ROOT {
            VIEWPORT_W
        } else {
            out.boxes[out.boxes[i].parent].content_w
        };

        let mtv = resolve_len(p.mt, avail);
        let mbv = resolve_len(p.mb, avail);
        let ptv = resolve_len(p.pt, avail).max(0);
        let pbv = resolve_len(p.pb, avail).max(0);
        let btv = resolve_len(p.bt, avail).max(0);
        let bbv = resolve_len(p.bb, avail).max(0);
        let plv = resolve_len(p.pl, avail).max(0);
        let prv = resolve_len(p.pr, avail).max(0);
        let blv = resolve_len(p.bl, avail).max(0);
        let brv = resolve_len(p.br, avail).max(0);

        let ml_auto = p.ml == Len::Auto;
        let mr_auto = p.mr == Len::Auto;
        let ml_fixed = if ml_auto { 0 } else { resolve_len(p.ml, avail) };
        let mr_fixed = if mr_auto { 0 } else { resolve_len(p.mr, avail) };

        // specified content width (None = auto)
        let spec_w: Option<i64> = match p.width {
            Len::Auto => None,
            Len::Px(v) => Some(v.max(0)), // negative width is invalid -> 0
            Len::Pct(pc) => Some(pct_of(pc, avail).max(0)),
        };
        let cw = spec_w.map(|w| {
            if p.border_box {
                // width includes padding and border: shrink the content
                (w.saturating_sub(plv).saturating_sub(prv)
                    .saturating_sub(blv).saturating_sub(brv))
                .max(0)
            } else {
                w
            }
        });

        let fixed = plv.saturating_add(prv).saturating_add(blv).saturating_add(brv);
        let (mlu, mru, content_w) = match cw {
            Some(w) => {
                let rem = avail
                    .saturating_sub(w)
                    .saturating_sub(fixed)
                    .saturating_sub(ml_fixed)
                    .saturating_sub(mr_fixed);
                match (ml_auto, mr_auto) {
                    (true, true) => {
                        if rem >= 0 {
                            let l = rem / 2;
                            (l, rem - l, w) // exact split, no leftover loss
                        } else {
                            // equal split would be negative: both 0, then
                            // the over-constrained rule moves the remainder
                            // to margin-right (LTR)
                            let r = avail.saturating_sub(w).saturating_sub(fixed);
                            (0, r, w)
                        }
                    }
                    (true, false) => (rem, mr_fixed, w),
                    // exactly one auto margin absorbs the remainder; no
                    // auto margins = over-constrained: margin-right takes
                    // the difference (LTR); rem may be negative
                    (false, _) => (ml_fixed, rem, w),
                }
            }
            None => {
                // width auto: any auto margins become 0, width fills the
                // rest (clamped at 0)
                let w = avail
                    .saturating_sub(fixed)
                    .saturating_sub(ml_fixed)
                    .saturating_sub(mr_fixed)
                    .max(0);
                (ml_fixed, mr_fixed, w)
            }
        };

        let b = &mut out.boxes[i];
        b.cb_w = avail;
        b.mt = mtv;
        b.mb = mbv;
        b.pt = ptv;
        b.pb = pbv;
        b.bt = btv;
        b.bb = bbv;
        b.pl = plv;
        b.pr = prv;
        b.bl = blv;
        b.br = brv;
        b.ml = mlu;
        b.mr = mru;
        b.content_w = content_w;
    }

    // ---- phase B1.5: line layout inside anonymous lines boxes ----
    // Runs need content widths (B1); heights consume the results (B2).
    // A per-element TextProps cache avoids re-parsing declarations; run
    // owners may be inline elements (no box), so the cache is sized by
    // the total element count observed in phase A.
    let total_elems = elem_index;
    let mut tp_cache: Vec<Option<TextProps>> = vec![None; total_elems.max(1)];
    for i in 0..out.boxes.len() {
        if out.boxes[i].kind != BoxKind::Lines {
            continue;
        }
        layout_lines(&mut out, i, ap, &mut tp_cache);
    }

    // ---- phase B2a: definite heights, parents FIRST (§10.5/§10.6.3) ----
    // Definiteness is a top-down property: a % height is definite only if
    // the parent's height is definite; the definite VALUE of a % height
    // needs the parent's used content height — both require parents-first.
    for i in 0..out.boxes.len() {
        let p = &props_of[i];
        let (parent_definite, parent_ch) = if out.boxes[i].parent == ROOT {
            (true, VIEWPORT_H) // the ICB has a definite height
        } else {
            let par = &out.boxes[out.boxes[i].parent];
            (par.height_definite, par.content_h)
        };
        // vertical paddings/borders resolved in B1 (read-only borrow ends here)
        let (ptv, pbv, btv, bbv) = {
            let b = &out.boxes[i];
            (b.pt, b.pb, b.bt, b.bb)
        };
        let hspec: Option<i64> = match p.height {
            Len::Px(v) => Some(v.max(0)), // negative height is invalid -> 0
            // §10.5: % height resolves only against a definite CB height
            Len::Pct(pc) if parent_definite => Some(pct_of(pc, parent_ch).max(0)),
            _ => None, // auto, or % against an indefinite chain -> auto
        };
        if let Some(h) = hspec {
            let content_h = if p.border_box {
                h.saturating_sub(ptv).saturating_sub(pbv)
                    .saturating_sub(btv).saturating_sub(bbv)
                    .max(0)
            } else {
                h
            };
            let b = &mut out.boxes[i];
            b.content_h = content_h;
            b.height_definite = true;
        }
    }

    // ---- phase B2b: auto heights, children FIRST (§10.6.3) ----
    // Reversed document order guarantees every descendant is solved before
    // its ancestor; definite-height boxes keep their B2a values; lines
    // boxes keep their B1.5 line sum (they have no element children).
    for i in (0..out.boxes.len()).rev() {
        if out.boxes[i].height_definite || out.boxes[i].kind == BoxKind::Lines {
            continue;
        }
        // auto height: sum of the in-flow children's margin boxes
        let ids: Vec<usize> = out.boxes[i].children.clone();
        let mut sum: i64 = 0;
        for c in ids {
            sum = sum.saturating_add(out.boxes[c].margin_h());
        }
        let b = &mut out.boxes[i];
        b.content_h = sum;
        b.height_definite = false;
    }

    // ---- phase C: positions, parents before children ----
    let mut icb_y: i64 = 0; // running stack inside the ICB
    for i in 0..out.boxes.len() {
        let is_root = out.boxes[i].parent == ROOT;
        if is_root {
            let b = &mut out.boxes[i];
            b.x = b.ml; // ICB content left edge is 0
            b.y = icb_y.saturating_add(b.mt);
            icb_y = b.y.saturating_add(b.border_h()).saturating_add(b.mb);
        }
        // children positions (roots and inner boxes alike)
        let (content_left, content_top, ids) = {
            let b = &out.boxes[i];
            (
                b.x.saturating_add(b.bl).saturating_add(b.pl),
                b.y.saturating_add(b.bt).saturating_add(b.pt),
                b.children.clone(),
            )
        };
        let mut content_top = content_top;
        for c in ids {
            let cb = &mut out.boxes[c];
            cb.x = content_left.saturating_add(cb.ml);
            cb.y = content_top.saturating_add(cb.mt);
            content_top = cb.y.saturating_add(cb.border_h()).saturating_add(cb.mb);
        }
    }

    out
}

/// Frozen dump format (machine-verified by tests):
/// `LAYOUT v2` header; one `B<index> <path>` line per element box in
/// document order (display:none subtrees are absent), then two-space
/// indented fields:
///   rect x y w h      border-box rectangle in page coordinates
///   margin t r b l    used margins
///   padding t r b l   used paddings
///   border t r b l    used border widths
///   content w h       content-box size
/// Anonymous lines boxes print `B# <parent path>::lines` and, after the
/// standard fields, one `LN y h chars w` per line box followed by its
/// words as `WORD x elem text` (text runs to the end of the line).
pub fn dump(out: &LayoutOut) -> String {
    let mut s = String::from("LAYOUT v2\n");
    for b in &out.boxes {
        if b.kind == BoxKind::Lines {
            s.push_str(&format!("B# {}\n", b.path));
        } else {
            s.push_str(&format!("B{} {}\n", b.index, b.path));
        }
        s.push_str(&format!("  rect {} {} {} {}\n", b.x, b.y, b.w(), b.border_h()));
        s.push_str(&format!("  margin {} {} {} {}\n", b.mt, b.mr, b.mb, b.ml));
        s.push_str(&format!("  padding {} {} {} {}\n", b.pt, b.pr, b.pb, b.pl));
        s.push_str(&format!("  border {} {} {} {}\n", b.bt, b.br, b.bb, b.bl));
        s.push_str(&format!("  content {} {}\n", b.content_w, b.content_h));
        for l in &b.lines {
            s.push_str(&format!(
                "  LN {} {} {} {}\n",
                l.y, l.h, l.chars, l.w
            ));
            for wd in &l.words {
                s.push_str(&format!("  WORD {} {} {}\n", wd.x, wd.elem, wd.text));
            }
        }
    }
    s
}

// ---------------- tests ----------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css;
    use crate::dom;

    fn styled_dom(html: &str, css_src: &str) -> (dom::ParseResult, LayoutOut) {
        let doc = dom::build(html);
        let o = css::parse(css_src);
        let ap = css::apply(&doc.dom, &o.rules);
        let lo = run(&doc.dom, &ap);
        (doc, lo)
    }

    fn box_of<'a>(lo: &'a LayoutOut, index: usize) -> &'a LBox {
        lo.boxes.iter().find(|b| b.index == index).unwrap()
    }

    #[test]
    fn value_parsing() {
        assert_eq!(parse_len("12px"), Some(Len::Px(12)));
        assert_eq!(parse_len("-3px"), Some(Len::Px(-3)));
        assert_eq!(parse_len("50%"), Some(Len::Pct(50)));
        assert_eq!(parse_len("0"), Some(Len::Px(0)));
        assert_eq!(parse_len("auto"), Some(Len::Auto));
        assert_eq!(parse_len("AUTO"), Some(Len::Auto));
        assert_eq!(parse_len(" 7.9px "), Some(Len::Px(8))); // rounds
        assert_eq!(parse_len("1e3px"), Some(Len::Px(1000)));
        assert_eq!(parse_len(".5px"), Some(Len::Px(1))); // .5 rounds away from zero
        assert_eq!(parse_len("12em"), None); // non-px unit unsupported
        assert_eq!(parse_len("red"), None);
        assert_eq!(parse_len(""), None);
        assert_eq!(parse_len("12"), None); // unitless non-zero
        assert_eq!(parse_len("5 px"), None);
        // clamp
        assert_eq!(parse_len("999999999999999999px"), Some(Len::Px(LEN_LIMIT)));
        assert_eq!(parse_len("-999999999999999999px"), Some(Len::Px(-LEN_LIMIT)));
        assert_eq!(pct_of(50, 800), 400);
        assert_eq!(pct_of(33, 50), 16); // trunc toward zero
        assert_eq!(pct_of(-10, 400), -40);
    }

    #[test]
    fn box_shorthand_expansion() {
        assert_eq!(
            parse_box4("1px", true),
            Some([Len::Px(1), Len::Px(1), Len::Px(1), Len::Px(1)])
        );
        assert_eq!(
            parse_box4("1px 2px", true),
            Some([Len::Px(1), Len::Px(2), Len::Px(1), Len::Px(2)])
        );
        assert_eq!(
            parse_box4("1px 2px 3px", true),
            Some([Len::Px(1), Len::Px(2), Len::Px(3), Len::Px(2)])
        );
        assert_eq!(
            parse_box4("0 auto", true),
            Some([Len::Px(0), Len::Auto, Len::Px(0), Len::Auto])
        );
        assert_eq!(parse_box4("0 auto", false), None); // padding: no auto
        assert_eq!(parse_box4("1px 2px 3px 4px 5px", true), None);
        assert_eq!(parse_box4("red", true), None);
        assert_eq!(parse_box4("", true), None);
    }

    #[test]
    fn display_defaults() {
        assert_eq!(default_display("div"), Disp::Block);
        assert_eq!(default_display("p"), Disp::Block);
        assert_eq!(default_display("span"), Disp::Inline);
        assert_eq!(default_display("em"), Disp::Inline);
        assert_eq!(default_display("head"), Disp::None);
        assert_eq!(default_display("script"), Disp::None);
        assert_eq!(default_display("br"), Disp::None);
        assert_eq!(default_display("title"), Disp::None);
    }

    #[test]
    fn props_errors_counted() {
        let p = parse_props(
            "div",
            &[
                ("width".into(), "red".into()),
                ("display".into(), "flex".into()),
                ("color".into(), "blue".into()), // ignored, not an error
                ("border".into(), "1px solid black".into()), // not read, not an error
            ],
        );
        assert_eq!(p.errors, 2);
        assert_eq!(p.width, Len::Auto); // falls back
        assert_eq!(p.disp, Disp::Block); // unsupported value keeps default
        let p = parse_props("div", &[("padding".into(), "auto".into())]);
        assert_eq!(p.errors, 1); // padding never takes auto
        let p = parse_props("div", &[("height".into(), "2em".into())]);
        assert_eq!(p.errors, 1);
    }

    #[test]
    fn width_auto_fills_containing_block() {
        // ordinals: 0 html, 1 head (hidden), 2 body, 3 div — all auto widths
        let (_, lo) = styled_dom("<div>x</div>", "");
        assert_eq!(lo.hidden, 1); // head
        assert_eq!(lo.errors, 0);
        assert_eq!(lo.boxes.len(), 4); // html, body, div + anonymous lines
        assert_eq!(box_of(&lo, 0).content_w, 800);
        assert_eq!(box_of(&lo, 2).content_w, 800);
        let d = box_of(&lo, 3);
        assert_eq!(d.content_w, 800);
        assert_eq!((d.x, d.y), (0, 0));
        assert_eq!(d.cb_w, 800);
    }

    #[test]
    fn width_definite_centering_and_over_constrained() {
        // margin: 0 auto centers the 200px box: ml = mr = 300
        let (_, lo) = styled_dom(
            r#"<div id="a" style="width:200px;margin:0 auto">x</div>"#,
            "",
        );
        let d = box_of(&lo, 3); // div ordinal
        assert_eq!((d.ml, d.mr), (300, 300));
        assert_eq!(d.x, 300);
        assert_eq!(d.content_w, 200);

        // width 200, margins 0: over-constrained, margin-right absorbs
        let (_, lo) = styled_dom(r#"<div style="width:200px">x</div>"#, "");
        let d = box_of(&lo, 3);
        assert_eq!((d.ml, d.mr), (0, 600));

        // only margin-left auto absorbs the whole remainder
        let (_, lo) = styled_dom(
            r#"<div style="width:200px;margin-left:auto">x</div>"#,
            "",
        );
        let d = box_of(&lo, 3);
        assert_eq!((d.ml, d.mr), (600, 0));
        assert_eq!(d.x, 600);
    }

    #[test]
    fn width_auto_makes_auto_margins_zero() {
        // §10.3.3: width auto + auto margins -> margins 0, width fills
        let (_, lo) = styled_dom(r#"<div style="margin:0 auto">x</div>"#, "");
        let d = box_of(&lo, 3);
        assert_eq!((d.ml, d.mr), (0, 0));
        assert_eq!(d.content_w, 800);
    }

    #[test]
    fn box_sizing_border_box() {
        // width 100 border-box with padding 10 and border 2 -> content 76
        let (_, lo) = styled_dom(
            r#"<div style="box-sizing:border-box;width:100px;height:40px;padding:10px;border-width:2px">x</div>"#,
            "",
        );
        let d = box_of(&lo, 3);
        assert_eq!(d.content_w, 76);
        assert_eq!(d.content_h, 16);
        assert_eq!(d.w(), 100);
        assert_eq!(d.border_h(), 40);
        // equation holds exactly
        assert_eq!(d.w(), d.bl + d.pl + d.content_w + d.pr + d.br);
        assert_eq!(d.border_h(), d.bt + d.pt + d.content_h + d.pb + d.bb);
    }

    #[test]
    fn height_auto_stacks_children() {
        // two siblings 40/50 tall with margins 5/10 and 10/20;
        // ordinals: 0 html, 2 body, 3 first div, 4 second div
        let (_, lo) = styled_dom(
            r#"<div style="height:40px;margin:5px 0 10px">a</div><div style="height:50px;margin:10px 0 20px">b</div>"#,
            "",
        );
        let body = box_of(&lo, 2);
        // body auto height = 5+40+10 + 10+50+20 = 135 (no collapsing)
        assert_eq!(body.content_h, 135);
        assert!(!body.height_definite);
        let c1 = box_of(&lo, 3);
        let c2 = box_of(&lo, 4);
        assert_eq!((c1.y, c1.content_h), (5, 40));
        assert_eq!(c2.y, 5 + 40 + 10 + 10); // prev margin-box + own mt
        assert_eq!(c2.content_h, 50);
        // html height = body margin-box height
        assert_eq!(box_of(&lo, 0).content_h, 135);
    }

    #[test]
    fn percentage_height_definite_vs_indefinite() {
        // definite chain: html 100% of 600, body 100% of 600, div 50% of 600
        let (_, lo) = styled_dom(
            r#"<div style="height:50%">x</div>"#,
            "html, body { height: 100% }",
        );
        assert_eq!(box_of(&lo, 0).content_h, 600);
        assert_eq!(box_of(&lo, 2).content_h, 600);
        assert_eq!(box_of(&lo, 3).content_h, 300);
        assert!(box_of(&lo, 3).height_definite);

        // indefinite chain: body auto -> div 50% computes to auto ->
        // its own line (24 = 16 * 1.5) instead of the stage-6 zero
        let (_, lo) = styled_dom(r#"<div style="height:50%">x</div>"#, "");
        assert_eq!(box_of(&lo, 2).content_h, 24); // body auto = div margin-box
        assert_eq!(box_of(&lo, 3).content_h, 24); // one line of text
        assert!(!box_of(&lo, 3).height_definite);
    }

    #[test]
    fn percentage_margins_resolve_against_width() {
        // vertical percentages resolve against the CB WIDTH per spec
        let (_, lo) = styled_dom(r#"<div style="margin-top:25%;height:10px">x</div>"#, "");
        let d = box_of(&lo, 3);
        assert_eq!(d.mt, 200); // 25% of 800
        assert_eq!(d.y, 200);
        // horizontal negative margin shifts the box left
        let (_, lo) = styled_dom(r#"<div style="margin-left:-50px;height:1px">x</div>"#, "");
        assert_eq!(box_of(&lo, 3).x, -50);
    }

    #[test]
    fn display_none_removes_subtree() {
        // ordinals: 0 html, 1 head(hidden), 2 body, 3 div, 4 div[none],
        // 5 span(inside hidden), 6 div(c)
        let (_, lo) = styled_dom(
            r#"<div>a<div style="display:none">b<span>deep</span></div><div>c</div></div>"#,
            "",
        );
        // boxes: html, body, outer div, its lines ('a'), last div,
        // its lines ('c'); the display:none div and span are gone and
        // their text produced no runs
        assert_eq!(lo.boxes.len(), 6);
        assert_eq!(lo.hidden, 2); // head + the display:none div
        assert!(!lo.boxes.iter().any(|b| b.path.contains("span")));
        let paths: Vec<&str> = lo.boxes.iter().map(|b| b.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "html",
                "html>body",
                "html>body>div",
                "html>body>div::lines",
                "html>body>div>div",
                "html>body>div>div::lines",
            ]
        );
    }

    #[test]
    fn non_rendered_elements_hidden() {
        // title/script land in head (display:none via head); br is none too.
        // hidden counts elements whose OWN display suppressed the box:
        // head + br = 2 (title/script sit inside a hidden subtree);
        // stage 9: 'x' feeds a lines box, br additionally acts as a break
        let (_, lo) = styled_dom("<title>t</title><script>1</script>x<br>", "");
        assert_eq!(lo.hidden, 2);
        assert_eq!(lo.boxes.len(), 3); // html, body, ::lines('x')
        assert_eq!(lo.breaks, 1);
        assert_eq!(lo.lines_total, 1);
    }

    #[test]
    fn inline_flow_no_inline_boxes() {
        // stage 9 deviation: inline elements carry no box; their text joins
        // the nearest block's inline flow with their own ordinal as owner;
        // ordinals: span 3, b 4
        let (_, lo) = styled_dom("<span>a</span><b>b</b>", "");
        assert_eq!(lo.boxes.len(), 3); // html, body, ::lines
        assert_eq!(lo.lines_total, 1);
        let lines_box = lo
            .boxes
            .iter()
            .find(|b| b.kind == BoxKind::Lines)
            .expect("lines box");
        assert_eq!(lines_box.lines.len(), 1);
        let words = &lines_box.lines[0].words;
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].elem, 3); // span owns 'a'
        assert_eq!(words[0].text, "a");
        assert_eq!(words[1].elem, 4); // b owns 'b'
        assert_eq!(words[1].text, "b");
    }

    #[test]
    fn run_style_owner_and_shared_segment() {
        // runs interleave across inline elements inside one block
        let (_, lo) = styled_dom("<p>one <em>two</em> three</p>", "");
        let lines_box = lo
            .boxes
            .iter()
            .find(|b| b.kind == BoxKind::Lines)
            .expect("lines box");
        let words = &lines_box.lines[0].words;
        let owners: Vec<usize> = words.iter().map(|w| w.elem).collect();
        assert_eq!(owners, vec![3, 4, 3]); // p, em, p
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["one", "two", "three"]);
        // the separator between "one" and "two" was kept (x of 'two' > 0)
        assert!(words[1].x > words[0].x);
    }

    #[test]
    fn text_line_height_default_is_15() {
        // HUMAN_FIRST: UA default line-height = 1.5 (not CSS `normal`)
        let (_, lo) = styled_dom("<p>text only</p>", "");
        let p = box_of(&lo, 3);
        assert_eq!(p.content_h, 24); // 16 * 1.5
        assert_eq!(lo.lines_total, 1);
        assert_eq!(lo.lh_violations, 0);
        assert_eq!(box_of(&lo, 2).content_h, 24); // body auto = p margin-box
    }

    #[test]
    fn line_wrap_narrow_container() {
        // 'aa bb cc' at 60px: 'aa bb' fits (20+10+20=50), 'cc' wraps
        let (_, lo) = styled_dom(r#"<p style="width:60px">aa bb cc</p>"#, "");
        let lines_box = lo
            .boxes
            .iter()
            .find(|b| b.kind == BoxKind::Lines)
            .expect("lines box");
        assert_eq!(lines_box.lines.len(), 2);
        let (l0, l1) = (&lines_box.lines[0], &lines_box.lines[1]);
        assert_eq!(l0.chars, 5); // 'aa bb'
        assert_eq!(l0.w, 50);
        assert_eq!(l1.chars, 2); // 'cc'
        assert_eq!(l1.w, 20);
        assert_eq!(l1.y, 24); // second line sits one line-height down
    }

    #[test]
    fn br_struts_and_trailing() {
        // a<br><br>b -> 3 line boxes: 'a', empty strut, 'b'
        let (_, lo) = styled_dom("<div>a<br><br>b</div>", "");
        assert_eq!(lo.breaks, 2);
        assert_eq!(lo.lines_total, 3);
        let lines_box = lo
            .boxes
            .iter()
            .find(|b| b.kind == BoxKind::Lines)
            .expect("lines box");
        assert_eq!(lines_box.lines[1].chars, 0); // strut
        assert_eq!(lines_box.lines[1].w, 0);
        assert_eq!(box_of(&lo, 3).content_h, 72); // 3 * 24
        // trailing br does not emit a phantom line
        let (_, lo) = styled_dom("<div>a<br></div>", "");
        assert_eq!(lo.lines_total, 1);
        // br on an empty line emits the strut (CSS: <div><br></div>)
        let (_, lo) = styled_dom("<div><br></div>", "");
        assert_eq!(lo.lines_total, 1);
        assert_eq!(box_of(&lo, 3).content_h, 24);
    }

    #[test]
    fn health_norms_counted() {
        // line-height below 1.5 counts a violation per line
        let (_, lo) = styled_dom(r#"<p style="line-height:1.2">x</p>"#, "");
        assert_eq!(lo.lines_total, 1);
        assert_eq!(lo.lh_violations, 1);
        // px value equal to 1.5x does not
        let (_, lo) = styled_dom(r#"<p style="line-height:24px">x</p>"#, "");
        assert_eq!(lo.lh_violations, 0);
        // a line over 75 characters counts a length violation (Baymard)
        let long = "a".repeat(80);
        let (_, lo) = styled_dom(&format!("<p>{}</p>", long), "");
        assert_eq!(lo.lines_total, 1);
        assert_eq!(lo.line_chars_max, 80);
        assert_eq!(lo.len_violations, 1);
        assert_eq!(lo.text_chars, 80);
    }

    #[test]
    fn font_size_scales_and_errors() {
        // px subset: 32px scales advance (10 -> 20) and line height (48)
        let (_, lo) = styled_dom(r#"<p style="font-size:32px">xy</p>"#, "");
        let lines_box = lo
            .boxes
            .iter()
            .find(|b| b.kind == BoxKind::Lines)
            .expect("lines box");
        assert_eq!(lines_box.lines[0].w, 40);
        assert_eq!(lines_box.lines[0].h, 48);
        assert_eq!(lo.lh_violations, 0);
        // non-px font-size counts an error and falls back to 16
        let (_, lo) = styled_dom(r#"<p style="font-size:2em">x</p>"#, "");
        assert_eq!(lo.errors, 1);
        let lines_box = lo
            .boxes
            .iter()
            .find(|b| b.kind == BoxKind::Lines)
            .expect("lines box");
        assert_eq!(lines_box.lines[0].w, 10);
    }

    #[test]
    fn text_props_inherited() {
        // stage 5 inheritance: body's font-size/color reach the run owner
        let (_, lo) = styled_dom(
            "<body><p>x</p></body>",
            "body { font-size: 32px }",
        );
        let lines_box = lo
            .boxes
            .iter()
            .find(|b| b.kind == BoxKind::Lines)
            .expect("lines box");
        assert_eq!(lines_box.lines[0].w, 20); // 'x' at 32px = 20
        assert_eq!(lines_box.lines[0].h, 48);
    }

    #[test]
    fn nbsp_is_word_glyph() {
        // NBSP is not a separator: 'a\u{a0}b' is one 3-glyph word
        let (_, lo) = styled_dom("<p>a\u{a0}b</p>", "");
        let lines_box = lo
            .boxes
            .iter()
            .find(|b| b.kind == BoxKind::Lines)
            .expect("lines box");
        assert_eq!(lines_box.lines[0].words.len(), 1);
        assert_eq!(lines_box.lines[0].chars, 3);
        assert_eq!(lines_box.lines[0].w, 30);
    }

    #[test]
    fn missing_glyph_counted() {
        // emoji has no glyph in the embedded font: measured as a space,
        // counted honestly (never invented, never dropped silently)
        let (_, lo) = styled_dom("<p>\u{1F4D6}</p>", "");
        assert_eq!(lo.missing_glyphs, 1);
        let lines_box = lo
            .boxes
            .iter()
            .find(|b| b.kind == BoxKind::Lines)
            .expect("lines box");
        assert_eq!(lines_box.lines[0].w, 10);
    }

    #[test]
    fn text_contributes_height() {
        // stage 9 REPLACES the v0.x invariant "text contributes no height":
        // 'text only' feeds one anonymous lines box; at the default 16px
        // with the HUMAN_FIRST UA default line-height 1.5 the p has a real
        // content height of 24, and body (auto) wraps it
        let (_, lo) = styled_dom("<p>text only</p>", "");
        assert_eq!(box_of(&lo, 3).content_h, 24);
        assert_eq!(box_of(&lo, 2).content_h, 24); // body auto -> wraps p
    }

    #[test]
    fn frozen_dump_format() {
        let (_, lo) = styled_dom(
            r#"<div id="a" style="width:200px;margin:0 auto"><p style="width:100px;height:50px">x</p></div>"#,
            "",
        );
        // stage 9: 'x' inside the definite-height p feeds an anonymous
        // lines box (B# ...::lines) with one 24px line, word 'x' (10px)
        let expected = concat!(
            "LAYOUT v2\n",
            "B0 html\n",
            "  rect 0 0 800 50\n",
            "  margin 0 0 0 0\n",
            "  padding 0 0 0 0\n",
            "  border 0 0 0 0\n",
            "  content 800 50\n",
            "B2 html>body\n",
            "  rect 0 0 800 50\n",
            "  margin 0 0 0 0\n",
            "  padding 0 0 0 0\n",
            "  border 0 0 0 0\n",
            "  content 800 50\n",
            "B3 html>body>div\n",
            "  rect 300 0 200 50\n",
            "  margin 0 300 0 300\n",
            "  padding 0 0 0 0\n",
            "  border 0 0 0 0\n",
            "  content 200 50\n",
            "B4 html>body>div>p\n",
            "  rect 300 0 100 50\n",
            "  margin 0 100 0 0\n",
            "  padding 0 0 0 0\n",
            "  border 0 0 0 0\n",
            "  content 100 50\n",
            "B# html>body>div>p::lines\n",
            "  rect 300 0 100 24\n",
            "  margin 0 0 0 0\n",
            "  padding 0 0 0 0\n",
            "  border 0 0 0 0\n",
            "  content 100 24\n",
            "  LN 0 24 1 10\n",
            "  WORD 0 4 x\n",
        );
        assert_eq!(dump(&lo), expected);
    }

    #[test]
    fn rect_equation_invariant() {
        let (_, lo) = styled_dom(
            r#"<div style="padding:4px;border-width:1px 2px;margin:3px"><p style="height:9px;width:70%">x</p><span style="margin:2px;padding:1px">y</span></div>"#,
            "p { margin: 0 8% }",
        );
        assert!(lo.errors == 0, "errors={}", lo.errors);
        for b in &lo.boxes {
            assert_eq!(
                b.w(),
                b.bl + b.pl + b.content_w + b.pr + b.br,
                "width equation broken for {}",
                b.path
            );
            assert_eq!(
                b.border_h(),
                b.bt + b.pt + b.content_h + b.pb + b.bb,
                "height equation broken for {}",
                b.path
            );
        }
    }

    #[test]
    fn deep_nesting_10000_no_overflow() {
        let n = 10000;
        let mut s = String::new();
        for _ in 0..n {
            s.push_str("<div>");
        }
        for _ in 0..n {
            s.push_str("</div>");
        }
        let (_, lo) = styled_dom(&s, "");
        // html + body + n divs (head is display:none)
        assert_eq!(lo.boxes.len(), n + 2);
        assert_eq!(lo.errors, 0);
        // innermost box sits at the end of a 10k-deep chain
        let last = &lo.boxes[lo.boxes.len() - 1];
        assert_eq!(last.index, n + 2);
        assert_eq!(last.path.split('>').count(), n + 2);
    }

    #[test]
    fn inherited_styles_do_not_affect_layout() {
        // color inherits; layout properties do not — the box is plain
        let (_, lo) = styled_dom(
            "<div><p>x</p></div>",
            "body { color: green; margin: 0 }",
        );
        assert_eq!(lo.errors, 0);
        let p = box_of(&lo, 3);
        assert_eq!((p.x, p.content_w), (0, 800));
        assert_eq!((p.mt, p.ml), (0, 0));
    }

    #[test]
    fn errors_propagate_through_run() {
        // unsupported values are counted by run(), not just parse_props
        let (_, lo) = styled_dom(
            r#"<div style="width:red;display:flex;height:2em">x</div>"#,
            "",
        );
        assert_eq!(lo.errors, 3);
        // layout still completes: unsupported display falls back to its
        // block default (a box exists), auto width fills the CB; the
        // 'x' text adds an anonymous lines box
        assert_eq!(lo.boxes.len(), 4);
        assert_eq!(box_of(&lo, 3).content_w, 800);
    }

    #[test]
    fn negative_height_and_width_clamp_to_zero() {
        let (_, lo) = styled_dom(
            r#"<div style="width:-5px;height:-9px">x</div>"#,
            "",
        );
        let d = box_of(&lo, 3);
        assert_eq!((d.content_w, d.content_h), (0, 0));
        assert_eq!(lo.errors, 0); // parse ok, used value clamped
    }
}
