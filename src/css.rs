//! neutral-browser — stage 4: CSS tokenizer -> stylesheet -> cascade -> STYLE.
//!
//! Chain position: URL -> BROKER -> TCP -> HTTP/1.1 -> BODY -> HTML -> DOM -> CSS -> STYLE.
//! Subset of CSS Syntax Level 3 + Selectors Level 3. Pure module: no I/O,
//! no dependencies, no panics on any input (error recovery, counted).
//!
//! Subset deviations from the full spec (documented, honest):
//! - comments produce no tokens (per spec); their bytes may still sit inside a
//!   declaration's value span and are stripped when the value text is built
//! - CDO/CDC ("<!--", "-->") tokenize as plain delimiters
//! - at-rules are consumed and ignored (counted in at_rules_skipped):
//!   no @media evaluation, no @import fetching
//! - selectors: type / universal / class / id / [attr] / [attr=value]
//!   (exact match only); descendant + child combinators; comma grouping;
//!   NO pseudo-classes, NO +/-/~ sibling combinators, NO attribute operators
//!   (~= ^= $= *= |=), NO namespaces
//! - declarations: the name must be an ident (lowercased); the value text is
//!   the verbatim source span (comments stripped, ends trimmed), whitespace
//!   inside is not collapsed
//! - cascade: author origin only; winner = max(important, inline, specificity,
//!   source order); no shorthand expansion, no var(), no inherit/initial/
//!   unset/revert keywords (their values stay verbatim)
//! - inheritance (stage 5): a frozen subset of inherited properties
//!   (`is_inherited`) propagates the parent's computed value down the tree;
//!   no initial values — an inherited property with no ancestor value stays
//!   absent; the dump marks received values with a `~` prefix (STYLE v2)
//! - sheet collection (stage 5): `collect_sheets` yields <style> texts and
//!   <link rel=stylesheet> href descriptors in document order; fetching link
//!   sheets is I/O and lives in main.rs (this module stays pure)
//! - a lone "-" is a Delim; a number immediately followed by "-" does not
//!   become a dimension (custom-property units are not supported)

use crate::dom::{Dom, NodeData, NodeId};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_INPUT_CHARS: usize = 8 * 1024 * 1024;

// ---------------- tokenizer ----------------

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ws,
    Ident(String),
    Function(String),
    AtKeyword(String),
    Hash(String),
    Str(String),
    BadStr,
    Url(String),
    BadUrl,
    Num(String),
    Percent(String),
    Dim(String, String), // number, unit
    Delim(char),
    Colon,
    Semi,
    Comma,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TokT {
    pub t: Tok,
    pub start: usize, // byte offset into the input (inclusive)
    pub end: usize,   // byte offset (exclusive)
}

pub fn tokenize(input: &str) -> (Vec<TokT>, usize) {
    let tz = Tokenizer::new(input);
    tz.run()
}

