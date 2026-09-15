//! neutral-browser — stage 6: block layout (LAYOUT).
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
//!   * no line layout: text nodes generate no boxes and contribute no
//!     height (the engine has no fonts); a block whose content is only
//!     text has content height 0;
//!   * inline-level boxes (display:inline and the inline-by-default
//!     element names) are laid out as blocks — block-in-inline splitting
//!     is NOT_IMPLEMENTED;
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

/// UA-subset: elements that never render. `br` is here because line
/// layout does not exist in this engine (a line break without lines
/// generates no box).
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
            // color/font/border-style/border-color/outline/... are not
            // layout properties in this subset: ignored, not errors
            _ => {}
        }
    }
    p
}

// ---------------- box arena ----------------

#[derive(Debug)]
pub struct LBox {
    pub index: usize,  // document-order element ordinal (matches STYLE)
    pub path: String,  // element names from the root, '>'-joined
    pub parent: usize, // box arena id, ROOT for top-level boxes
    pub children: Vec<usize>,
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

/// Run the block layout over the DOM with the computed STYLE.
/// Iterative in every pass: build (pre-order walk), widths (parents
/// first), heights (children first), positions (parents first).
pub fn run(dom: &Dom, ap: &StyleApply) -> LayoutOut {
    let mut out = LayoutOut::default();
    let mut props_of: Vec<Props> = Vec::new(); // parallel to out.boxes

    // ---- phase A: build the box arena in document order ----
    let mut path: Vec<String> = Vec::new();
    let mut open: Vec<usize> = Vec::new(); // arena ids of open boxes
    let mut actions: Vec<u8> = Vec::new(); // per open element: 0 suppressed, 1 hidden-root, 2 box
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
            let name = match &dom.nodes[id].data {
                NodeData::Element { name, .. } => name,
                _ => {
                    // non-element: traverse, no bookkeeping
                    for &c in dom.nodes[id].children.iter().rev() {
                        stack.push((c, false));
                        stack.push((c, true));
                    }
                    continue;
                }
            };
            let idx = elem_index;
            elem_index += 1;
            path.push(name.clone());
            if hidden_depth > 0 {
                // inside a display:none subtree: no box, no counting
                actions.push(0);
            } else {
                let decls = lookup_decls(ap, idx);
                let props = parse_props(name, decls);
                out.errors += props.errors;
                if props.disp == Disp::None {
                    out.hidden += 1;
                    hidden_depth += 1;
                    actions.push(1);
                } else {
                    let parent = open.last().copied().unwrap_or(ROOT);
                    let bid = out.boxes.len();
                    out.boxes.push(LBox {
                        index: idx,
                        path: path.join(">"),
                        parent,
                        children: Vec::new(),
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
                    actions.push(2);
                }
            }
            for &c in dom.nodes[id].children.iter().rev() {
                stack.push((c, false));
                stack.push((c, true));
            }
        } else if matches!(dom.nodes[id].data, NodeData::Element { .. }) {
            path.pop();
            match actions.pop() {
                Some(1) => hidden_depth -= 1,
                Some(2) => {
                    open.pop();
                }
                _ => {}
            }
        }
    }

    // ---- phase B1: used widths, parents before children (§10.3.3) ----
    for i in 0..out.boxes.len() {
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
    // its ancestor; definite-height boxes keep their B2a values.
    for i in (0..out.boxes.len()).rev() {
        if out.boxes[i].height_definite {
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
/// `LAYOUT v1` header; one `B<index> <path>` line per box in document
/// order (display:none subtrees are absent), then two-space indented
/// fields:
///   rect x y w h      border-box rectangle in page coordinates
///   margin t r b l    used margins
///   padding t r b l   used paddings
///   border t r b l    used border widths
///   content w h       content-box size
pub fn dump(out: &LayoutOut) -> String {
    let mut s = String::from("LAYOUT v1\n");
    for b in &out.boxes {
        s.push_str(&format!("B{} {}\n", b.index, b.path));
        s.push_str(&format!("  rect {} {} {} {}\n", b.x, b.y, b.w(), b.border_h()));
        s.push_str(&format!("  margin {} {} {} {}\n", b.mt, b.mr, b.mb, b.ml));
        s.push_str(&format!("  padding {} {} {} {}\n", b.pt, b.pr, b.pb, b.pl));
        s.push_str(&format!("  border {} {} {} {}\n", b.bt, b.br, b.bb, b.bl));
        s.push_str(&format!("  content {} {}\n", b.content_w, b.content_h));
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
        assert_eq!(lo.boxes.len(), 3); // html body div
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

        // indefinite chain: body auto -> div 50% computes to auto -> 0
        let (_, lo) = styled_dom(r#"<div style="height:50%">x</div>"#, "");
        assert_eq!(box_of(&lo, 2).content_h, 0); // auto, no element children
        assert_eq!(box_of(&lo, 3).content_h, 0);
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
        // boxes: html, body, outer div, last div (hidden div and span gone)
        assert_eq!(lo.boxes.len(), 4);
        assert_eq!(lo.hidden, 2); // head + the display:none div
        assert!(!lo.boxes.iter().any(|b| b.path.contains("span")));
        let paths: Vec<&str> = lo.boxes.iter().map(|b| b.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["html", "html>body", "html>body>div", "html>body>div>div"]
        );
    }

    #[test]
    fn non_rendered_elements_hidden() {
        // title/script land in head (display:none via head); br is none too.
        // hidden counts elements whose OWN display suppressed the box:
        // head + br = 2 (title/script sit inside a hidden subtree)
        let (_, lo) = styled_dom("<title>t</title><script>1</script>x<br>", "");
        assert_eq!(lo.hidden, 2);
        assert_eq!(lo.boxes.len(), 2); // html, body
    }

    #[test]
    fn inline_default_lays_out_as_block() {
        // documented deviation: inline boxes become blocks and stack;
        // ordinals: span 3, b 4
        let (_, lo) = styled_dom("<span>a</span><b>b</b>", "");
        assert_eq!(lo.boxes.len(), 4); // html, body, span, b
        let s = box_of(&lo, 3);
        let b = box_of(&lo, 4);
        assert_eq!(b.y, s.y + s.border_h()); // stacked, not side by side
    }

    #[test]
    fn text_contributes_no_height() {
        let (_, lo) = styled_dom("<p>text only</p>", "");
        assert_eq!(box_of(&lo, 3).content_h, 0);
        assert_eq!(box_of(&lo, 2).content_h, 0); // body auto -> 0
    }

    #[test]
    fn frozen_dump_format() {
        let (_, lo) = styled_dom(
            r#"<div id="a" style="width:200px;margin:0 auto"><p style="width:100px;height:50px">x</p></div>"#,
            "",
        );
        let expected = concat!(
            "LAYOUT v1\n",
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
        // block default (a box exists), auto width fills the CB
        assert_eq!(lo.boxes.len(), 3);
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
