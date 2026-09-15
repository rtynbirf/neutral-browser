//! HTML tokenizer — a documented subset of the WHATWG HTML Standard
//! tokenization section (§13.5), no external crates, no I/O here.
//!
//! Architecture mirrors the standard: the tree builder (dom.rs) decides the
//! tokenizer mode by flipping TokControl after receiving a start tag
//! (script/style -> RAWTEXT, title/textarea -> RCDATA, plaintext -> PLAINTEXT).
//! A self-closing start tag never triggers a raw mode (the tree builder does
//! not set the switch for it) — `<script/>` therefore does NOT swallow the
//! rest of the document.
//!
//! Implemented states: Data, TagOpen, EndTagOpen, TagName, MarkupDeclOpen,
//! the full attribute family, comments (incl. the `--!>` close), bogus
//! comment, DOCTYPE (name only), RAWTEXT/RCDATA with appropriate end tag,
//! PLAINTEXT, and character references.
//!
//! Documented subset decisions (NOT hallucinated "full support"):
//! - Named character references: a fixed table of common entities,
//!   semicolon REQUIRED. The full 2231-entry table and the legacy
//!   without-semicolon set are NOT implemented (unknown `&name;` stays
//!   literal text, which matches the spec's behavior for unknown names).
//! - Numeric references require `;` (the spec consumes missing-`;` with a
//!   parse error; we keep them literal). The normative windows-1252 remap
//!   table for 0x80..=0x9F IS implemented, as are the U+FFFD rules.
//! - DOCTYPE: the name is captured; public/system ids and quirks mode are NOT.
//! - CDATA sections in HTML content become bogus comments (spec behavior
//!   outside foreign content).
//! - `<!x`, `<?x`, `</3` become bogus comments (spec).
//! - Attributes of end tags are dropped (spec ignores them); inside raw
//!   modes the matched end tag swallows trailing junk up to `>`.
//! - Input is clamped to MAX_INPUT_CHARS; the char-vector representation is
//!   a documented stage-3 memory tradeoff (4 bytes per char).

pub const MAX_INPUT_CHARS: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Doctype { name: String },
    StartTag {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    EndTag { name: String },
    Text(String),
    Comment(String),
}

