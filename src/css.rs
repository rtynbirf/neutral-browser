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
//!   source order); no inheritance, no shorthand expansion, no var()
//! - a lone "-" is a Delim; a number immediately followed by "-" does not
//!   become a dimension (custom-property units are not supported)

use crate::dom::{Dom, NodeData, NodeId};
use std::collections::BTreeMap;

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
                let nm = nm.to_ascii_lowercase();
                i += 1;
                nm
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
    pub decls: Vec<(String, String)>, // alphabetically sorted by name
}

#[derive(Debug, Default)]
pub struct StyleApply {
    pub elements: Vec<StyledElem>, // only elements with >=1 declaration
    pub styled: usize,
    pub props: usize,
    pub inline_errors: usize, // parse errors of inline style attributes
}

/// Apply the cascade (author origin) to every element of the tree.
/// Winner per property = max over the key
/// (important, inline, specificity, source order), applied ascending.
pub fn apply(dom: &Dom, rules: &[Rule]) -> StyleApply {
    let mut ap = StyleApply::default();
    let mut path: Vec<String> = Vec::new();
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
                style_element(dom, id, index, &path, rules, &mut ap);
            }
            for &c in dom.nodes[id].children.iter().rev() {
                stack.push((c, false));
                stack.push((c, true));
            }
        } else if matches!(dom.nodes[id].data, NodeData::Element { .. }) {
            path.pop();
        }
    }
    ap
}

type CascadeKey = (u8, u8, u32, u32, u32, usize);

fn style_element(
    dom: &Dom,
    id: NodeId,
    index: usize,
    path: &[String],
    rules: &[Rule],
    ap: &mut StyleApply,
) {
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
    if entries.is_empty() {
        return;
    }
    entries.sort_by_key(|e| e.0);
    let mut m: BTreeMap<&str, &str> = BTreeMap::new();
    for (_, d) in &entries {
        m.insert(d.name.as_str(), d.value.as_str());
    }
    let decls: Vec<(String, String)> = m
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    ap.props += decls.len();
    ap.styled += 1;
    ap.elements.push(StyledElem {
        index,
        path: path.join(">"),
        decls,
    });
}

/// Frozen dump format (machine-verified by tests):
/// `STYLE v1` header; one `E<index> <path>` line per styled element in
/// document order; two-space indented `name: value` lines (alphabetical).
pub fn dump(ap: &StyleApply) -> String {
    let mut s = String::from("STYLE v1\n");
    for e in &ap.elements {
        s.push_str(&format!("E{} {}\n", e.index, e.path));
        for (k, v) in &e.decls {
            s.push_str(&format!("  {}: {}\n", k, v));
        }
    }
    s
}

/// Concatenated text of all `<style>` elements (document order, joined
/// with newlines) + their count. External stylesheets are NOT fetched.
pub fn collect_style_text(dom: &Dom) -> (String, usize) {
    let mut parts: Vec<String> = Vec::new();
    let mut count = 0;
    let mut stack: Vec<NodeId> = dom.nodes[0].children.iter().rev().copied().collect();
    while let Some(id) = stack.pop() {
        if let NodeData::Element { name, .. } = &dom.nodes[id].data {
            if name == "style" {
                count += 1;
                for &c in &dom.nodes[id].children {
                    if let NodeData::Text(t) = &dom.nodes[c].data {
                        parts.push(t.clone());
                    }
                }
                continue; // raw text content: no traversal inside
            }
        }
        for &c in dom.nodes[id].children.iter().rev() {
            stack.push(c);
        }
    }
    (parts.join("\n"), count)
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
            "STYLE v1\nE3 html>body>p\n  color: red\n  margin: 0\n"
        );
    }

    #[test]
    fn collect_style_text_order() {
        let doc = crate::dom::build("<style>p{color:red}</style><style>.x{y:1}</style>");
        let (t, n) = collect_style_text(&doc.dom);
        assert_eq!(n, 2);
        assert_eq!(t, "p{color:red}\n.x{y:1}");
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
}