struct Tokenizer<'a> {
    src: &'a str,
    chars: Vec<char>,
    pos: Vec<usize>, // byte offset of each char
    i: usize,        // char index
    toks: Vec<TokT>,
    errors: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(src: &'a str) -> Self {
        let mut chars = Vec::new();
        let mut pos = Vec::new();
        for (b, c) in src.char_indices() {
            chars.push(c);
            pos.push(b);
        }
        Tokenizer {
            src,
            chars,
            pos,
            i: 0,
            toks: Vec::new(),
            errors: 0,
        }
    }

    /// Byte offset of char index `j` (input length past the end).
    fn byte(&self, j: usize) -> usize {
        self.pos.get(j).copied().unwrap_or(self.src.len())
    }

    fn peek(&self, k: usize) -> Option<char> {
        self.chars.get(self.i + k).copied()
    }

    fn cur(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn push(&mut self, t: Tok, start_ci: usize) {
        let start = self.byte(start_ci);
        let end = self.byte(self.i);
        self.toks.push(TokT { t, start, end });
    }

    fn ws(c: char) -> bool {
        matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}')
    }
    fn nl(c: char) -> bool {
        matches!(c, '\n' | '\r')
    }
    fn hexv(c: char) -> Option<u32> {
        c.to_digit(16)
    }
    fn ident_start(c: char) -> bool {
        c.is_ascii_alphabetic() || c == '_' || c >= '\u{80}'
    }
    fn ident_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_' || c == '-' || c >= '\u{80}'
    }

    /// Consume an escape sequence; `self.i` is on the char after `\`.
    fn escape(&mut self) -> char {
        match self.cur() {
            None => {
                self.errors += 1; // EOF after backslash
                '\u{fffd}'
            }
            Some(c) if Tokenizer::hexv(c).is_some() => {
                let mut v: u32 = 0;
                let mut n = 0;
                while n < 6 {
                    match self.cur() {
                        Some(h) if Tokenizer::hexv(h).is_some() => {
                            v = v.saturating_mul(16).saturating_add(Tokenizer::hexv(h).unwrap());
                            self.i += 1;
                            n += 1;
                        }
                        _ => break,
                    }
                }
                if let Some(c2) = self.cur() {
                    if Tokenizer::ws(c2) {
                        self.i += 1; // exactly one ws consumed after the escape
                    }
                }
                if v == 0 || v > 0x10ffff || (0xd800..=0xdfff).contains(&v) {
                    '\u{fffd}'
                } else {
                    char::from_u32(v).unwrap_or('\u{fffd}')
                }
            }
            Some(_) => {
                let c = self.cur().unwrap();
                self.i += 1;
                c
            }
        }
    }

    /// Consume an identifier starting at `self.i` (first char passes
    /// ident-start or is an escape; `-` handled by the caller).
    fn read_ident(&mut self) -> String {
        let mut s = String::new();
        loop {
            match self.cur() {
                Some(c) if Tokenizer::ident_char(c) => {
                    s.push(c);
                    self.i += 1;
                }
                Some('\\') => {
                    self.i += 1;
                    let e = self.escape();
                    s.push(e);
                }
                _ => break,
            }
        }
        s
    }

    fn at_ident_start(&self) -> bool {
        match self.cur() {
            Some(c) if Tokenizer::ident_start(c) => true,
            Some('\\') => true,
            _ => false,
        }
    }

    fn number_start(&self) -> bool {
        match self.cur() {
            Some(c) if c.is_ascii_digit() => true,
            Some('.') => matches!(self.peek(1), Some(d) if d.is_ascii_digit()),
            Some('+') | Some('-') => {
                let c1 = self.peek(1);
                match c1 {
                    Some(d) if d.is_ascii_digit() => true,
                    Some('.') => matches!(self.peek(2), Some(d) if d.is_ascii_digit()),
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn number(&mut self) -> String {
        let mut s = String::new();
        if matches!(self.cur(), Some('+') | Some('-')) {
            s.push(self.cur().unwrap());
            self.i += 1;
        }
        while matches!(self.cur(), Some(c) if c.is_ascii_digit()) {
            s.push(self.chars[self.i]);
            self.i += 1;
        }
        if self.cur() == Some('.') && matches!(self.peek(1), Some(d) if d.is_ascii_digit()) {
            s.push('.');
            self.i += 1;
            while matches!(self.cur(), Some(c) if c.is_ascii_digit()) {
                s.push(self.chars[self.i]);
                self.i += 1;
            }
        }
        if matches!(self.cur(), Some('e') | Some('E')) {
            let ok = matches!(self.peek(1), Some(d) if d.is_ascii_digit())
                || (matches!(self.peek(1), Some('+') | Some('-'))
                    && matches!(self.peek(2), Some(d) if d.is_ascii_digit()));
            if ok {
                s.push(self.chars[self.i]);
                self.i += 1;
                if matches!(self.cur(), Some('+') | Some('-')) {
                    s.push(self.chars[self.i]);
                    self.i += 1;
                }
                while matches!(self.cur(), Some(c) if c.is_ascii_digit()) {
                    s.push(self.chars[self.i]);
                    self.i += 1;
                }
            }
        }
        s
    }

    fn string(&mut self, quote: char, start_ci: usize) {
        self.i += 1; // opening quote
        let mut s = String::new();
        loop {
            match self.cur() {
                None => {
                    self.errors += 1; // EOF in string
                    self.push(Tok::BadStr, start_ci);
                    return;
                }
                Some(c) if c == quote => {
                    self.i += 1;
                    self.push(Tok::Str(s), start_ci);
                    return;
                }
                Some(c) if Tokenizer::nl(c) => {
                    // newline in string -> bad-string (subset: stop at the line end)
                    self.errors += 1;
                    self.push(Tok::BadStr, start_ci);
                    return;
                }
                Some('\\') => {
                    self.i += 1;
                    match self.cur() {
                        Some(c) if Tokenizer::nl(c) => {
                            // escaped newline in a string is dropped (per spec)
                            self.i += 1;
                            // handle \r\n as one newline
                            if c == '\r' && self.cur() == Some('\n') {
                                self.i += 1;
                            }
                        }
                        _ => {
                            let e = self.escape();
                            s.push(e);
                        }
                    }
                }
                Some(c) => {
                    s.push(c);
                    self.i += 1;
                }
            }
        }
    }

    /// `self.i` is on `(` right after the ident "url" (case-insensitive).
    fn url_token(&mut self, start_ci: usize) {
        self.i += 1; // consume (
        let after_paren = self.i;
        while matches!(self.cur(), Some(c) if Tokenizer::ws(c)) {
            self.i += 1;
        }
        // quoted -> it is a function call, rewind and let the main loop handle it
        if matches!(self.cur(), Some('"') | Some('\'')) {
            self.i = after_paren;
            self.push(Tok::Ident("url".into()), start_ci);
            self.push(Tok::LParen, after_paren - 1);
            return;
        }
        // unquoted url per spec: no ws, no quotes, no ( inside; ws is allowed
        // only right before the closing paren
        let mut s = String::new();
        loop {
            match self.cur() {
                None => {
                    self.errors += 1;
                    self.push(Tok::BadUrl, start_ci);
                    return;
                }
                Some(')') => {
                    self.i += 1;
                    self.push(Tok::Url(s), start_ci);
                    return;
                }
                Some(c) if Tokenizer::ws(c) => {
                    // ws allowed only directly before ')'
                    let mut k = self.i;
                    while matches!(self.chars.get(k).copied(), Some(c2) if Tokenizer::ws(c2)) {
                        k += 1;
                    }
                    if self.chars.get(k).copied() == Some(')') {
                        self.i = k + 1;
                        self.push(Tok::Url(s), start_ci);
                    } else {
                        self.errors += 1;
                        self.consume_bad_url();
                        self.push(Tok::BadUrl, start_ci);
                    }
                    return;
                }
                Some('"') | Some('\'') | Some('(') => {
                    self.errors += 1;
                    self.consume_bad_url();
                    self.push(Tok::BadUrl, start_ci);
                    return;
                }
                Some('\\') => {
                    self.i += 1;
                    let e = self.escape();
                    s.push(e);
                }
                Some(c) if Tokenizer::nl(c) => {
                    self.errors += 1;
                    self.consume_bad_url();
                    self.push(Tok::BadUrl, start_ci);
                    return;
                }
                Some(c) => {
                    s.push(c);
                    self.i += 1;
                }
            }
        }
    }

    /// Consume remnants of a bad url up to and including `)` (subset:
    /// escapes skip one char, no block nesting).
    fn consume_bad_url(&mut self) {
        while let Some(c) = self.cur() {
            if c == ')' {
                self.i += 1;
                return;
            }
            if c == '\\' {
                self.i += 2; // escape: skip the backslash and one char
            } else {
                self.i += 1;
            }
        }
    }

    fn run(mut self) -> (Vec<TokT>, usize) {
        while self.i < self.chars.len() {
            let start = self.i;
            let c = self.chars[self.i];

            if Tokenizer::ws(c) {
                while matches!(self.cur(), Some(c2) if Tokenizer::ws(c2)) {
                    self.i += 1;
                }
                self.push(Tok::Ws, start);
                continue;
            }

            // comment -> no token
            if c == '/' && self.peek(1) == Some('*') {
                self.i += 2;
                let mut closed = false;
                while self.i < self.chars.len() {
                    if self.chars[self.i] == '*' && self.peek(1) == Some('/') {
                        self.i += 2;
                        closed = true;
                        break;
                    }
                    self.i += 1;
                }
                if !closed {
                    self.errors += 1;
                }
                continue;
            }

            if c == '"' || c == '\'' {
                self.string(c, start);
                continue;
            }

            if c == '#' {
                self.i += 1;
                let is_name = matches!(self.cur(), Some(x) if Tokenizer::ident_char(x) || x == '\\');
                if is_name {
                    let name = self.read_ident();
                    self.push(Tok::Hash(name), start);
                } else {
                    self.push(Tok::Delim('#'), start);
                }
                continue;
            }

            if c == '@' {
                self.i += 1;
                if self.at_ident_start() {
                    let name = self.read_ident();
                    self.push(Tok::AtKeyword(name), start);
                } else {
                    self.push(Tok::Delim('@'), start);
                }
                continue;
            }

            if self.number_start() {
                let num = self.number();
                match self.cur() {
                    Some('%') => {
                        self.i += 1;
                        self.push(Tok::Percent(num), start);
                    }
                    Some(x) if Tokenizer::ident_start(x) || x == '\\' => {
                        let unit = self.read_ident();
                        self.push(Tok::Dim(num, unit), start);
                    }
                    _ => self.push(Tok::Num(num), start),
                }
                continue;
            }

            // lone "-" is a delim; "-" starting an identifier goes below
            if c == '-' {
                let starts_ident = matches!(self.peek(1), Some(x) if Tokenizer::ident_start(x) || x == '-' || x == '\\');
                if !starts_ident && !self.number_start() {
                    self.i += 1;
                    self.push(Tok::Delim('-'), start);
                    continue;
                }
            }

            let is_ident_start =
                Tokenizer::ident_start(c) || c == '-' || c == '\\';
            if is_ident_start {
                let name = if c == '\\' {
                    self.i += 1;
                    let e0 = self.escape();
                    let mut s = String::new();
                    s.push(e0);
                    s.push_str(&self.read_ident());
                    s
                } else {
                    self.read_ident()
                };
                if self.cur() == Some('(') {
                    if name.eq_ignore_ascii_case("url") {
                        self.url_token(start);
                    } else {
                        self.i += 1;
                        self.push(Tok::Function(name), start);
                    }
                } else {
                    self.push(Tok::Ident(name), start);
                }
                continue;
            }

            let t = match c {
                ':' => Tok::Colon,
                ';' => Tok::Semi,
                ',' => Tok::Comma,
                '(' => Tok::LParen,
                ')' => Tok::RParen,
                '[' => Tok::LBracket,
                ']' => Tok::RBracket,
                '{' => Tok::LBrace,
                '}' => Tok::RBrace,
                _ => Tok::Delim(c),
            };
            self.i += 1;
            self.push(t, start);
        }
        (self.toks, self.errors)
    }
}

// ---------------- selectors ----------------

#[derive(Debug, Clone, PartialEq)]
pub enum Simple {
    Type(String), // ASCII-lowercased
    Universal,
    Class(String),
    Id(String),
    Attr {
        name: String, // lowercased
        value: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Compound {
    pub parts: Vec<Simple>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Comb {
    Desc,
    Child,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Selector {
    pub compounds: Vec<Compound>,
    pub combs: Vec<Comb>, // combs[i] joins compounds[i] and compounds[i+1]
    pub spec: (u32, u32, u32),
}

// ---------------- declarations and rules ----------------

#[derive(Debug, Clone)]
pub struct Decl {
    pub name: String, // lowercased
    pub value: String,
    pub important: bool,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub selectors: Vec<Selector>,
    pub decls: Vec<Decl>,
    pub order: usize, // source order of the rule (0-based)
}

#[derive(Debug, Default)]
pub struct CssOutcome {
    pub rules: Vec<Rule>,
    pub errors: usize,
    pub at_rules_skipped: usize,
}

impl CssOutcome {
    /// Total number of surviving declarations across all rules.
    pub fn decl_count(&self) -> usize {
        self.rules.iter().map(|r| r.decls.len()).sum()
    }
}

pub fn parse(input: &str) -> CssOutcome {
    let mut out = CssOutcome::default();
    if input.len() > MAX_INPUT_CHARS {
        out.errors += 1;
        return out;
    }
    let (toks, terr) = tokenize(input);
    out.errors += terr;
    let n = toks.len();
    let mut i = 0;
    while i < n {
        match toks[i].t {
            Tok::Ws | Tok::Semi => {
                i += 1;
            }
            Tok::AtKeyword(_) => {
                // subset: consume the at-rule and ignore it entirely
                let mut j = i + 1;
                let mut block = None;
                while j < n {
                    match toks[j].t {
                        Tok::LBrace => {
                            block = Some(j);
                            break;
                        }
                        Tok::Semi => {
                            j += 1;
                            break;
                        }
                        _ => j += 1,
                    }
                }
                match block {
                    Some(bi) => {
                        let after = skip_balanced(&toks, bi);
                        if after == n && toks[n - 1].t != Tok::RBrace {
                            out.errors += 1; // EOF inside the at-rule block
                        }
                        i = after;
                    }
                    None => i = j,
                }
                out.at_rules_skipped += 1;
            }
            Tok::LBrace => {
                out.errors += 1; // orphan block
                i = skip_balanced(&toks, i);
            }
            Tok::RBrace => {
                out.errors += 1; // stray closing brace
                i += 1;
            }
            _ => {
                // qualified rule: prelude up to '{'
                let prelude_start = i;
                let mut j = i;
                let mut block = None;
                while j < n {
                    if matches!(toks[j].t, Tok::LBrace) {
                        block = Some(j);
                        break;
                    }
                    j += 1;
                }
                match block {
                    None => {
                        out.errors += 1; // EOF in prelude
                        i = n;
                    }
                    Some(bi) => {
                        let sels = parse_selectors(&toks[prelude_start..bi]);
                        let (decls, derr, next_i, closed) =
                            parse_decls(input, &toks, bi + 1, true);
                        out.errors += derr;
                        if !closed {
                            out.errors += 1; // EOF inside the rule block
                        }
                        i = next_i;
                        match sels {
                            Some(s) if !s.is_empty() => out.rules.push(Rule {
                                selectors: s,
                                decls,
                                order: out.rules.len(),
                            }),
                            _ => out.errors += 1, // invalid selector list -> rule dropped
                        }
                    }
                }
            }
        }
    }
    out
}

/// Index after the matching `}` for the `{` at `i` (or input end).
fn skip_balanced(toks: &[TokT], i: usize) -> usize {
    let n = toks.len();
    let mut depth = 0usize;
    let mut j = i;
    while j < n {
        match toks[j].t {
            Tok::LBrace => depth += 1,
            Tok::RBrace => {
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
        j += 1;
    }
    n
}

fn skip_to(toks: &[TokT], mut i: usize) -> usize {
    while i < toks.len() {
        match toks[i].t {
            Tok::Semi => return i + 1,
            Tok::RBrace => return i,
            Tok::LBrace => return skip_balanced(toks, i),
            _ => i += 1,
        }
    }
    i
}

/// Parse a declaration list starting at token index `i`.
/// Stops at `}` (consumed when `consume_rbrace`) or EOF.
fn parse_decls(
    input: &str,
    toks: &[TokT],
    mut i: usize,
    consume_rbrace: bool,
) -> (Vec<Decl>, usize, usize, bool) {
    let n = toks.len();
    let mut decls = Vec::new();
    let mut errors = 0;
    loop {
        while i < n && matches!(toks[i].t, Tok::Ws | Tok::Semi) {
            i += 1;
        }
        if i >= n {
            return (decls, errors, i, false);
        }
        if let Tok::RBrace = toks[i].t {
            let ni = if consume_rbrace { i + 1 } else { i };
            return (decls, errors, ni, true);
        }
        if let Tok::LBrace = toks[i].t {
            errors += 1; // nested rule inside a style block: parse error, skip
            i = skip_balanced(toks, i);
            continue;
        }
        let name = match &toks[i].t {
            Tok::Ident(nm) => {
                i += 1;
                // CSS custom property names (--x) are case-SENSITIVE;
                // regular property names fold to lowercase
                if nm.starts_with("--") {
                    nm.clone()
                } else {
                    nm.to_ascii_lowercase()
                }
            }
            _ => {
                errors += 1;
                i = skip_to(toks, i);
                continue;
            }
        };
        while i < n && toks[i].t == Tok::Ws {
            i += 1;
        }
        if i >= n || toks[i].t != Tok::Colon {
            errors += 1;
            i = skip_to(toks, i);
            continue;
        }
        i += 1;
        // collect value tokens until ; } { or EOF
        let vstart = i;
        while i < n && !matches!(toks[i].t, Tok::Semi | Tok::RBrace | Tok::LBrace) {
            i += 1;
        }
        let vend = i;
        match value_from(input, &toks[vstart..vend]) {
            Ok((value, important)) => {
                decls.push(Decl {
                    name,
                    value,
                    important,
                });
            }
            Err(()) => {
                errors += 1; // empty/invalid value -> declaration dropped
            }
        }
    }
}

/// Public wrapper for inline `style` attribute values (top-level list).
pub fn parse_declarations(input: &str) -> (Vec<Decl>, usize) {
    let (toks, terr) = tokenize(input);
    let (decls, e, _, _) = parse_decls(input, &toks, 0, false);
    (decls, e + terr)
}

fn strip_comments(s: &str) -> String {
    if !s.contains("/*") {
        return s.to_string();
    }
    let b: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == '/' && i + 1 < b.len() && b[i + 1] == '*' {
            i += 2;
            while i < b.len() {
                if b[i] == '*' && i + 1 < b.len() && b[i + 1] == '/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Build one declaration value from its tokens: balanced brackets required,
/// optional trailing `!important`, verbatim source span with comments stripped.
fn value_from(input: &str, toks: &[TokT]) -> Result<(String, bool), ()> {
    let mut bal_p = 0i32;
    let mut bal_b = 0i32;
    let mut bal_c = 0i32;
    for t in toks {
        match t.t {
            Tok::LParen | Tok::Function(_) => bal_p += 1,
            Tok::RParen => {
                bal_p -= 1;
                if bal_p < 0 {
                    return Err(());
                }
            }
            Tok::LBracket => bal_b += 1,
            Tok::RBracket => {
                bal_b -= 1;
                if bal_b < 0 {
                    return Err(());
                }
            }
            Tok::LBrace => bal_c += 1,
            Tok::RBrace => {
                bal_c -= 1;
                if bal_c < 0 {
                    return Err(());
                }
            }
            _ => {}
        }
    }
    if bal_p != 0 || bal_b != 0 || bal_c != 0 {
        return Err(());
    }

    let mut end = toks.len();
    let mut important = false;
    let bangs: Vec<usize> = toks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.t == Tok::Delim('!'))
        .map(|(k, _)| k)
        .collect();
    if bangs.len() > 1 {
        return Err(());
    }
    if bangs.len() == 1 {
        let p = bangs[0];
        let mut k = p + 1;
        while k < toks.len() && toks[k].t == Tok::Ws {
            k += 1;
        }
        let ok = match toks.get(k) {
            Some(t) => match &t.t {
                Tok::Ident(id) if id.eq_ignore_ascii_case("important") => {
                    toks[k + 1..].iter().all(|t2| t2.t == Tok::Ws)
                }
                _ => false,
            },
            None => false,
        };
        if ok {
            important = true;
            end = p;
        } else {
            return Err(()); // `!` must be followed by `important` at the value end
        }
    }

    let mut a = 0;
    while a < end && toks[a].t == Tok::Ws {
        a += 1;
    }
    let mut b = end;
    while b > a && toks[b - 1].t == Tok::Ws {
        b -= 1;
    }
    if a >= b {
        return Err(()); // empty value
    }
    let s = toks[a].start;
    let e = toks[b - 1].end;
    if s > e || e > input.len() {
        return Err(());
    }
    let txt = strip_comments(&input[s..e]);
    let t2 = txt.trim();
    if t2.is_empty() {
        return Err(());
    }
    Ok((t2.to_string(), important))
}

// ---------------- selector parsing ----------------

fn parse_selectors(toks: &[TokT]) -> Option<Vec<Selector>> {
    let mut out = Vec::new();
    let mut seg: Vec<&TokT> = Vec::new();
    let mut depth = 0usize;
    for t in toks {
        match t.t {
            Tok::LParen | Tok::LBracket | Tok::LBrace => {
                depth += 1;
                seg.push(t);
            }
            Tok::RParen | Tok::RBracket | Tok::RBrace => {
                depth = depth.saturating_sub(1);
                seg.push(t);
            }
            Tok::Comma if depth == 0 => {
                out.push(parse_one_selector(&seg)?);
                seg.clear();
            }
            _ => seg.push(t),
        }
    }
    out.push(parse_one_selector(&seg)?);
    Some(out)
}

fn parse_one_selector(seg: &[&TokT]) -> Option<Selector> {
    let n = seg.len();
    let mut i = 0;
    let mut compounds: Vec<Compound> = Vec::new();
    let mut combs: Vec<Comb> = Vec::new();
    loop {
        while i < n && seg[i].t == Tok::Ws {
            i += 1;
        }
        if i >= n {
            return None; // a compound is required
        }
        let c = parse_compound(seg, &mut i)?;
        compounds.push(c);
        let mut j = i;
        let mut had_ws = false;
        while j < n && seg[j].t == Tok::Ws {
            had_ws = true;
            j += 1;
        }
        if j >= n {
            break;
        }
        match seg[j].t {
            Tok::Delim('>') => {
                j += 1;
                while j < n && seg[j].t == Tok::Ws {
                    j += 1;
                }
                if j >= n {
                    return None;
                }
                combs.push(Comb::Child);
                i = j;
            }
            Tok::Delim('+') | Tok::Delim('~') => return None, // unsupported combinator
            _ if had_ws => {
                combs.push(Comb::Desc);
                i = j;
            }
            _ => return None, // tokens glued without a combinator
        }
    }
    let (mut a, mut b, mut cc) = (0u32, 0u32, 0u32);
    for comp in &compounds {
        for p in &comp.parts {
            match p {
                Simple::Id(_) => a += 1,
                Simple::Class(_) | Simple::Attr { .. } => b += 1,
                Simple::Type(_) => cc += 1,
                Simple::Universal => {}
            }
        }
    }
    Some(Selector {
        compounds,
        combs,
        spec: (a, b, cc),
    })
}

fn parse_compound(seg: &[&TokT], i: &mut usize) -> Option<Compound> {
    let mut parts = Vec::new();
    match &seg[*i].t {
        Tok::Ident(nm) => {
            parts.push(Simple::Type(nm.to_ascii_lowercase()));
            *i += 1;
        }
        Tok::Delim('*') => {
            parts.push(Simple::Universal);
            *i += 1;
        }
        Tok::Hash(nm) => {
            parts.push(Simple::Id(nm.clone()));
            *i += 1;
        }
        Tok::Delim('.') => {
            *i += 1;
            match seg.get(*i) {
                Some(t) => match &t.t {
                    Tok::Ident(nm) => {
                        parts.push(Simple::Class(nm.clone()));
                        *i += 1;
                    }
                    _ => return None,
                },
                None => return None,
            }
        }
        Tok::LBracket => {
            parts.push(parse_attr(seg, i)?);
        }
        _ => return None,
    }
    loop {
        match seg.get(*i).map(|t| &t.t) {
            Some(Tok::Delim('.')) => {
                *i += 1;
                match seg.get(*i) {
                    Some(t) => match &t.t {
                        Tok::Ident(nm) => {
                            parts.push(Simple::Class(nm.clone()));
                            *i += 1;
                        }
                        _ => return None,
                    },
                    None => return None,
                }
            }
            Some(Tok::Hash(nm)) => {
                parts.push(Simple::Id(nm.clone()));
                *i += 1;
            }
            Some(Tok::LBracket) => {
                parts.push(parse_attr(seg, i)?);
            }
            _ => break,
        }
    }
    Some(Compound { parts })
}

fn parse_attr(seg: &[&TokT], i: &mut usize) -> Option<Simple> {
    *i += 1; // [
    while *i < seg.len() && seg[*i].t == Tok::Ws {
        *i += 1;
    }
    let name = match seg.get(*i) {
        Some(t) => match &t.t {
            Tok::Ident(nm) => {
                let nm = nm.to_ascii_lowercase();
                *i += 1;
                nm
            }
            _ => return None,
        },
        None => return None,
    };
    while *i < seg.len() && seg[*i].t == Tok::Ws {
        *i += 1;
    }
    match seg.get(*i) {
        Some(t) if t.t == Tok::RBracket => {
            *i += 1;
            return Some(Simple::Attr { name, value: None });
        }
        Some(t) if t.t == Tok::Delim('=') => {
            *i += 1;
        }
        _ => return None, // attribute operators ~= ^= $= *= |= unsupported
    }
    while *i < seg.len() && seg[*i].t == Tok::Ws {
        *i += 1;
    }
    let value = match seg.get(*i) {
        Some(t) => match &t.t {
            Tok::Ident(v) => {
                let v = v.clone();
                *i += 1;
                v
            }
            Tok::Str(v) => {
                let v = v.clone();
                *i += 1;
                v
            }
            _ => return None,
        },
        None => return None,
    };
    while *i < seg.len() && seg[*i].t == Tok::Ws {
        *i += 1;
    }
    match seg.get(*i) {
        Some(t) if t.t == Tok::RBracket => {
            *i += 1;
            Some(Simple::Attr {
                name,
                value: Some(value),
            })
        }
        _ => None,
    }
}

// ---------------- matching ----------------

fn attr_val<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn compound_matches(dom: &Dom, id: NodeId, c: &Compound) -> bool {
    let (name, attrs) = match &dom.nodes[id].data {
        NodeData::Element { name, attrs } => (name.as_str(), attrs),
        _ => return false,
    };
    for p in &c.parts {
        let ok = match p {
            Simple::Universal => true,
            Simple::Type(t) => name == t,
            Simple::Class(cl) => match attr_val(attrs, "class") {
                Some(cv) => cv.split_ascii_whitespace().any(|w| w == cl),
                None => false,
            },
            Simple::Id(idv) => attr_val(attrs, "id") == Some(idv.as_str()),
            Simple::Attr { name: an, value } => match value {
                None => attr_val(attrs, an).is_some(),
                Some(v) => attr_val(attrs, an) == Some(v.as_str()),
            },
        };
        if !ok {
            return false;
        }
    }
    true
}

fn parent_element(dom: &Dom, id: NodeId) -> Option<NodeId> {
    dom.nodes[id]
        .parent
        .filter(|&p| matches!(dom.nodes[p].data, NodeData::Element { .. }))
}

/// Match the selector against the element (right-to-left evaluation).
pub fn matches(dom: &Dom, el: NodeId, sel: &Selector) -> bool {
    if sel.compounds.is_empty() {
        return false;
    }
    let last = sel.compounds.len() - 1;
    if !compound_matches(dom, el, &sel.compounds[last]) {
        return false;
    }
    let mut node = el;
    for i in (0..sel.combs.len()).rev() {
        match sel.combs[i] {
            Comb::Child => match parent_element(dom, node) {
                Some(p) if compound_matches(dom, p, &sel.compounds[i]) => node = p,
                _ => return false,
            },
            Comb::Desc => {
                let mut found = None;
                let mut p = parent_element(dom, node);
                while let Some(a) = p {
                    if compound_matches(dom, a, &sel.compounds[i]) {
                        found = Some(a);
                        break;
                    }
                    p = parent_element(dom, a);
                }
                match found {
                    Some(a) => node = a,
                    None => return false,
                }
            }
        }
    }
    true
}

/// Highest specificity among the selectors that match (the rule applies
/// with the specificity of its most specific matching selector).
fn best_specificity(dom: &Dom, el: NodeId, sels: &[Selector]) -> Option<(u32, u32, u32)> {
    let mut best: Option<(u32, u32, u32)> = None;
    for s in sels {
        if matches(dom, el, s) && best.map_or(true, |b| s.spec > b) {
            best = Some(s.spec);
        }
    }
    best
}

// ---------------- cascade -> STYLE ----------------

#[derive(Debug, Clone)]
pub struct StyledElem {
    pub index: usize, // element ordinal in document order (0-based)
    pub path: String, // element names from the root, '>'-joined
    pub decls: Vec<(String, String)>, // own cascaded winners, alphabetical
    pub inherited: Vec<(String, String)>, // values received from ancestors
}

#[derive(Debug, Default)]
pub struct StyleApply {
    pub elements: Vec<StyledElem>, // elements with >=1 own or inherited value
    pub styled: usize,             // elements with >=1 own cascaded property
    pub props: usize,              // total own cascaded properties
    pub inline_errors: usize,      // parse errors of inline style attributes
    pub inherited: usize,          // total inherited (~) values materialized
    pub inherited_elements: usize, // elements carrying >=1 inherited value
    // stage 10 (STAGE10_VARS): var()/custom properties, honest counters
    pub vars_defined: usize,      // own custom properties that computed OK
    pub var_refs: usize,          // var( calls encountered during substitution
    pub var_fallback_used: usize, // substitutions that fell back
    pub var_unresolved: usize,    // var( refs with no value and no usable fallback
    pub var_cycles: usize,        // custom props invalidated by reference cycles
}

/// Apply the cascade (author origin) plus inheritance to every element.
/// Cascade winner per property = max over the key
/// (important, inline, specificity, source order), applied ascending.
/// Inheritance: for every `is_inherited` property not set on the element
/// itself, the parent's computed value propagates (CSS: inherited value =
/// the parent's computed value); no initial values exist in this subset.
pub fn apply(dom: &Dom, rules: &[Rule]) -> StyleApply {
    let mut ap = StyleApply::default();
    let mut path: Vec<String> = Vec::new();
    // computed inherited maps along the current element path
    let mut maps: Vec<BTreeMap<String, String>> = Vec::new();
    // (id, entering); enter marker pushed after the exit marker like dom::dump
    let mut stack: Vec<(NodeId, bool)> = dom.nodes[0]
        .children
        .iter()
        .rev()
        .map(|&c| (c, true))
        .collect();
    let mut elem_index = 0usize;
    while let Some((id, entering)) = stack.pop() {
        if entering {
            let is_elem = matches!(dom.nodes[id].data, NodeData::Element { .. });
            if is_elem {
                let name = match &dom.nodes[id].data {
                    NodeData::Element { name, .. } => name.clone(),
                    _ => String::new(),
                };
                path.push(name);
                let index = elem_index;
                elem_index += 1;
                let parent = maps.last().cloned().unwrap_or_default();
                let computed = style_element(dom, id, index, &path, rules, &mut ap, &parent);
                maps.push(computed);
            }
            for &c in dom.nodes[id].children.iter().rev() {
                stack.push((c, false));
                stack.push((c, true));
            }
        } else if matches!(dom.nodes[id].data, NodeData::Element { .. }) {
            path.pop();
            maps.pop();
        }
    }
    ap
}

type CascadeKey = (u8, u8, u32, u32, u32, usize);

// ---------------- stage 10: var() substitution ----------------

/// Substitution recursion cap: CSS has no fixed depth limit, but a
/// deterministic engine needs one; beyond it the value is treated as
/// unresolvable (counted in var_unresolved, never a panic).
pub const MAX_VAR_DEPTH: usize = 32;

#[derive(Default)]
struct VarStats {
    refs: usize,
    fallback_used: usize,
    unresolved: usize,
}

fn var_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c >= '\u{80}'
}

/// True when `chars[i..]` starts a `var(` function call: the four chars
/// match and the preceding char is not an identifier character (so
/// `-var(`, `foovar(` do not count).
fn is_var_call(chars: &[char], i: usize) -> bool {
    if i + 4 > chars.len() {
        return false;
    }
    if chars[i] != 'v' || chars[i + 1] != 'a' || chars[i + 2] != 'r' || chars[i + 3] != '(' {
        return false;
    }
    if i > 0 && var_ident_char(chars[i - 1]) {
        return false;
    }
    true
}

/// For a `var(` starting at index `v` (chars[v] == 'v'):
/// (inner_start, inner_end, close_paren_index). `None` = unbalanced.
fn var_call_span(chars: &[char], v: usize) -> Option<(usize, usize, usize)> {
    let mut depth = 0i32;
    let mut j = v + 3; // at '('
    while j < chars.len() {
        match chars[j] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((v + 4, j, j));
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Split the inside of var() at the FIRST top-level comma:
/// (custom-name, Option<fallback>). Trailing/leading ws trimmed.
fn split_var_inner(inner: &[char]) -> Option<(String, Option<String>)> {
    let mut depth = 0i32;
    for k in 0..inner.len() {
        match inner[k] {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                let name: String = inner[..k].iter().collect();
                let fb: String = inner[k + 1..].iter().collect();
                return Some((name.trim().to_string(), Some(fb.trim().to_string())));
            }
            _ => {}
        }
    }
    let name: String = inner.iter().collect();
    Some((name.trim().to_string(), None))
}

fn valid_custom_name(s: &str) -> bool {
    let cs: Vec<char> = s.chars().collect();
    cs.len() >= 3
        && cs[0] == '-'
        && cs[1] == '-'
        && cs[2..].iter().all(|&c| var_ident_char(c))
}

/// Names referenced by var() calls inside `value` (top level or nested).
fn var_ref_names(value: &str) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if is_var_call(&chars, i) {
            if let Some((ins, ine, _)) = var_call_span(&chars, i) {
                if let Some((name, _fb)) = split_var_inner(&chars[ins..ine]) {
                    out.push(name);
                }
                i = ins; // keep scanning inside the call for nested var()
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Substitute every var() in `value` against `vars`.
/// - name has a value -> splice the (already resolved) value;
/// - name unknown, fallback present -> splice the fallback (it may itself
///   contain var() -> resolved recursively);
/// - name unknown, no fallback -> the whole value is invalid: None.
fn resolve_value(
    value: &str,
    vars: &BTreeMap<String, String>,
    st: &mut VarStats,
    depth: usize,
) -> Option<String> {
    if depth > MAX_VAR_DEPTH {
        st.unresolved += 1;
        return None;
    }
    let chars: Vec<char> = value.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0usize;
    let mut replaced = false;
    while i < chars.len() {
        if is_var_call(&chars, i) {
            if let Some((ins, ine, close)) = var_call_span(&chars, i) {
                st.refs += 1;
                let (name, fallback) = split_var_inner(&chars[ins..ine])?;
                if !valid_custom_name(&name) {
                    st.unresolved += 1;
                    return None;
                }
                match vars.get(&name) {
                    Some(v) => {
                        out.extend(v.chars());
                        replaced = true;
                        i = close + 1;
                        continue;
                    }
                    None => match fallback {
                        Some(fb) => {
                            st.fallback_used += 1;
                            let r = resolve_value(&fb, vars, st, depth + 1)?;
                            out.extend(r.chars());
                            replaced = true;
                            i = close + 1;
                            continue;
                        }
                        None => {
                            st.unresolved += 1;
                            return None;
                        }
                    },
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    if !replaced {
        return Some(chars.iter().collect());
    }
    let s: String = out.iter().collect();
    if s.contains("var(") {
        // a fallback spliced text that itself references vars
        resolve_value(&s, vars, st, depth + 1)
    } else {
        Some(s)
    }
}

fn style_element(
    dom: &Dom,
    id: NodeId,
    index: usize,
    path: &[String],
    rules: &[Rule],
    ap: &mut StyleApply,
    parent_inherited: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut entries: Vec<(CascadeKey, &Decl)> = Vec::new();
    for r in rules {
        if let Some(spec) = best_specificity(dom, id, &r.selectors) {
            for d in &r.decls {
                entries.push(((d.important as u8, 0, spec.0, spec.1, spec.2, r.order), d));
            }
        }
    }
    // inline style attribute: the user principal's own declarations
    let inline_decls: Vec<Decl>;
    if let NodeData::Element { attrs, .. } = &dom.nodes[id].data {
        if let Some(style_attr) = attr_val(attrs, "style") {
            let (ds, errs) = parse_declarations(style_attr);
            ap.inline_errors += errs;
            let mut keep = Vec::new();
            for d in ds {
                keep.push(d);
            }
            inline_decls = keep;
            for d in &inline_decls {
                entries.push(((d.important as u8, 1, 0, 0, 0, usize::MAX), d));
            }
        }
    }
    // start from the parent's computed values; own winners of inherited
    // properties override them, everything else propagates unchanged
    let mut computed = parent_inherited.clone();
    if entries.is_empty() {
        if !computed.is_empty() {
            let inherited: Vec<(String, String)> =
                computed.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            ap.inherited += inherited.len();
            ap.inherited_elements += 1;
            ap.elements.push(StyledElem {
                index,
                path: path.join(">"),
                decls: Vec::new(),
                inherited,
            });
        }
        return computed;
    }
    entries.sort_by_key(|e| e.0);
    let mut m: BTreeMap<&str, &str> = BTreeMap::new();
    for (_, d) in &entries {
        m.insert(d.name.as_str(), d.value.as_str());
    }

    // ---- stage 10 (STAGE10_VARS): custom properties + var() ----
    let mut st = VarStats::default();
    // environment: parent's computed custom properties first
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in parent_inherited.iter() {
        if k.starts_with("--") {
            vars.insert(k.clone(), v.clone());
        }
    }
    // own custom-property winners (raw values, may reference vars)
    let own_custom: Vec<(String, String)> = m
        .iter()
        .filter(|(k, _)| k.starts_with("--"))
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    // fixpoint: a prop resolves once every var() it references is either a
    // parent var or an already-resolved own prop; props stuck referencing
    // each other never resolve -> reference cycles (counted, invalidated)
    let mut resolved_own: BTreeMap<String, String> = BTreeMap::new();
    let mut broken: BTreeSet<String> = BTreeSet::new();
    loop {
        let mut progress = false;
        for (name, raw) in &own_custom {
            if resolved_own.contains_key(name) || broken.contains(name) {
                continue;
            }
            let pending_ref = var_ref_names(raw)
                .iter()
                .any(|r| own_custom.iter().any(|(n, _)| n == r) && !resolved_own.contains_key(r));
            if pending_ref {
                continue; // wait for the referenced own prop
            }
            match resolve_value(raw, &vars, &mut st, 0) {
                Some(v) => {
                    resolved_own.insert(name.clone(), v.clone());
                    // the env grows immediately: chains --b: var(--a) resolve
                    // in the same pass once --a is resolved
                    vars.insert(name.clone(), v);
                    progress = true;
                }
                None => {
                    broken.insert(name.clone());
                    progress = true;
                }
            }
        }
        if !progress || resolved_own.len() + broken.len() == own_custom.len() {
            break;
        }
    }
    for (name, _) in &own_custom {
        if !resolved_own.contains_key(name) && !broken.contains(name) {
            ap.var_cycles += 1; // stuck in a reference cycle -> invalid
        }
    }
    ap.vars_defined += resolved_own.len();
    for (name, v) in &resolved_own {
        vars.insert(name.clone(), v.clone());
        // children inherit the RESOLVED value (computed-value time)
        computed.insert(name.clone(), v.clone());
    }
    // normal winners: substitute var(); unresolved -> declaration invalid
    // at computed-value time -> dropped (an inherited property naturally
    // falls back to the parent's value already sitting in `computed`)
    let mut decls: Vec<(String, String)> = Vec::new();
    for (k, v) in &m {
        if k.starts_with("--") {
            continue; // custom props: only the resolved ones survive
        }
        if v.contains("var(") {
            if let Some(rv) = resolve_value(v, &vars, &mut st, 0) {
                decls.push(((*k).to_string(), rv));
            }
        } else {
            decls.push(((*k).to_string(), (*v).to_string()));
        }
    }
    for (name, v) in &resolved_own {
        decls.push((name.clone(), v.clone()));
    }
    decls.sort();
    // surviving own winners override the inherited environment for children
    for (k, v) in &decls {
        if is_inherited(k) {
            computed.insert(k.clone(), v.clone());
        }
    }
    ap.var_refs += st.refs;
    ap.var_fallback_used += st.fallback_used;
    ap.var_unresolved += st.unresolved;
    let own_names: BTreeSet<&str> = decls.iter().map(|(k, _)| k.as_str()).collect();
    let inherited: Vec<(String, String)> = computed
        .iter()
        .filter(|(k, _)| !own_names.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    ap.props += decls.len();
    ap.styled += 1;
    ap.inherited += inherited.len();
    if !inherited.is_empty() {
        ap.inherited_elements += 1;
    }
    ap.elements.push(StyledElem {
        index,
        path: path.join(">"),
        decls,
        inherited,
    });
    computed
}

/// Frozen dump format (machine-verified by tests):
/// `STYLE v2` header; one `E<index> <path>` line per styled element in
/// document order; two-space indented own `name: value` lines (alphabetical)
/// followed by two-space indented `~name: value` lines for values received
/// from ancestors (alphabetical). Elements with neither own nor inherited
/// values are absent.
pub fn dump(ap: &StyleApply) -> String {
    let mut s = String::from("STYLE v2\n");
    for e in &ap.elements {
        s.push_str(&format!("E{} {}\n", e.index, e.path));
        for (k, v) in &e.decls {
            s.push_str(&format!("  {}: {}\n", k, v));
        }
        for (k, v) in &e.inherited {
            s.push_str(&format!("  ~{}: {}\n", k, v));
        }
    }
    s
}

// ---------------- inheritance ----------------

/// Frozen subset of properties whose computed value inherits to descendants
/// (CSS 2.1 text/font/list/table basics). Everything else does NOT inherit
/// in this engine, and there are no initial values: an inherited property
/// with no ancestor value simply stays absent (documented subset).
pub fn is_inherited(name: &str) -> bool {
    // CSS custom properties always inherit (their whole point is to be
    // read by descendants through var()).
    if name.starts_with("--") {
        return true;
    }
    matches!(
        name,
        "color"
            | "direction"
            | "font"
            | "font-family"
            | "font-size"
            | "font-style"
            | "font-variant"
            | "font-weight"
            | "letter-spacing"
            | "line-height"
            | "list-style"
            | "list-style-image"
            | "list-style-position"
            | "list-style-type"
            | "quotes"
            | "text-align"
            | "text-indent"
            | "text-transform"
            | "visibility"
            | "white-space"
            | "word-spacing"
            | "border-collapse"
            | "border-spacing"
            | "caption-side"
            | "empty-cells"
            | "cursor"
            | "orphans"
            | "widows"
    )
}

// ---------------- sheet collection (document order) ----------------

/// A stylesheet source descriptor collected from the DOM. `Inline` carries
/// the raw text of one `<style>` element; `Link` carries the (possibly
/// empty) href attribute of a `<link rel="stylesheet">` element. Fetching
/// `Link` sheets is I/O and happens in main.rs — this module stays pure.
#[derive(Debug, Clone, PartialEq)]
pub enum SheetRef {
    Inline(String),
    Link { href: String },
}

#[derive(Debug, Default)]
pub struct SheetList {
    pub sheets: Vec<SheetRef>, // document order
    pub style_elems: usize,    // <style> elements found
    pub link_elems: usize,     // <link rel=stylesheet> elements found
}

/// A link is auto-applied when its rel token list contains "stylesheet"
/// (case-insensitive) and does not contain "alternate" (alternate
/// stylesheets are user-selected, never auto-applied — documented subset).
fn is_stylesheet_link(attrs: &[(String, String)]) -> bool {
    let Some(rel) = attr_val(attrs, "rel") else {
        return false;
    };
    let toks: Vec<&str> = rel.split_ascii_whitespace().collect();
    toks.iter().any(|t| t.eq_ignore_ascii_case("stylesheet"))
        && !toks.iter().any(|t| t.eq_ignore_ascii_case("alternate"))
}

/// Collect stylesheet sources in document order. `<style>` yields its raw
/// text children concatenated; `<link rel="stylesheet">` yields a Link
/// descriptor (empty href when the attribute is missing — the caller counts
/// and reports it as a failure). `media`/`disabled` attributes are ignored:
/// every collected sheet participates (documented subset).
pub fn collect_sheets(dom: &Dom) -> SheetList {
    let mut out = SheetList::default();
    let mut stack: Vec<NodeId> = dom.nodes[0].children.iter().rev().copied().collect();
    while let Some(id) = stack.pop() {
        if let NodeData::Element { name, attrs, .. } = &dom.nodes[id].data {
            if name == "style" {
                out.style_elems += 1;
                let mut text = String::new();
                for &c in &dom.nodes[id].children {
                    if let NodeData::Text(t) = &dom.nodes[c].data {
                        text.push_str(t);
                    }
                }
                out.sheets.push(SheetRef::Inline(text));
                continue; // raw text content: no traversal inside
            }
            if name == "link" && is_stylesheet_link(attrs) {
                out.link_elems += 1;
                let href = attr_val(attrs, "href").unwrap_or("").to_string();
                out.sheets.push(SheetRef::Link { href });
                continue; // void element
            }
        }
        for &c in dom.nodes[id].children.iter().rev() {
            stack.push(c);
        }
    }
    out
}

// ---------------- tests ----------------

#[cfg(test)]
mod tests {
    use super::*;

    fn first_tok(input: &str) -> Tok {
        tokenize(input).0.remove(0).t
    }

    #[test]
    fn tokenizer_basics() {
        let (t, e) = tokenize("a.b#c[x=\"1\"] { color: red }");
        assert_eq!(e, 0);
        let kinds: Vec<&Tok> = t.iter().map(|x| &x.t).collect();
        assert_eq!(
            kinds,
            vec![
                &Tok::Ident("a".into()),
                &Tok::Delim('.'),
                &Tok::Ident("b".into()),
                &Tok::Hash("c".into()),
                &Tok::LBracket,
                &Tok::Ident("x".into()),
                &Tok::Delim('='),
                &Tok::Str("1".into()),
                &Tok::RBracket,
                &Tok::Ws,
                &Tok::LBrace,
                &Tok::Ws,
                &Tok::Ident("color".into()),
                &Tok::Colon,
                &Tok::Ws,
                &Tok::Ident("red".into()),
                &Tok::Ws,
                &Tok::RBrace,
            ]
        );
    }

    #[test]
    fn tokenizer_strings_and_escapes() {
        assert_eq!(first_tok(r#""hi\" there""#), Tok::Str("hi\" there".into()));
        assert_eq!(first_tok(r#""\41x""#), Tok::Str("Ax".into())); // hex escape
        assert_eq!(first_tok(r#""\A9""#), Tok::Str("\u{a9}".into()));
        assert_eq!(tokenize("\"a\nb\"").0[0].t, Tok::BadStr); // newline in string
        assert_eq!(tokenize("\"a").1, 1); // EOF in string
        assert_eq!(tokenize("/*c*/a").0.len(), 1); // comments produce no tokens
        assert_eq!(tokenize("/*c").1, 1); // unterminated comment
    }

    #[test]
    fn tokenizer_url_numbers_functions() {
        assert_eq!(first_tok("url(a/b.png)"), Tok::Url("a/b.png".into()));
        assert_eq!(first_tok("url( x )"), Tok::Url("x".into()));
        let (t, e) = tokenize("url(a b)");
        assert_eq!(t[0].t, Tok::BadUrl);
        assert_eq!(e, 1);
        let (t, _) = tokenize("url('x')"); // quoted -> function call
        let kinds: Vec<&Tok> = t.iter().map(|x| &x.t).collect();
        assert_eq!(
            kinds,
            vec![
                &Tok::Ident("url".into()),
                &Tok::LParen,
                &Tok::Str("x".into()),
                &Tok::RParen,
            ]
        );
        assert_eq!(first_tok("12px"), Tok::Dim("12".into(), "px".into()));
        assert_eq!(first_tok("50%"), Tok::Percent("50".into()));
        assert_eq!(first_tok("-3.5e2"), Tok::Num("-3.5e2".into()));
        assert_eq!(first_tok(".5em"), Tok::Dim(".5".into(), "em".into()));
        let (t, _) = tokenize("rgb(1,2,3)");
        let kinds: Vec<&Tok> = t.iter().map(|x| &x.t).collect();
        assert_eq!(
            kinds,
            vec![
                &Tok::Function("rgb".into()),
                &Tok::Num("1".into()),
                &Tok::Comma,
                &Tok::Num("2".into()),
                &Tok::Comma,
                &Tok::Num("3".into()),
                &Tok::RParen,
            ]
        );
        assert_eq!(first_tok("@media"), Tok::AtKeyword("media".into()));
        assert_eq!(first_tok("@"), Tok::Delim('@'));
        assert_eq!(first_tok("-"), Tok::Delim('-'));
        assert_eq!(first_tok("--x"), Tok::Ident("--x".into()));
    }

    #[test]
    fn parse_rules_and_decls() {
        let o = parse("p, .x { color: red; font-size : 12px }");
        assert_eq!(o.errors, 0);
        assert_eq!(o.rules.len(), 1);
        assert_eq!(o.rules[0].selectors.len(), 2);
        assert_eq!(o.rules[0].selectors[0].spec, (0, 0, 1));
        assert_eq!(o.rules[0].selectors[1].spec, (0, 1, 0));
        assert_eq!(o.rules[0].decls.len(), 2);
        assert_eq!(o.rules[0].decls[0].name, "color");
        assert_eq!(o.rules[0].decls[0].value, "red");
        assert_eq!(o.rules[0].decls[1].value, "12px");
    }

    #[test]
    fn parse_important_and_at_rules() {
        let o = parse("#a { color: red !important; color: blue ! IMPORTANT }");
        assert_eq!(o.errors, 0);
        assert_eq!(o.rules[0].decls.len(), 2);
        assert!(o.rules[0].decls[0].important);
        assert_eq!(o.rules[0].decls[0].value, "red");
        assert!(o.rules[0].decls[1].important);
        assert_eq!(o.rules[0].decls[1].value, "blue");

        let o = parse("@media screen { p { color: blue } } p { color: red }");
        assert_eq!(o.at_rules_skipped, 1);
        assert_eq!(o.rules.len(), 1);
        assert_eq!(o.rules[0].decls[0].value, "red");
        assert_eq!(o.errors, 0);

        // @import without a block is consumed too
        let o = parse("@import url(x.css); p { y: 1 }");
        assert_eq!(o.at_rules_skipped, 1);
        assert_eq!(o.rules.len(), 1);
    }

    #[test]
    fn parse_error_recovery() {
        let o = parse("a { color: }"); // empty value
        assert_eq!(o.decl_count(), 0);
        assert_eq!(o.errors, 1);

        let o = parse("a { color: red !x }"); // bad !important
        assert_eq!(o.decl_count(), 0);
        assert_eq!(o.errors, 1);

        let o = parse("a { color red; x: 1 }"); // missing colon -> skipped
        assert_eq!(o.decl_count(), 1);
        assert_eq!(o.errors, 1);

        let o = parse("a { color: red; b { x: y } }"); // nested rule -> error, skipped
        assert_eq!(o.decl_count(), 1);
        assert_eq!(o.errors, 1);

        let o = parse("p { color: red"); // EOF inside the block
        assert_eq!(o.decl_count(), 1);
        assert_eq!(o.errors, 1);

        let o = parse("p:hover { color: red }"); // pseudo-class unsupported
        assert_eq!(o.rules.len(), 0);
        assert_eq!(o.errors, 1);

        let o = parse("p ~ q { color: red }"); // sibling combinator unsupported
        assert_eq!(o.rules.len(), 0);
        assert_eq!(o.errors, 1);

        let o = parse("p { color: red } }"); // stray brace
        assert_eq!(o.rules.len(), 1);
        assert_eq!(o.errors, 1);
    }

    #[test]
    fn selector_specificity_and_shapes() {
        let o = parse("div p.x#a [data-b] { y: 1 }");
        assert_eq!(o.rules[0].selectors[0].spec, (1, 2, 2));
        let o = parse("* { y: 1 }");
        assert_eq!(o.rules[0].selectors[0].spec, (0, 0, 0));
        let o = parse("[data-x=\"1\"] { y: 2 }");
        assert_eq!(o.rules[0].selectors[0].spec, (0, 1, 0));
        assert_eq!(
            o.rules[0].selectors[0].compounds[0].parts[0],
            Simple::Attr {
                name: "data-x".into(),
                value: Some("1".into())
            }
        );
        let o = parse("p > q .z { y: 3 }");
        assert_eq!(o.rules[0].selectors[0].combs, vec![Comb::Child, Comb::Desc]);
        // PARSER property names are case-insensitive; class names are not
        let o = parse("p { COLOR: Red }");
        assert_eq!(o.rules[0].decls[0].name, "color");
        assert_eq!(o.rules[0].decls[0].value, "Red");
        let o = parse("p.X { y: 1 } p.x { y: 2 }");
        assert_eq!(o.rules[0].selectors[0].compounds[0].parts[1], Simple::Class("X".into()));
        assert_eq!(o.rules[1].selectors[0].compounds[0].parts[1], Simple::Class("x".into()));
    }

    #[test]
    fn cascade_specificity_order_important() {
        let doc = crate::dom::build(r#"<p class="x" id="y">t</p>"#);
        let o = parse("p { color: red } .x { color: green } #y { color: blue } p.x { color: black }");
        let ap = apply(&doc.dom, &o.rules);
        assert_eq!(ap.styled, 1);
        let color = ap.elements[0]
            .decls
            .iter()
            .find(|(k, _)| k == "color")
            .unwrap();
        // (1,0,0) id wins over class/type
        assert_eq!(color.1, "blue");

        let doc = crate::dom::build(r#"<p class="x">t</p>"#);
        let o = parse("p { color: red } .x { color: green }");
        let ap = apply(&doc.dom, &o.rules);
        let color = ap.elements[0].decls.iter().find(|(k, _)| k == "color").unwrap();
        assert_eq!(color.1, "green"); // class beats type

        // equal specificity -> later source order wins
        let o = parse("p { color: red } p { color: green }");
        let ap = apply(&doc.dom, &o.rules);
        let color = ap.elements[0].decls.iter().find(|(k, _)| k == "color").unwrap();
        assert_eq!(color.1, "green");
    }

    #[test]
    fn inline_style_and_important() {
        let doc = crate::dom::build(r#"<div id="a"><p style="color: black">x</p></div>"#);
        // inline beats sheet normal
        let o = parse("p { color: red }");
        let ap = apply(&doc.dom, &o.rules);
        let p = &ap.elements.iter().find(|e| e.index == 4).unwrap();
        assert_eq!(p.decls.iter().find(|(k, _)| k == "color").unwrap().1, "black");
        // sheet important beats inline normal
        let o = parse("#a p { color: green !important }");
        let ap = apply(&doc.dom, &o.rules);
        let p = &ap.elements.iter().find(|e| e.index == 4).unwrap();
        assert_eq!(p.decls.iter().find(|(k, _)| k == "color").unwrap().1, "green");
    }

    #[test]
    fn matching_descendant_child_class_split() {
        let doc = crate::dom::build(r#"<div><section class="a b"><p>x</p></section></div>"#);
        let o = parse("div .a p { y: 1 }"); // descendant chain
        assert_eq!(apply(&doc.dom, &o.rules).styled, 1);
        let o = parse("div > p { y: 1 }"); // p is not a direct child of div
        assert_eq!(apply(&doc.dom, &o.rules).styled, 0);
        let o = parse("section > p { y: 1 }");
        assert_eq!(apply(&doc.dom, &o.rules).styled, 1);
        let o = parse(".b { y: 1 }"); // class list split on whitespace
        assert_eq!(apply(&doc.dom, &o.rules).styled, 1);
        let o = parse("[class=\"a\"] { y: 1 }"); // exact attr value, no split
        assert_eq!(apply(&doc.dom, &o.rules).styled, 0);
    }

    #[test]
    fn dump_format_frozen() {
        let doc = crate::dom::build(r#"<p style="color: red">x</p>"#);
        let o = parse("p { margin: 0 }");
        let ap = apply(&doc.dom, &o.rules);
        assert_eq!(
            dump(&ap),
            "STYLE v2\nE3 html>body>p\n  color: red\n  margin: 0\n"
        );
    }

    #[test]
    fn collect_sheets_order_and_rel() {
        let doc = crate::dom::build(
            "<style>a{x:1}</style><link rel=\"stylesheet\" href=\"/a.css\">\
             <style>b{y:2}</style><link REL=\"STYLESHEET\" href=\"/b.css\">\
             <link rel=\"alternate stylesheet\" href=\"/c.css\">\
             <link rel=\"stylesheet\"><link href=\"/d.css\">",
        );
        let l = collect_sheets(&doc.dom);
        assert_eq!(l.style_elems, 2);
        assert_eq!(l.link_elems, 3); // a, b and the href-less one; c (alternate) and d (no rel) do not match
        assert_eq!(l.sheets.len(), 5);
        assert_eq!(l.sheets[0], SheetRef::Inline("a{x:1}".into()));
        assert_eq!(l.sheets[1], SheetRef::Link { href: "/a.css".into() });
        assert_eq!(l.sheets[2], SheetRef::Inline("b{y:2}".into()));
        assert_eq!(l.sheets[3], SheetRef::Link { href: "/b.css".into() });
        assert_eq!(l.sheets[4], SheetRef::Link { href: "".into() });
    }

    #[test]
    fn is_inherited_table() {
        for p in [
            "color",
            "font",
            "font-size",
            "line-height",
            "text-align",
            "visibility",
            "white-space",
            "cursor",
        ] {
            assert!(is_inherited(p));
        }
        for p in ["margin", "border", "padding", "width", "display", "background"] {
            assert!(!is_inherited(p));
        }
    }

    #[test]
    fn inheritance_basic_and_dump_v2() {
        let doc =
            crate::dom::build("<html><body><div><p>x<em>e</em></p></div><span>s</span>");
        let o = parse(
            "body { color: green } div > p { text-align: center } em { border-width: 3px }",
        );
        let ap = apply(&doc.dom, &o.rules);
        // body and p and em have own decls; div/p/em/span carry inherited ones
        assert_eq!(ap.styled, 3);
        assert_eq!(ap.props, 3);
        assert_eq!(ap.inherited, 5);
        assert_eq!(ap.inherited_elements, 4);
        assert_eq!(
            dump(&ap),
            concat!(
                "STYLE v2\n",
                "E2 html>body\n",
                "  color: green\n",
                "E3 html>body>div\n",
                "  ~color: green\n",
                "E4 html>body>div>p\n",
                "  text-align: center\n",
                "  ~color: green\n",
                "E5 html>body>div>p>em\n",
                "  border-width: 3px\n",
                "  ~color: green\n",
                "  ~text-align: center\n",
                "E6 html>body>span\n",
                "  ~color: green\n",
            )
        );
    }

    #[test]
    fn inheritance_override_and_non_inherited() {
        // the child sees the nearest computed value (override in the middle)
        let doc = crate::dom::build("<div><p>x<span>s</span></p></div>");
        let o = parse("div { color: red } p { color: blue }");
        let ap = apply(&doc.dom, &o.rules);
        let span = &ap
            .elements
            .iter()
            .find(|e| e.path.ends_with("span"))
            .unwrap();
        assert_eq!(
            span.inherited
                .iter()
                .find(|(k, _)| k == "color")
                .unwrap()
                .1,
            "blue"
        );
        // non-inherited properties never propagate
        let doc = crate::dom::build("<div><p>x</p></div>");
        let o = parse("div { margin: 1px }");
        let ap = apply(&doc.dom, &o.rules);
        assert_eq!(ap.styled, 1);
        assert_eq!(ap.inherited, 0);
        assert_eq!(ap.inherited_elements, 0);
        assert_eq!(ap.elements.len(), 1);
    }

    #[test]
    fn inheritance_deep_chain_no_overflow() {
        let n = 500;
        let mut s = String::from("<html><body>");
        for _ in 0..n {
            s.push_str("<div>");
        }
        for _ in 0..n {
            s.push_str("</div>");
        }
        s.push_str("</body></html>");
        let doc = crate::dom::build(&s);
        let o = parse("html { color: teal }");
        let ap = apply(&doc.dom, &o.rules);
        // head + body + 500 divs carry the inherited color
        assert_eq!(ap.styled, 1);
        assert_eq!(ap.inherited, n + 2);
        assert_eq!(ap.inherited_elements, n + 2);
    }

    #[test]
    fn deep_tree_no_overflow() {
        let n = 500;
        let mut s = String::new();
        for _ in 0..n {
            s.push_str("<div>");
        }
        for _ in 0..n {
            s.push_str("</div>");
        }
        let doc = crate::dom::build(&s);
        let o = parse("div { color: red }");
        let ap = apply(&doc.dom, &o.rules);
        assert_eq!(ap.styled, n);
        let o = parse("div div div { color: red }"); // deep descendant chains
        let ap = apply(&doc.dom, &o.rules);
        assert_eq!(ap.styled, n - 2);
    }

    // ---------------- stage 10: var() + custom properties ----------------

    fn apply_style(src: &str, css_text: &str) -> StyleApply {
        let doc = crate::dom::build(src);
        let o = parse(css_text);
        apply(&doc.dom, &o.rules)
    }

    fn own_value<'a>(ap: &'a StyleApply, idx: usize, name: &str) -> Option<&'a str> {
        // dom::build synthesizes html/head/body -> for "<div><p>..</p></div>"
        // the div is E3 and the p is E4 (see STYLE dump contract)
        let e = ap.elements.iter().find(|e| e.index == idx)?;
        e.decls
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn var_basic_substitution_and_inheritance() {
        // custom prop defined on :root-ish first div, used by a child
        let src = "<div><p>x</p></div>";
        let ap = apply_style(
            src,
            "div { --accent: #ff0000; color: var(--accent) } p { color: var(--accent) }",
        );
        // div (E0) resolves its own use; p (E1) resolves via inheritance
        assert_eq!(own_value(&ap, 3, "color"), Some("#ff0000"));
        assert_eq!(own_value(&ap, 4, "color"), Some("#ff0000"));
        assert_eq!(ap.vars_defined, 1);
        assert_eq!(ap.var_refs, 2);
        assert_eq!(ap.var_unresolved, 0);
        assert_eq!(ap.var_fallback_used, 0);
        assert_eq!(ap.var_cycles, 0);
    }

    #[test]
    fn var_custom_name_is_case_sensitive() {
        let ap = apply_style(
            "<p>x</p>",
            "p { --Acc: blue; color: var(--Acc) }",
        );
        assert_eq!(own_value(&ap, 3, "color"), Some("blue"));
        assert_eq!(own_value(&ap, 3, "--Acc"), Some("blue"));
        // a different-case lookup must NOT resolve
        let ap2 = apply_style("<p>x</p>", "p { --Acc: blue; color: var(--acc) }");
        assert_eq!(own_value(&ap2, 3, "color"), None);
        assert_eq!(ap2.var_unresolved, 1);
    }

    #[test]
    fn var_fallback_used_when_missing() {
        let ap = apply_style("<p>x</p>", "p { color: var(--nope, #123456) }");
        assert_eq!(own_value(&ap, 3, "color"), Some("#123456"));
        assert_eq!(ap.var_fallback_used, 1);
        assert_eq!(ap.var_unresolved, 0);
    }

    #[test]
    fn var_unresolved_drops_declaration_parent_value_survives() {
        // parent sets color; child references an undefined var with no
        // fallback -> the child's declaration is dropped and the inherited
        // parent value keeps flowing (CSS: unset == inherit here)
        let src = "<div><p>x</p></div>";
        let ap = apply_style(src, "div { color: #00ff00 } p { color: var(--gone) }");
        assert_eq!(own_value(&ap, 3, "color"), Some("#00ff00"));
        assert_eq!(own_value(&ap, 4, "color"), None); // own winner dropped
        let e1 = ap.elements.iter().find(|e| e.index == 4).unwrap();
        assert!(e1.inherited.iter().any(|(k, v)| k == "color" && v == "#00ff00"));
        assert_eq!(ap.var_unresolved, 1);
    }

    #[test]
    fn var_cycle_invalidates_both_props() {
        let ap = apply_style(
            "<p>x</p>",
            "p { --a: var(--b); --b: var(--a); color: var(--a) }",
        );
        assert_eq!(ap.var_cycles, 2); // both custom props invalid
        assert_eq!(own_value(&ap, 3, "--a"), None);
        assert_eq!(own_value(&ap, 3, "--b"), None);
        assert_eq!(own_value(&ap, 3, "color"), None); // use of invalid var
        assert_eq!(ap.var_unresolved, 1); // the color use
    }

    #[test]
    fn var_self_reference_is_cycle() {
        let ap = apply_style("<p>x</p>", "p { --a: var(--a) }");
        assert_eq!(ap.var_cycles, 1);
        assert_eq!(own_value(&ap, 3, "--a"), None);
    }

    #[test]
    fn var_chain_resolves_in_order() {
        let ap = apply_style(
            "<p>x</p>",
            "p { --a: red; --b: var(--a); --c: var(--b); color: var(--c) }",
        );
        assert_eq!(own_value(&ap, 3, "--c"), Some("red"));
        assert_eq!(own_value(&ap, 3, "color"), Some("red"));
        assert_eq!(ap.var_refs, 3);
        assert_eq!(ap.var_unresolved, 0);
    }

    #[test]
    fn var_nested_fallback_resolves() {
        let ap = apply_style(
            "<p>x</p>",
            "p { color: var(--x, var(--y, green)); background-color: var(--y) }",
        );
        assert_eq!(own_value(&ap, 3, "color"), Some("green"));
        assert_eq!(own_value(&ap, 3, "background-color"), None); // no fallback
        assert_eq!(ap.var_fallback_used, 2); // outer miss + inner miss
        assert_eq!(ap.var_unresolved, 1);
    }

    #[test]
    fn var_multi_token_value_splices() {
        let ap = apply_style(
            "<p>x</p>",
            "p { --w: 120px; width: var(--w); }",
        );
        assert_eq!(own_value(&ap, 3, "width"), Some("120px"));
    }

    #[test]
    fn var_bad_name_is_unresolved() {
        // var(bare) without -- is invalid per CSS
        let ap = apply_style("<p>x</p>", "p { color: var(bare) }");
        assert_eq!(own_value(&ap, 3, "color"), None);
        assert_eq!(ap.var_unresolved, 1);
    }

    #[test]
    fn var_no_var_no_refs() {
        let ap = apply_style("<p>x</p>", "p { color: red }");
        assert_eq!(ap.var_refs, 0);
        assert_eq!(ap.vars_defined, 0);
        // foovar( and -var( are not var() calls
        let ap2 = apply_style("<p>x</p>", "p { --a: foovar(--b) }");
        assert_eq!(own_value(&ap2, 3, "--a"), Some("foovar(--b)"));
        assert_eq!(ap2.var_refs, 0);
    }

    #[test]
    fn var_inline_style_uses_pipeline() {
        let src = "<p style=\"--local: purple; color: var(--local)\">x</p>";
        let ap = apply_style(src, "p { }");
        assert_eq!(own_value(&ap, 3, "color"), Some("purple"));
        assert_eq!(ap.vars_defined, 1);
    }
}