/// Set by the tree builder inside the token sink to force the tokenizer into
/// a raw mode after the current token (spec: tree construction drives the
/// tokenizer state).
#[derive(Default)]
pub struct TokControl {
    /// (frame element name, raw mode)
    pub switch: Option<(String, RawMode)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawMode {
    /// no character references, no tags (script, style, xmp, iframe, ...)
    Raw,
    /// character references decoded, no tags (title, textarea)
    Rc,
    /// everything is text forever, no references (plaintext)
    Plain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    Data,
    Plain,
    TagOpen,
    EndTagOpen,
    TagName,
    MarkupDeclOpen,
    BeforeAttrName,
    AttrName,
    AfterAttrName,
    BeforeAttrValue,
    AttrValueDq,
    AttrValueSq,
    AttrValueUq,
    AfterAttrValueQuoted,
    SelfClosingStart,
    CommentStart,
    CommentStartDash,
    Comment,
    CommentEndDash,
    CommentEnd,
    CommentEndBang,
    BogusComment,
    BeforeDoctypeName,
    DoctypeName,
    AfterDoctypeName,
    BogusDoctype,
    Raw,
    RawLessThan,
    RawEndTagOpen,
    RawEndTagName,
}

/// Named character reference table (subset; every entry is the exact WHATWG
/// name and code point). Semicolon required after the name.
const NAMED_REFS: &[(&str, u32)] = &[
    ("amp", 0x26),
    ("lt", 0x3C),
    ("gt", 0x3E),
    ("quot", 0x22),
    ("apos", 0x27),
    ("nbsp", 0xA0),
    ("shy", 0xAD),
    ("cent", 0xA2),
    ("pound", 0xA3),
    ("yen", 0xA5),
    ("sect", 0xA7),
    ("copy", 0xA9),
    ("laquo", 0xAB),
    ("reg", 0xAE),
    ("deg", 0xB0),
    ("plusmn", 0xB1),
    ("sup2", 0xB2),
    ("sup3", 0xB3),
    ("micro", 0xB5),
    ("para", 0xB6),
    ("middot", 0xB7),
    ("raquo", 0xBB),
    ("frac14", 0xBC),
    ("frac12", 0xBD),
    ("times", 0xD7),
    ("divide", 0xF7),
    ("Alpha", 0x391),
    ("Beta", 0x392),
    ("Pi", 0x3A0),
    ("Omega", 0x3A9),
    ("alpha", 0x3B1),
    ("beta", 0x3B2),
    ("gamma", 0x3B3),
    ("delta", 0x3B4),
    ("pi", 0x3C0),
    ("ensp", 0x2002),
    ("emsp", 0x2003),
    ("thinsp", 0x2009),
    ("zwnj", 0x200C),
    ("zwj", 0x200D),
    ("lrm", 0x200E),
    ("rlm", 0x200F),
    ("ndash", 0x2013),
    ("mdash", 0x2014),
    ("lsquo", 0x2018),
    ("rsquo", 0x2019),
    ("ldquo", 0x201C),
    ("rdquo", 0x201D),
    ("bull", 0x2022),
    ("hellip", 0x2026),
    ("prime", 0x2032),
    ("euro", 0x20AC),
    ("trade", 0x2122),
    ("larr", 0x2190),
    ("uarr", 0x2191),
    ("rarr", 0x2192),
    ("darr", 0x2193),
    ("harr", 0x2194),
    ("forall", 0x2200),
    ("part", 0x2202),
    ("exist", 0x2203),
    ("empty", 0x2205),
    ("nabla", 0x2207),
    ("isin", 0x2208),
    ("prod", 0x220F),
    ("sum", 0x2211),
    ("minus", 0x2212),
    ("lowast", 0x2217),
    ("radic", 0x221A),
    ("infin", 0x221E),
    ("cap", 0x2229),
    ("cup", 0x222A),
    ("int", 0x222B),
    ("ne", 0x2260),
    ("equiv", 0x2261),
    ("le", 0x2264),
    ("ge", 0x2265),
    ("spades", 0x2660),
    ("clubs", 0x2663),
    ("hearts", 0x2665),
    ("diams", 0x2666),
];

/// Normative windows-1252 remap for numeric references 0x80..=0x9F
/// (WHATWG HTML §13.2.5.72). Absent entries map to U+FFFD.
fn numeric_remap(n: u32) -> Option<u32> {
    let v = match n {
        0x80 => 0x20AC,
        0x82 => 0x201A,
        0x83 => 0x0192,
        0x84 => 0x201E,
        0x85 => 0x2026,
        0x86 => 0x2020,
        0x87 => 0x2021,
        0x88 => 0x02C6,
        0x89 => 0x2030,
        0x8A => 0x0160,
        0x8B => 0x2039,
        0x8C => 0x0152,
        0x8E => 0x017D,
        0x91 => 0x2018,
        0x92 => 0x2019,
        0x93 => 0x201C,
        0x94 => 0x201D,
        0x95 => 0x2022,
        0x96 => 0x2013,
        0x97 => 0x2014,
        0x98 => 0x02DC,
        0x99 => 0x2122,
        0x9A => 0x0161,
        0x9B => 0x203A,
        0x9C => 0x0153,
        0x9E => 0x017E,
        0x9F => 0x0178,
        _ => return None,
    };
    Some(v)
}

enum CharRef {
    /// Replacement text (1 char in this subset) or literal reconstruction.
    Chars(String),
    /// Just `&`; the run was not consumed.
    LiteralAmp,
}

struct Tokenizer<'a> {
    chars: &'a [char],
    pos: usize,
    state: St,
    raw: RawMode,
    frame: String,
    text: String,
    err: usize,
    tag_name: String,
    tag_end: bool,
    tag_self_closing: bool,
    tag_attrs: Vec<(String, String)>,
    comment: String,
    doctype_name: String,
}

impl<'a> Tokenizer<'a> {
    fn err(&mut self) {
        self.err += 1;
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn flush_text(&mut self, sink: &mut dyn FnMut(Token, &mut TokControl), ctl: &mut TokControl) {
        if !self.text.is_empty() {
            let t = std::mem::take(&mut self.text);
            sink(Token::Text(t), ctl);
        }
    }

    fn start_attr(&mut self, name: String) {
        self.tag_attrs.push((name, String::new()));
    }

    fn last_attr_value(&mut self) -> &mut String {
        let n = self.tag_attrs.len();
        &mut self.tag_attrs[n - 1].1
    }

    /// Character reference; called with self.pos just after the `&`.
    /// Consumes input on success; on `LiteralAmp` nothing beyond `&` is eaten.
    fn char_ref(&mut self) -> CharRef {
        let amp = self.pos - 1;
        if self.peek() == Some('#') {
            self.pos += 1;
            let hex = matches!(self.peek(), Some('x') | Some('X'));
            if hex {
                self.pos += 1;
            }
            let digits_start = self.pos;
            while let Some(c) = self.peek() {
                let ok = if hex {
                    c.is_ascii_hexdigit()
                } else {
                    c.is_ascii_digit()
                };
                if ok {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            let ndigits = self.pos - digits_start;
            let semi = self.peek() == Some(';');
            if ndigits == 0 || !semi {
                // Documented subset deviation: no digits or missing `;` ->
                // the whole consumed slice stays literal (original case kept).
                let s: String = self.chars[amp..self.pos].iter().collect();
                return CharRef::Chars(s);
            }
            let num_str: String = self.chars[digits_start..self.pos].iter().collect();
            self.pos += 1; // consume ';'
            let cp = u32::from_str_radix(&num_str, if hex { 16 } else { 10 }).unwrap_or(0x110000);
            let cp = if cp == 0 || (0xD800..=0xDFFF).contains(&cp) || cp > 0x10FFFF {
                0xFFFD
            } else if (0x80..=0x9F).contains(&cp) {
                numeric_remap(cp).unwrap_or(0xFFFD)
            } else {
                cp
            };
            return CharRef::Chars(char::from_u32(cp).unwrap_or('\u{FFFD}').to_string());
        }
        // Named reference: longest table match, `;` required.
        if !self.peek().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false) {
            return CharRef::LiteralAmp;
        }
        let run_start = self.pos;
        let mut run_end = self.pos;
        while self
            .chars
            .get(run_end)
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false)
        {
            run_end += 1;
        }
        let run: String = self.chars[run_start..run_end].iter().collect();
        let mut len = run.len();
        while len >= 1 {
            if self.chars.get(run_start + len) == Some(&';') {
                let name = &run[..len];
                if let Some((_, cp)) = NAMED_REFS.iter().find(|(n, _)| *n == name) {
                    self.pos = run_start + len + 1;
                    return CharRef::Chars(char::from_u32(*cp).unwrap_or('\u{FFFD}').to_string());
                }
            }
            len -= 1;
        }
        CharRef::LiteralAmp
    }

    fn emit_text_token(&mut self, sink: &mut dyn FnMut(Token, &mut TokControl), ctl: &mut TokControl) {
        self.flush_text(sink, ctl);
    }

    fn emit_comment(&mut self, sink: &mut dyn FnMut(Token, &mut TokControl), ctl: &mut TokControl) {
        let d = std::mem::take(&mut self.comment);
        self.flush_text(sink, ctl);
        sink(Token::Comment(d), ctl);
    }

    fn emit_doctype(&mut self, sink: &mut dyn FnMut(Token, &mut TokControl), ctl: &mut TokControl) {
        let n = std::mem::take(&mut self.doctype_name);
        self.flush_text(sink, ctl);
        sink(Token::Doctype { name: n }, ctl);
    }

    fn emit_tag(&mut self, sink: &mut dyn FnMut(Token, &mut TokControl), ctl: &mut TokControl) {
        self.flush_text(sink, ctl);
        let name = std::mem::take(&mut self.tag_name);
        let attrs = std::mem::take(&mut self.tag_attrs);
        let sc = std::mem::take(&mut self.tag_self_closing);
        let end = self.tag_end;
        self.tag_end = false;
        if end {
            sink(Token::EndTag { name }, ctl);
            self.state = St::Data;
        } else {
            sink(
                Token::StartTag {
                    name,
                    attrs,
                    self_closing: sc,
                },
                ctl,
            );
            // The tree builder may request a raw mode (script/style/title/...).
            if let Some((frame, mode)) = ctl.switch.take() {
                self.frame = frame;
                self.raw = mode;
                self.state = if mode == RawMode::Plain {
                    St::Plain
                } else {
                    St::Raw
                };
            } else {
                self.state = St::Data;
            }
        }
    }
}

/// Tokenize the whole input, streaming tokens to `sink`. Returns the parse
/// error count.
pub fn tokenize(input: &str, sink: &mut dyn FnMut(Token, &mut TokControl)) -> usize {
    let owned: Vec<char> = input.chars().take(MAX_INPUT_CHARS).collect();
    let mut ctl = TokControl::default();
    let mut t = Tokenizer {
        chars: &owned,
        pos: 0,
        state: St::Data,
        raw: RawMode::Raw,
        frame: String::new(),
        text: String::new(),
        err: 0,
        tag_name: String::new(),
        tag_end: false,
        tag_self_closing: false,
        tag_attrs: Vec::new(),
        comment: String::new(),
        doctype_name: String::new(),
    };

    loop {
        let Some(c) = t.peek() else {
            break;
        };
        t.pos += 1;
        match t.state {
            St::Data => match c {
                '&' => match t.char_ref() {
                    CharRef::Chars(s) => t.text.push_str(&s),
                    CharRef::LiteralAmp => t.text.push('&'),
                },
                '<' => t.state = St::TagOpen,
                '\0' => {
                    t.err();
                    t.text.push('\u{FFFD}');
                }
                _ => t.text.push(c),
            },
            St::Plain => t.text.push(c),
            St::TagOpen => match c {
                '!' => t.state = St::MarkupDeclOpen,
                '/' => t.state = St::EndTagOpen,
                '?' => {
                    t.err();
                    t.comment.clear();
                    t.pos -= 1; // reconsume '?' into the comment data (spec)
                    t.state = St::BogusComment;
                }
                c if c.is_ascii_alphabetic() => {
                    t.tag_end = false;
                    t.tag_self_closing = false;
                    t.tag_name.clear();
                    t.tag_name.push(c.to_ascii_lowercase());
                    t.tag_attrs.clear();
                    t.state = St::TagName;
                }
                _ => {
                    t.err();
                    t.text.push('<');
                    t.pos -= 1; // reconsume in data (spec)
                    t.state = St::Data;
                }
            },
            St::EndTagOpen => match c {
                c if c.is_ascii_alphabetic() => {
                    t.tag_end = true;
                    t.tag_self_closing = false;
                    t.tag_name.clear();
                    t.tag_name.push(c.to_ascii_lowercase());
                    t.tag_attrs.clear();
                    t.state = St::TagName;
                }
                '>' => {
                    t.err(); // `</>`: ignored completely (spec)
                    t.state = St::Data;
                }
                _ => {
                    t.err();
                    t.comment.clear();
                    t.pos -= 1; // reconsume into bogus comment (spec: data "/")
                    t.state = St::BogusComment;
                }
            },
            St::TagName => match c {
                c if c.is_ascii_whitespace() => t.state = St::BeforeAttrName,
                '/' => t.state = St::SelfClosingStart,
                '>' => t.emit_tag(sink, &mut ctl),
                '\0' => {
                    t.err();
                    t.tag_name.push('\u{FFFD}');
                }
                c => t.tag_name.push(c.to_ascii_lowercase()),
            },
            St::MarkupDeclOpen => {
                // c is the char right after `<!`; look ahead as needed.
                if c == '-' && t.peek() == Some('-') {
                    t.pos += 1; // consume second '-'
                    t.comment.clear();
                    t.state = St::CommentStart;
                } else if c.to_ascii_lowercase() == 'd'
                    && matches_doctype(&t.chars[t.pos..])
                {
                    t.pos += 6; // consume "octype"
                    t.doctype_name.clear();
                    t.state = St::BeforeDoctypeName;
                } else if c == '[' && matches_cdata(&t.chars[t.pos..]) {
                    t.pos += 6; // consume "CDATA["
                    t.comment.clear();
                    t.comment.push_str("[CDATA["); // reconsume semantics (spec)
                    t.state = St::BogusComment;
                } else {
                    t.err();
                    t.comment.clear();
                    t.comment.push(c);
                    t.state = St::BogusComment;
                }
            }
            St::BeforeAttrName => match c {
                c if c.is_ascii_whitespace() => {}
                '/' => t.state = St::SelfClosingStart,
                '>' => t.emit_tag(sink, &mut ctl),
                '\0' => {
                    t.err();
                    t.start_attr("\u{FFFD}".into());
                    t.state = St::AttrName;
                }
                '=' => {
                    t.err();
                    t.start_attr("=".into());
                    t.state = St::AttrName;
                }
                c => {
                    t.start_attr(c.to_ascii_lowercase().to_string());
                    t.state = St::AttrName;
                }
            },
            St::AttrName => match c {
                c if c.is_ascii_whitespace() => t.state = St::AfterAttrName,
                '/' => t.state = St::SelfClosingStart,
                '>' => t.emit_tag(sink, &mut ctl),
                '=' => t.state = St::BeforeAttrValue,
                '\0' => {
                    t.err();
                    if let Some(n) = t.tag_attrs.last_mut() {
                        n.0.push('\u{FFFD}');
                    }
                }
                c => {
                    if let Some(n) = t.tag_attrs.last_mut() {
                        n.0.push(c.to_ascii_lowercase());
                    }
                }
            },
            St::AfterAttrName => match c {
                c if c.is_ascii_whitespace() => {}
                '/' => t.state = St::SelfClosingStart,
                '>' => t.emit_tag(sink, &mut ctl),
                '=' => t.state = St::BeforeAttrValue,
                '\0' => {
                    t.err();
                    t.start_attr("\u{FFFD}".into());
                    t.state = St::AttrName;
                }
                c => {
                    t.start_attr(c.to_ascii_lowercase().to_string());
                    t.state = St::AttrName;
                }
            },
            St::BeforeAttrValue => match c {
                c if c.is_ascii_whitespace() => {}
                '"' => t.state = St::AttrValueDq,
                '\'' => t.state = St::AttrValueSq,
                '>' => {
                    t.err(); // missing attribute value (spec)
                    t.emit_tag(sink, &mut ctl);
                }
                _ => {
                    t.state = St::AttrValueUq;
                    t.pos -= 1; // reconsume as unquoted value (spec)
                }
            },
            St::AttrValueDq => match c {
                '"' => t.state = St::AfterAttrValueQuoted,
                '&' => match t.char_ref() {
                    CharRef::Chars(s) => t.last_attr_value().push_str(&s),
                    CharRef::LiteralAmp => t.last_attr_value().push('&'),
                },
                '\0' => {
                    t.err();
                    t.last_attr_value().push('\u{FFFD}');
                }
                c => t.last_attr_value().push(c),
            },
            St::AttrValueSq => match c {
                '\'' => t.state = St::AfterAttrValueQuoted,
                '&' => match t.char_ref() {
                    CharRef::Chars(s) => t.last_attr_value().push_str(&s),
                    CharRef::LiteralAmp => t.last_attr_value().push('&'),
                },
                '\0' => {
                    t.err();
                    t.last_attr_value().push('\u{FFFD}');
                }
                c => t.last_attr_value().push(c),
            },
            St::AttrValueUq => match c {
                c if c.is_ascii_whitespace() => t.state = St::BeforeAttrName,
                '&' => match t.char_ref() {
                    CharRef::Chars(s) => t.last_attr_value().push_str(&s),
                    CharRef::LiteralAmp => t.last_attr_value().push('&'),
                },
                '>' => t.emit_tag(sink, &mut ctl),
                '\0' => {
                    t.err();
                    t.last_attr_value().push('\u{FFFD}');
                }
                c => t.last_attr_value().push(c),
            },
            St::AfterAttrValueQuoted => match c {
                c if c.is_ascii_whitespace() => t.state = St::BeforeAttrName,
                '/' => t.state = St::SelfClosingStart,
                '>' => t.emit_tag(sink, &mut ctl),
                _ => {
                    t.err();
                    t.start_attr(String::new());
                    t.state = St::AttrName;
                    t.pos -= 1; // reconsume (spec)
                }
            },
            St::SelfClosingStart => match c {
                '>' => {
                    t.tag_self_closing = true;
                    t.emit_tag(sink, &mut ctl);
                }
                _ => {
                    t.err();
                    t.state = St::BeforeAttrName;
                    t.pos -= 1; // reconsume (spec)
                }
            },
            St::CommentStart => match c {
                '-' => t.state = St::CommentStartDash,
                '>' => {
                    t.err();
                    t.emit_comment(sink, &mut ctl);
                    t.state = St::Data;
                }
                _ => {
                    t.state = St::Comment;
                    t.pos -= 1; // reconsume into comment data (spec)
                }
            },
            St::CommentStartDash => match c {
                '-' => t.state = St::CommentEnd,
                '>' => {
                    t.err();
                    t.emit_comment(sink, &mut ctl);
                    t.state = St::Data;
                }
                _ => {
                    t.comment.push('-');
                    t.state = St::Comment;
                    t.pos -= 1; // reconsume (spec)
                }
            },
            St::Comment => match c {
                '-' => t.state = St::CommentEndDash,
                '\0' => {
                    t.err();
                    t.comment.push('\u{FFFD}');
                }
                c => t.comment.push(c),
            },
            St::CommentEndDash => match c {
                '-' => t.state = St::CommentEnd,
                _ => {
                    t.comment.push('-');
                    t.state = St::Comment;
                    t.pos -= 1; // reconsume (spec)
                }
            },
            St::CommentEnd => match c {
                '>' => {
                    t.emit_comment(sink, &mut ctl);
                    t.state = St::Data;
                }
                '!' => {
                    t.err();
                    t.state = St::CommentEndBang;
                }
                '-' => {
                    t.err();
                    t.comment.push('-');
                }
                _ => {
                    t.err();
                    t.comment.push_str("--");
                    t.state = St::Comment;
                    t.pos -= 1; // reconsume (spec)
                }
            },
            St::CommentEndBang => match c {
                '-' => t.state = St::CommentEndDash,
                '>' => {
                    t.err();
                    t.emit_comment(sink, &mut ctl);
                    t.state = St::Data;
                }
                _ => {
                    t.err();
                    t.comment.push_str("--!");
                    t.state = St::Comment;
                    t.pos -= 1; // reconsume (spec)
                }
            },
            St::BogusComment => match c {
                '>' => {
                    t.emit_comment(sink, &mut ctl);
                    t.state = St::Data;
                }
                '\0' => t.comment.push('\u{FFFD}'),
                c => t.comment.push(c),
            },
            St::BeforeDoctypeName => match c {
                c if c.is_ascii_whitespace() => {}
                '>' => {
                    t.err();
                    t.emit_doctype(sink, &mut ctl);
                    t.state = St::Data;
                }
                '\0' => {
                    t.err();
                    t.doctype_name.push('\u{FFFD}');
                    t.state = St::DoctypeName;
                }
                c => {
                    t.doctype_name.push(c.to_ascii_lowercase());
                    t.state = St::DoctypeName;
                }
            },
            St::DoctypeName => match c {
                c if c.is_ascii_whitespace() => t.state = St::AfterDoctypeName,
                '>' => {
                    t.emit_doctype(sink, &mut ctl);
                    t.state = St::Data;
                }
                '\0' => {
                    t.err();
                    t.doctype_name.push('\u{FFFD}');
                }
                c => t.doctype_name.push(c.to_ascii_lowercase()),
            },
            St::AfterDoctypeName => match c {
                c if c.is_ascii_whitespace() => {}
                '>' => {
                    t.emit_doctype(sink, &mut ctl);
                    t.state = St::Data;
                }
                _ => {
                    t.err(); // public/system ids: NOT implemented (documented)
                    t.state = St::BogusDoctype;
                    t.pos -= 1; // reconsume (spec)
                }
            },
            St::BogusDoctype => match c {
                '>' => {
                    t.emit_doctype(sink, &mut ctl);
                    t.state = St::Data;
                }
                '\0' => t.err(),
                _ => {}
            },
            St::Raw => match c {
                '&' if t.raw == RawMode::Rc => match t.char_ref() {
                    CharRef::Chars(s) => t.text.push_str(&s),
                    CharRef::LiteralAmp => t.text.push('&'),
                },
                '<' => t.state = St::RawLessThan,
                '\0' if t.raw == RawMode::Rc => {
                    t.err();
                    t.text.push('\u{FFFD}');
                }
                c => t.text.push(c),
            },
            St::RawLessThan => match c {
                '/' => t.state = St::RawEndTagOpen,
                _ => {
                    t.text.push('<');
                    t.state = St::Raw;
                    t.pos -= 1; // reconsume (spec)
                }
            },
            St::RawEndTagOpen => match c {
                c if c.is_ascii_alphabetic() => {
                    t.tag_name.clear();
                    t.tag_name.push(c.to_ascii_lowercase());
                    t.state = St::RawEndTagName;
                }
                _ => {
                    t.text.push_str("</");
                    t.state = St::Raw;
                    t.pos -= 1; // reconsume (spec)
                }
            },
            St::RawEndTagName => match c {
                c if c.is_ascii_alphabetic() => t.tag_name.push(c.to_ascii_lowercase()),
                c if c.is_ascii_whitespace() || c == '/' || c == '>' => {
                    if t.tag_name == t.frame {
                        // Appropriate end tag: attributes on end tags are
                        // dropped (spec); swallow junk through `>`.
                        if c != '>' {
                            while let Some(nc) = t.peek() {
                                t.pos += 1;
                                if nc == '>' {
                                    break;
                                }
                            }
                        }
                        let name = std::mem::take(&mut t.tag_name);
                        t.emit_text_token(sink, &mut ctl);
                        sink(Token::EndTag { name }, &mut ctl);
                        t.state = St::Data;
                    } else {
                        t.text.push_str("</");
                        let n = std::mem::take(&mut t.tag_name);
                        t.text.push_str(&n);
                        t.text.push(c);
                        t.state = St::Raw;
                    }
                }
                _ => {
                    t.text.push_str("</");
                    let n = std::mem::take(&mut t.tag_name);
                    t.text.push_str(&n);
                    t.text.push(c);
                    t.state = St::Raw;
                }
            },
        }
    }

    // EOF handling per state (subset: flush what is pending, count errors).
    match t.state {
        St::TagOpen | St::EndTagOpen | St::TagName | St::MarkupDeclOpen | St::BeforeAttrName
        | St::AttrName | St::AfterAttrName | St::BeforeAttrValue | St::AttrValueDq
        | St::AttrValueSq | St::AttrValueUq | St::AfterAttrValueQuoted | St::SelfClosingStart => {
            t.err(); // eof-in-tag: pending tag is dropped (spec)
        }
        St::RawEndTagOpen | St::RawEndTagName => {
            t.err();
            t.text.push_str("</");
            let n = std::mem::take(&mut t.tag_name);
            t.text.push_str(&n);
        }
        _ => {}
    }

    // Emit trailing pending tokens accumulated at EOF.
    match t.state {
        St::MarkupDeclOpen => {
            // error already counted in the EOF block above
            t.emit_comment(sink, &mut ctl);
        }
        St::CommentStart | St::CommentStartDash | St::Comment | St::CommentEndDash
        | St::CommentEnd | St::CommentEndBang | St::BogusComment => t.emit_comment(sink, &mut ctl),
        St::BeforeDoctypeName | St::DoctypeName | St::AfterDoctypeName | St::BogusDoctype => {
            t.emit_doctype(sink, &mut ctl)
        }
        _ => {}
    }
    t.flush_text(sink, &mut ctl);
    t.err
}

fn matches_doctype(rest: &[char]) -> bool {
    let pat = ['o', 'c', 't', 'y', 'p', 'e'];
    rest.len() >= 6
        && pat
            .iter()
            .zip(&rest[..6])
            .all(|(p, c)| p == &c.to_ascii_lowercase())
}

fn matches_cdata(rest: &[char]) -> bool {
    let pat = ['C', 'D', 'A', 'T', 'A', '['];
    rest.len() >= 6
        && pat
            .iter()
            .zip(&rest[..6])
            .all(|(p, c)| p == &c.to_ascii_uppercase())
}

/// Default sink-side switch rule, mirroring what the tree builder does
/// (dom.rs is the authority in the real pipeline): rawtext-capable start
/// tags flip the tokenizer mode; self-closing tags never do.
fn apply_default_switch(tok: &Token, ctl: &mut TokControl) {
    if let Token::StartTag {
        name, self_closing, ..
    } = tok
    {
        if !self_closing {
            let mode = match name.as_str() {
                "script" | "style" | "xmp" | "iframe" | "noembed" | "noframes" => RawMode::Raw,
                "title" | "textarea" => RawMode::Rc,
                "plaintext" => RawMode::Plain,
                _ => return,
            };
            ctl.switch = Some((name.clone(), mode));
        }
    }
}

/// Collect the full token stream (used by self-test and unit tests).
/// Applies the standard tree-construction raw-mode triggers.
pub fn collect(input: &str) -> (Vec<Token>, usize) {
    let mut toks = Vec::new();
    let n = tokenize(input, &mut |tok, ctl| {
        apply_default_switch(&tok, ctl);
        toks.push(tok);
    });
    (toks, n)
}

#[cfg(test)]
impl Token {
    fn tag_name(&self) -> &str {
        match self {
            Token::StartTag { name, .. } | Token::EndTag { name } => name,
            _ => "",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(input: &str) -> String {
        let (toks, _) = collect(input);
        toks.into_iter()
            .map(|t| match t {
                Token::Text(s) => s,
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn simple_tag_and_attrs() {
        let (toks, _) = collect(r#"<p CLASS=a data-x="1&amp;2" q='z'>t &amp; u</p>"#);
        assert_eq!(
            toks[0],
            Token::StartTag {
                name: "p".into(),
                attrs: vec![
                    ("class".into(), "a".into()),
                    ("data-x".into(), "1&2".into()),
                    ("q".into(), "z".into()),
                ],
                self_closing: false,
            }
        );
        assert_eq!(toks[1], Token::Text("t & u".into()));
        assert_eq!(toks[2], Token::EndTag { name: "p".into() });
    }

    #[test]
    fn self_closing_and_void() {
        let (toks, _) = collect("<br/><hr >");
        assert_eq!(
            toks[0],
            Token::StartTag {
                name: "br".into(),
                attrs: vec![],
                self_closing: true
            }
        );
        assert_eq!(
            toks[1],
            Token::StartTag {
                name: "hr".into(),
                attrs: vec![],
                self_closing: false
            }
        );
    }

    #[test]
    fn comments() {
        let (toks, e) = collect("a<!---->b<!--x--!>c<!--a -- b-->d");
        let comments: Vec<_> = toks
            .iter()
            .filter_map(|t| match t {
                Token::Comment(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(comments, vec!["", "x", "a -- b"]);
        assert_eq!(text_of("a<!---->b<!--x--!>c<!--a -- b-->d"), "abcd");
        // spec: `--!>` close = 2 parse errors (bang + emit);
        // ` -- ` inside a comment = 1 parse error (comment end anything-else)
        assert_eq!(e, 3);
    }

    #[test]
    fn bogus_comments() {
        let (toks, _) = collect("<?php echo ?> </3> <![CDATA[x]]>");
        let comments: Vec<_> = toks
            .iter()
            .filter_map(|t| match t {
                Token::Comment(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        // spec: `<?` reconsumes '?' into data (the trailing '?' before '>' is
        // ordinary comment content — bogus comments close at '>' only);
        // `</3` — digits never start a tag name, so bogus comment "3";
        // CDATA in HTML content keeps the "[CDATA[" prefix (reconsume).
        assert_eq!(comments, vec!["?php echo ?", "3", "[CDATA[x]]"]);
    }

    #[test]
    fn doctype() {
        let (toks, _) = collect("<!DOCTYPE HTML><!doctype html public \"x\">");
        assert_eq!(
            toks[0],
            Token::Doctype {
                name: "html".into()
            }
        );
        assert_eq!(
            toks[1],
            Token::Doctype {
                name: "html".into()
            }
        );
    }

    #[test]
    fn numeric_char_refs() {
        assert_eq!(text_of("&#65;&#x42;"), "AB");
        // windows-1252 remap (normative table)
        assert_eq!(text_of("&#128;"), "\u{20AC}");
        assert_eq!(text_of("&#x80;"), "\u{20AC}");
        // 0x81 not in table -> FFFD
        assert_eq!(text_of("&#129;"), "\u{FFFD}");
        // surrogate -> FFFD; NUL -> FFFD; out of range -> FFFD
        assert_eq!(text_of("&#xD800;"), "\u{FFFD}");
        assert_eq!(text_of("&#0;"), "\u{FFFD}");
        assert_eq!(text_of("&#1114112;"), "\u{FFFD}");
        // missing semicolon stays literal (documented subset deviation)
        assert_eq!(text_of("&#65"), "&#65");
        // no digits at all stays literal
        assert_eq!(text_of("&#x;"), "&#x;");
    }

    #[test]
    fn named_char_refs() {
        assert_eq!(text_of("a&amp;b &lt; &gt; &quot; &apos;"), "a&b < > \" '");
        assert_eq!(text_of("&nbsp;1&nbsp;2"), "\u{A0}1\u{A0}2");
        assert_eq!(
            text_of("&copy;&reg;&trade;&euro;"),
            "\u{A9}\u{AE}\u{2122}\u{20AC}"
        );
        assert_eq!(text_of("&mdash;&ndash;&hellip;"), "\u{2014}\u{2013}\u{2026}");
        // unknown name stays literal (spec behavior for unknown refs)
        assert_eq!(text_of("&unknown;"), "&unknown;");
        // longest match wins; case-sensitive table names
        assert_eq!(text_of("&le;&ge;&ne;"), "\u{2264}\u{2265}\u{2260}");
        assert_eq!(text_of("&Pi;&pi;"), "\u{3A0}\u{3C0}");
        // `&ampx;` has no table match -> literal
        assert_eq!(text_of("&ampx;"), "&ampx;");
    }

    #[test]
    fn attr_ref_exception() {
        // `&copy=2` has no semicolon -> literal (spec attribute exception)
        let (toks, _) = collect("<a href='?a=1&copy=2'>x</a>");
        match &toks[0] {
            Token::StartTag { attrs, .. } => {
                assert_eq!(attrs[0], ("href".into(), "?a=1&copy=2".into()));
            }
            _ => panic!("expected start tag"),
        }
    }

    #[test]
    fn rawtext_script() {
        let (toks, _) = collect(r#"<script>var s="</p>"; if(a<b) X();</script>after"#);
        assert_eq!(toks[0].tag_name(), "script");
        assert_eq!(toks[1], Token::Text(r#"var s="</p>"; if(a<b) X();"#.into()));
        assert_eq!(toks[2], Token::EndTag { name: "script".into() });
        assert_eq!(toks[3], Token::Text("after".into()));
    }

    #[test]
    fn rawtext_end_tag_with_junk() {
        // attributes on the rawtext end tag are dropped, not leaked as text
        let (toks, _) = collect("<style>x</style media=\"all\" >y");
        assert_eq!(toks[2], Token::EndTag { name: "style".into() });
        assert_eq!(toks[3], Token::Text("y".into()));
    }

    #[test]
    fn rawtext_case_insensitive_end() {
        let (toks, _) = collect("<script>a</SCRIPT>b");
        assert_eq!(toks[2], Token::EndTag { name: "script".into() });
        assert_eq!(toks[3], Token::Text("b".into()));
    }

    #[test]
    fn self_closing_script_does_not_enter_rawtext() {
        // tree builder decides; tokenizer alone must not force raw mode here:
        // with no sink control set, `<script/>` content is normal markup
        let (toks, _) = collect("<script/>ok");
        assert_eq!(toks[1], Token::Text("ok".into()));
    }

    #[test]
    fn rcdata_title() {
        let (toks, _) = collect("<title>1 < 2 &amp; 3</title>");
        assert_eq!(toks[1], Token::Text("1 < 2 & 3".into()));
    }

    #[test]
    fn rawtext_unclosed_at_eof_is_text() {
        let (toks, e) = collect("<style>a{}</style");
        assert!(e >= 1); // eof-in-tag
        assert!(matches!(toks.last(), Some(Token::Text(_))));
    }

    #[test]
    fn eof_in_tag_drops_tag() {
        let (toks, e) = collect("ok<div x=");
        assert_eq!(e, 1);
        assert_eq!(text_of("ok<div x="), "ok");
        let _ = toks;
    }

    #[test]
    fn lt_not_tag() {
        assert_eq!(text_of("a < b"), "a < b");
        assert_eq!(text_of("a <> b"), "a <> b"); // spec: `<>` -> literal '<' + '>'
        assert_eq!(text_of("a <<b> c"), "a < c"); // `<` literal, then real tag
    }

    #[test]
    fn empty_end_tag_ignored() {
        let (toks, e) = collect("a</>b");
        assert_eq!(e, 1);
        assert_eq!(text_of("a</>b"), "ab");
        let _ = toks;
    }

    #[test]
    fn duplicate_attrs_kept_by_tokenizer() {
        let (toks, _) = collect("<p a=1 a=2>");
        match &toks[0] {
            Token::StartTag { attrs, .. } => assert_eq!(attrs.len(), 2),
            _ => panic!(),
        }
    }
}
