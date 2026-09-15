//! DOM arena + HTML tree construction — a documented subset of the WHATWG
//! HTML Standard tree construction algorithm (§13.2.6).
//!
//! The tree builder drives the tokenizer's raw modes (script/style -> RAWTEXT,
//! title/textarea -> RCDATA, plaintext -> PLAINTEXT) through TokControl,
//! exactly as the standard prescribes. A self-closing start tag (non-void)
//! keeps the element open and never triggers a raw mode (spec: the flag is
//! acknowledged-and-ignored for HTML elements).
//!
//! Implemented insertion modes: initial, before html, before head, in head,
//! after head, in body, text, after body, after after body. Includes implied
//! html/head/body, void elements, `<p>` auto-closing for block starts,
//! li/dd/dt auto-closing (simplified scope checks), `</br>` -> `<br>`,
//! `image` -> `img`, the LF skip after pre/textarea/listing, first-wins
//! attribute dedup, ASCII-lowercased names, text-node merging, and
//! error recovery that never aborts (HTML error handling is
//! normative-recoverable).
//!
//! Documented subset decisions (NOT hallucinated "full support"):
//! - Foster parenting and the in-table insertion modes are NOT implemented:
//!   table/tbody/tr/td are inserted as ordinary elements (`<table><tr>`
//!   parses without the implied `<tbody>` — a known deviation).
//! - The adoption agency algorithm / active formatting elements are NOT
//!   implemented: mis-nested formatting elements stay literally nested.
//! - Foreign content (SVG/MathML) is NOT implemented; `<svg>`/`<math>` are
//!   ordinary HTML elements.
//! - The template element has no special insertion modes.
//! - Scope checks (p/li/dd/dt) use the standard terminator lists; the full
//!   "generate implied end tags" bookkeeping is simplified.
//! - Attribute merging on repeated `<html>`/`<body>` tags is not performed.

use crate::html::{self, RawMode, TokControl, Token};

pub type NodeId = usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeData {
    Document,
    Doctype(String),
    Element {
        name: String,
        attrs: Vec<(String, String)>, // names lowercased, first occurrence wins
    },
    Text(String),
    Comment(String),
}

#[derive(Debug)]
pub struct Node {
    pub data: NodeData,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

#[derive(Debug)]
pub struct Dom {
    pub nodes: Vec<Node>,
}

impl Dom {
    fn new() -> Self {
        Dom {
            nodes: vec![Node {
                data: NodeData::Document,
                parent: None,
                children: Vec::new(),
            }],
        }
    }

    fn create(&mut self, data: NodeData) -> NodeId {
        self.nodes.push(Node {
            data,
            parent: None,
            children: Vec::new(),
        });
        self.nodes.len() - 1
    }

    /// Append `id` to `parent`'s children. Adjacent text is merged
    /// (observable equivalent of the spec appending to the text node's data).
    fn append(&mut self, parent: NodeId, id: NodeId) {
        // take the text out (clone) to avoid overlapping borrows of nodes[]
        let text = match self.nodes[id].data.clone() {
            NodeData::Text(s) => s,
            _ => {
                self.nodes[id].parent = Some(parent);
                self.nodes[parent].children.push(id);
                return;
            }
        };
        let last = self.nodes[parent].children.last().copied();
        if let Some(last) = last {
            if let NodeData::Text(prev) = &mut self.nodes[last].data {
                prev.push_str(&text);
                return;
            }
        }
        self.nodes[id].parent = Some(parent);
        self.nodes[parent].children.push(id);
    }

    fn name_of(&self, id: NodeId) -> &str {
        match &self.nodes[id].data {
            NodeData::Element { name, .. } => name,
            _ => "",
        }
    }

    fn elem_name(name: &str, attrs: &[(String, String)]) -> NodeData {
        // first occurrence of each attribute name wins (spec)
        let mut seen: Vec<(String, String)> = Vec::new();
        for (k, v) in attrs {
            if !seen.iter().any(|(k2, _)| k2 == k) {
                seen.push((k.clone(), v.clone()));
            }
        }
        NodeData::Element {
            name: name.to_string(),
            attrs: seen,
        }
    }

    /// Canonical dump (frozen format — machine-verified by tests):
    /// `#document` at depth 0; two spaces per level; `<!DOCTYPE name>`;
    /// `<name k="v">` with `&`->`&amp;`, `"`->`&quot;` in attribute values;
    /// `"text"` with `\`->`\\`, `"`->`\"`, `\n`/`\t`/`\r` backslash-escaped;
    /// `<!--data-->` raw. Iterative (no recursion) — deep trees are safe.
    pub fn dump(&self) -> String {
        let mut out = String::from("#document\n");
        // (id, depth, entering); flat format — no closing lines
        let mut stack: Vec<(NodeId, usize, bool)> = Vec::new();
        let mut tops: Vec<NodeId> = self.nodes[0].children.clone();
        tops.reverse();
        for t in tops {
            stack.push((t, 1, true));
            stack.push((t, 1, false));
        }
        while let Some((id, depth, entering)) = stack.pop() {
            if !entering {
                continue;
            }
            let indent = "  ".repeat(depth);
            match &self.nodes[id].data {
                NodeData::Document => {}
                NodeData::Doctype(n) => {
                    out.push_str(&indent);
                    if n.is_empty() {
                        out.push_str("<!DOCTYPE>\n");
                    } else {
                        out.push_str(&format!("<!DOCTYPE {}>\n", n));
                    }
                }
                NodeData::Element { name, attrs } => {
                    out.push_str(&indent);
                    out.push('<');
                    out.push_str(name);
                    for (k, v) in attrs {
                        out.push(' ');
                        out.push_str(k);
                        out.push_str("=\"");
                        for ch in v.chars() {
                            match ch {
                                '&' => out.push_str("&amp;"),
                                '"' => out.push_str("&quot;"),
                                c => out.push(c),
                            }
                        }
                        out.push('"');
                    }
                    out.push_str(">\n");
                    let mut kids = self.nodes[id].children.clone();
                    kids.reverse();
                    for k in kids {
                        stack.push((k, depth + 1, true));
                        stack.push((k, depth + 1, false));
                    }
                }
                NodeData::Text(s) => {
                    out.push_str(&indent);
                    out.push('"');
                    for ch in s.chars() {
                        match ch {
                            '\\' => out.push_str("\\\\"),
                            '"' => out.push_str("\\\""),
                            '\n' => out.push_str("\\n"),
                            '\t' => out.push_str("\\t"),
                            '\r' => out.push_str("\\r"),
                            c => out.push(c),
                        }
                    }
                    out.push_str("\"\n");
                }
                NodeData::Comment(s) => {
                    out.push_str(&indent);
                    out.push_str("<!--");
                    out.push_str(s);
                    out.push_str("-->\n");
                }
            }
        }
        out
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        // iterative DFS from the document node
        let mut stack: Vec<(NodeId, usize)> = vec![(0, 0)];
        while let Some((id, depth)) = stack.pop() {
            c.depth = c.depth.max(depth);
            match &self.nodes[id].data {
                NodeData::Document => c.nodes += 1,
                NodeData::Doctype(_) => {
                    c.nodes += 1;
                    if c.doctype.is_none() {
                        c.doctype = Some(match &self.nodes[id].data {
                            NodeData::Doctype(n) => n.clone(),
                            _ => String::new(),
                        });
                    }
                }
                NodeData::Element { .. } => {
                    c.nodes += 1;
                    c.elements += 1;
                }
                NodeData::Text(_) => {
                    c.nodes += 1;
                    c.texts += 1;
                }
                NodeData::Comment(_) => {
                    c.nodes += 1;
                    c.comments += 1;
                }
            }
            for &k in &self.nodes[id].children {
                stack.push((k, depth + 1));
            }
        }
        c
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Counts {
    pub nodes: usize,
    pub elements: usize,
    pub texts: usize,
    pub comments: usize,
    pub depth: usize,
    pub doctype: Option<String>,
}

pub struct ParseResult {
    pub dom: Dom,
    pub errors: usize,
    pub kinds: Vec<&'static str>, // distinct tree-level kinds, cap 8
}

const VOID: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input",
    "keygen", "link", "meta", "param", "source", "track", "wbr",
];

const CLOSE_P: &[&str] = &[
    "address", "article", "aside", "blockquote", "center", "details", "dialog", "dir", "div",
    "dl", "fieldset", "figcaption", "figure", "footer", "header", "hgroup", "main", "menu",
    "nav", "ol", "p", "section", "summary", "ul",
];

const SCOPE_TERM: &[&str] = &[
    "applet", "caption", "html", "table", "td", "th", "marquee", "object", "template",
];

const BUTTON_TERM: &[&str] = &[
    "applet", "caption", "html", "table", "td", "th", "marquee", "object", "template", "button",
];

const LIST_ITEM_TERM: &[&str] = &[
    "applet", "caption", "html", "table", "td", "th", "marquee", "object", "template", "ol", "ul",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Initial,
    BeforeHtml,
    BeforeHead,
    InHead,
    AfterHead,
    InBody,
    Text,
    AfterBody,
    AfterAfterBody,
}

struct TreeBuilder {
    dom: Dom,
    stack: Vec<NodeId>,
    mode: Mode,
    orig_mode: Mode,
    frame: NodeId, // raw-mode frame element (for Text mode)
    body_open: bool,
    skip_lf: bool,
    errors: usize,
    kinds: Vec<&'static str>,
}

impl TreeBuilder {
    fn new() -> Self {
        TreeBuilder {
            dom: Dom::new(),
            stack: Vec::new(),
            mode: Mode::Initial,
            orig_mode: Mode::InBody,
            frame: 0,
            body_open: false,
            skip_lf: false,
            errors: 0,
            kinds: Vec::new(),
        }
    }

    fn err(&mut self, kind: &'static str) {
        self.errors += 1;
        if !self.kinds.contains(&kind) && self.kinds.len() < 8 {
            self.kinds.push(kind);
        }
    }

    fn current(&self) -> NodeId {
        *self.stack.last().unwrap_or(&0)
    }

    fn append_data(&mut self, data: NodeData) -> NodeId {
        let cur = self.current();
        let id = self.dom.create(data);
        self.dom.append(cur, id);
        id
    }

    /// Append a text/comment token to the DOCUMENT node (initial modes).
    fn append_to_doc(&mut self, tok: Token) {
        match tok {
            Token::Text(s) => {
                let id = self.dom.create(NodeData::Text(s));
                self.dom.append(0, id);
            }
            Token::Comment(s) => {
                let id = self.dom.create(NodeData::Comment(s));
                self.dom.append(0, id);
            }
            _ => unreachable!("append_to_doc only for text/comment"),
        }
    }

    fn insert_element(&mut self, name: &str, attrs: &[(String, String)]) -> NodeId {
        let id = self.append_data(Dom::elem_name(name, attrs));
        self.stack.push(id);
        id
    }

    fn is_void(name: &str) -> bool {
        VOID.contains(&name)
    }

    fn has_in_scope(&self, name: &str, terms: &[&str]) -> bool {
        for &id in self.stack.iter().rev() {
            let n = self.dom.name_of(id);
            if n == name {
                return true;
            }
            if terms.contains(&n) {
                return false;
            }
        }
        false
    }

    fn pop_through(&mut self, name: &str) {
        while let Some(id) = self.stack.pop() {
            if self.dom.name_of(id) == name {
                break;
            }
        }
    }

    fn close_p(&mut self) {
        if self.has_in_scope("p", BUTTON_TERM) {
            self.pop_through("p");
        }
    }

    /// Pop through the implied `</head>` (stray open elements above head are
    /// popped with it — documented simplification).
    fn pop_head(&mut self) {
        while let Some(id) = self.stack.pop() {
            if self.dom.name_of(id) == "head" {
                break;
            }
        }
    }

    fn implied_body(&mut self) {
        self.insert_element("body", &[]);
        self.body_open = true;
        self.mode = Mode::InBody;
    }

    fn enter_text_mode(&mut self, id: NodeId, name: &str, raw: RawMode, ctl: &mut TokControl) {
        self.frame = id;
        self.orig_mode = Mode::InBody;
        self.mode = Mode::Text;
        ctl.switch = Some((name.to_string(), raw));
    }

    fn feed(&mut self, tok: Token, ctl: &mut TokControl) {
        self.step(tok, ctl);
    }

    fn step(&mut self, tok: Token, ctl: &mut TokControl) {
        match self.mode {
            Mode::Initial => match tok {
                Token::Text(s) if s.chars().all(|c| c.is_ascii_whitespace()) => {}
                Token::Comment(_) => self.append_to_doc(tok),
                Token::Doctype { name } => {
                    let id = self.dom.create(NodeData::Doctype(name));
                    self.dom.append(0, id);
                    self.mode = Mode::BeforeHtml;
                }
                other => {
                    self.mode = Mode::BeforeHtml;
                    self.feed(other, ctl);
                }
            },
            Mode::BeforeHtml => match tok {
                Token::Text(s) if s.chars().all(|c| c.is_ascii_whitespace()) => {}
                Token::Comment(_) => self.append_to_doc(tok),
                Token::StartTag { name, attrs, .. } if name == "html" => {
                    self.insert_element("html", &attrs);
                    self.mode = Mode::BeforeHead;
                }
                Token::EndTag { name } if name == "br" => {
                    // spec: </br> acts as "anything else" (implied html)
                    self.insert_element("html", &[]);
                    self.mode = Mode::BeforeHead;
                    self.feed(Token::EndTag { name }, ctl);
                }
                Token::EndTag { .. } => self.err("UNEXPECTED_END_TAG"),
                other => {
                    self.insert_element("html", &[]); // implied
                    self.mode = Mode::BeforeHead;
                    self.feed(other, ctl);
                }
            },
            Mode::BeforeHead => match tok {
                Token::Text(s) if s.chars().all(|c| c.is_ascii_whitespace()) => {}
                Token::Comment(s) => {
                    self.append_data(NodeData::Comment(s));
                }
                Token::StartTag { name, attrs, .. } if name == "head" => {
                    self.insert_element("head", &attrs);
                    self.mode = Mode::InHead;
                }
                Token::EndTag { name } if name == "br" => {
                    // spec: </br> acts as "anything else" (implied head)
                    self.insert_element("head", &[]);
                    self.mode = Mode::InHead;
                    self.feed(Token::EndTag { name }, ctl);
                }
                Token::EndTag { .. } => self.err("UNEXPECTED_END_TAG"),
                other => {
                    self.insert_element("head", &[]); // implied
                    self.mode = Mode::InHead;
                    self.feed(other, ctl);
                }
            },
            Mode::InHead => match tok {
                Token::Text(s) if s.chars().all(|c| c.is_ascii_whitespace()) => {
                    self.append_data(NodeData::Text(s));
                }
                Token::Text(_) => {
                    // implied </head>, reprocess in after head (implies body)
                    self.pop_head();
                    self.mode = Mode::AfterHead;
                    self.feed(tok, ctl);
                }
                Token::Comment(s) => {
                    self.append_data(NodeData::Comment(s));
                }
                Token::Doctype { .. } => self.err("UNEXPECTED_DOCTYPE"),
                Token::StartTag { name, attrs, .. }
                    if name == "base" || name == "basefont" || name == "bgsound"
                        || name == "link" || name == "meta" =>
                {
                    // void elements of head: appended, never pushed
                    let _ = self.append_data(Dom::elem_name(&name, &attrs));
                }
                Token::StartTag { name, attrs, self_closing } if name == "title" => {
                    let id = self.insert_element("title", &attrs);
                    if !self_closing {
                        self.orig_mode = Mode::InHead;
                        self.mode = Mode::Text;
                        self.frame = id;
                        ctl.switch = Some(("title".into(), RawMode::Rc));
                    }
                }
                Token::StartTag { name, attrs, self_closing } if name == "script" => {
                    let id = self.insert_element("script", &attrs);
                    if !self_closing {
                        self.orig_mode = Mode::InHead;
                        self.mode = Mode::Text;
                        self.frame = id;
                        ctl.switch = Some(("script".into(), RawMode::Raw));
                    }
                }
                Token::StartTag { name, attrs, self_closing } if name == "style" => {
                    let id = self.insert_element("style", &attrs);
                    if !self_closing {
                        self.orig_mode = Mode::InHead;
                        self.mode = Mode::Text;
                        self.frame = id;
                        ctl.switch = Some(("style".into(), RawMode::Raw));
                    }
                }
                Token::StartTag { name, attrs, .. }
                    if name == "noscript" || name == "template" =>
                {
                    // documented subset: ordinary elements, no special modes
                    let _ = self.insert_element(&name, &attrs);
                }
                Token::StartTag { name, .. } if name == "head" => self.err("UNEXPECTED_TAG"),
                Token::EndTag { name } if name == "head" => {
                    self.stack.pop();
                    self.mode = Mode::AfterHead;
                }
                // any other end tag: spec "anything else" (implied </head>,
                // reprocess in after head)
                Token::EndTag { name } => {
                    self.pop_head();
                    self.mode = Mode::AfterHead;
                    self.feed(Token::EndTag { name }, ctl);
                }
                other => {
                    self.pop_head();
                    self.mode = Mode::AfterHead;
                    self.feed(other, ctl);
                }
            },
            Mode::AfterHead => match tok {
                Token::Text(s) if s.chars().all(|c| c.is_ascii_whitespace()) => {
                    self.append_data(NodeData::Text(s));
                }
                Token::Comment(s) => {
                    self.append_data(NodeData::Comment(s));
                }
                Token::Doctype { .. } => self.err("UNEXPECTED_DOCTYPE"),
                Token::StartTag { name, attrs, .. } if name == "body" => {
                    self.insert_element("body", &attrs);
                    self.body_open = true;
                    self.mode = Mode::InBody;
                }
                Token::StartTag { name, .. } if name == "head" => self.err("UNEXPECTED_TAG"),
                // any end tag: spec "anything else" (implied body, reprocess)
                Token::EndTag { name } => {
                    self.implied_body();
                    self.feed(Token::EndTag { name }, ctl);
                }
                other => {
                    self.implied_body();
                    self.feed(other, ctl);
                }
            },
            Mode::Text => match tok {
                Token::Text(s) => {
                    let s = if self.skip_lf {
                        self.skip_lf = false;
                        s.strip_prefix('\n').map(|x| x.to_string()).unwrap_or(s)
                    } else {
                        s
                    };
                    if !s.is_empty() {
                        let cur = self.frame;
                        let id = self.dom.create(NodeData::Text(s));
                        self.dom.append(cur, id);
                    }
                }
                Token::EndTag { name } if name == self.dom.name_of(self.frame) => {
                    self.stack.pop();
                    self.mode = self.orig_mode;
                }
                Token::EndTag { .. } => self.err("UNEXPECTED_END_TAG"),
                _ => self.err("UNEXPECTED_TAG"),
            },
            Mode::InBody => match tok {
                Token::Text(s) => {
                    let s = if self.skip_lf {
                        self.skip_lf = false;
                        s.strip_prefix('\n').map(|x| x.to_string()).unwrap_or(s)
                    } else {
                        s
                    };
                    if !s.is_empty() {
                        self.append_data(NodeData::Text(s));
                    }
                }
                Token::Comment(s) => {
                    self.append_data(NodeData::Comment(s));
                }
                Token::Doctype { .. } => self.err("UNEXPECTED_DOCTYPE"),
                Token::StartTag { name, .. }
                    if name == "html" || name == "body" || name == "head" =>
                {
                    self.err("UNEXPECTED_TAG")
                }
                Token::StartTag { name, attrs, .. } if Self::is_void(&name) => {
                    if name == "br" || name == "hr" {
                        self.close_p();
                    }
                    let _ = self.append_data(Dom::elem_name(&name, &attrs));
                }
                Token::StartTag { name, attrs, .. } if name == "image" => {
                    // spec legacy: `image` becomes `img`
                    let _ = self.append_data(Dom::elem_name("img", &attrs));
                }
                Token::StartTag { name, attrs, .. } if CLOSE_P.contains(&name.as_str()) => {
                    self.close_p();
                    let _ = self.insert_element(&name, &attrs);
                }
                Token::StartTag { name, attrs, .. }
                    if name == "h1" || name == "h2" || name == "h3" || name == "h4"
                        || name == "h5" || name == "h6" =>
                {
                    self.close_p();
                    let cur_name = self.dom.name_of(self.current()).to_string();
                    if matches!(cur_name.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
                        self.err("UNEXPECTED_TAG");
                        self.stack.pop();
                    }
                    let _ = self.insert_element(&name, &attrs);
                }
                Token::StartTag { name, attrs, .. } if name == "pre" || name == "listing" => {
                    self.close_p();
                    let _ = self.insert_element(&name, &attrs);
                    self.skip_lf = true;
                }
                Token::StartTag { name, attrs, .. } if name == "form" => {
                    // form pointer NOT tracked (documented subset)
                    self.close_p();
                    let _ = self.insert_element("form", &attrs);
                }
                Token::StartTag { name, attrs, .. } if name == "li" => {
                    if self.has_in_scope("li", LIST_ITEM_TERM) {
                        self.pop_through("li");
                    }
                    self.close_p();
                    let _ = self.insert_element("li", &attrs);
                }
                Token::StartTag { name, attrs, .. } if name == "dd" || name == "dt" => {
                    if self.has_in_scope("dd", SCOPE_TERM) {
                        self.pop_through("dd");
                    }
                    if self.has_in_scope("dt", SCOPE_TERM) {
                        self.pop_through("dt");
                    }
                    self.close_p();
                    let _ = self.insert_element(&name, &attrs);
                }
                Token::StartTag { name, attrs, self_closing } if name == "textarea" => {
                    self.close_p();
                    let id = self.insert_element("textarea", &attrs);
                    self.skip_lf = true;
                    if !self_closing {
                        self.enter_text_mode(id, "textarea", RawMode::Rc, ctl);
                    }
                }
                Token::StartTag { name, attrs, self_closing } if name == "xmp" => {
                    self.close_p();
                    let id = self.insert_element("xmp", &attrs);
                    if !self_closing {
                        self.enter_text_mode(id, "xmp", RawMode::Raw, ctl);
                    }
                }
                Token::StartTag { name, attrs, self_closing }
                    if name == "iframe" || name == "noembed" || name == "noframes" =>
                {
                    let id = self.insert_element(&name, &attrs);
                    if !self_closing {
                        self.enter_text_mode(id, &name, RawMode::Raw, ctl);
                    }
                }
                Token::StartTag { name, attrs, self_closing } if name == "script" => {
                    let id = self.insert_element("script", &attrs);
                    if !self_closing {
                        self.orig_mode = Mode::InBody;
                        self.mode = Mode::Text;
                        self.frame = id;
                        ctl.switch = Some(("script".into(), RawMode::Raw));
                    }
                }
                Token::StartTag { name, attrs, self_closing } if name == "title" => {
                    let id = self.insert_element("title", &attrs);
                    if !self_closing {
                        self.enter_text_mode(id, "title", RawMode::Rc, ctl);
                    }
                }
                Token::StartTag { name, attrs, self_closing } if name == "style" => {
                    let id = self.insert_element("style", &attrs);
                    if !self_closing {
                        self.enter_text_mode(id, "style", RawMode::Raw, ctl);
                    }
                }
                Token::StartTag { name, attrs, .. } if name == "plaintext" => {
                    self.close_p();
                    let _ = self.insert_element("plaintext", &attrs);
                    // PLAINTEXT: never leaves; remaining input is text
                    ctl.switch = Some(("plaintext".into(), RawMode::Plain));
                }
                Token::StartTag { name, attrs, .. }
                    if name == "table" || name == "tbody" || name == "thead" || name == "tfoot"
                        || name == "tr" || name == "td" || name == "th" || name == "caption"
                        || name == "col" || name == "colgroup" =>
                {
                    // documented subset: NO foster parenting, NO in-table modes
                    self.close_p();
                    let _ = self.insert_element(&name, &attrs);
                }
                Token::StartTag { name, attrs, .. } => {
                    // ordinary element (incl. svg/math as HTML — documented)
                    let _ = self.insert_element(&name, &attrs);
                }
                Token::EndTag { name } if name == "br" => {
                    // spec: </br> acts as <br>
                    self.close_p();
                    let _ = self.append_data(Dom::elem_name("br", &[]));
                }
                Token::EndTag { name } if name == "p" => {
                    if !self.has_in_scope("p", BUTTON_TERM) {
                        self.err("UNEXPECTED_END_TAG");
                        // spec: insert an implied <p>, then close it
                        let _ = self.insert_element("p", &[]);
                    }
                    self.pop_through("p");
                }
                Token::EndTag { name } if name == "body" => {
                    if self.body_open && self.has_in_scope("body", SCOPE_TERM) {
                        self.mode = Mode::AfterBody;
                    } else {
                        self.err("UNEXPECTED_END_TAG");
                    }
                }
                Token::EndTag { name } if name == "html" => {
                    if self.body_open && self.has_in_scope("body", SCOPE_TERM) {
                        self.mode = Mode::AfterBody;
                        self.feed(Token::EndTag { name }, ctl);
                    } else {
                        self.err("UNEXPECTED_END_TAG");
                    }
                }
                Token::EndTag { name } => {
                    // generic: pop through the matching open element, if any
                    if self.stack.iter().rev().any(|&id| self.dom.name_of(id) == name) {
                        self.pop_through(&name);
                    } else {
                        self.err("UNEXPECTED_END_TAG");
                    }
                }
            },
            Mode::AfterBody => match tok {
                Token::Text(s) if s.chars().all(|c| c.is_ascii_whitespace()) => {
                    // documented subset: dropped (spec reprocesses via in body)
                    let _ = s;
                }
                Token::Comment(s) => {
                    self.append_data(NodeData::Comment(s));
                }
                Token::EndTag { name } if name == "html" => self.mode = Mode::AfterAfterBody,
                other => {
                    self.mode = Mode::InBody;
                    self.feed(other, ctl);
                }
            },
            Mode::AfterAfterBody => match tok {
                Token::Text(s) if s.chars().all(|c| c.is_ascii_whitespace()) => {}
                Token::Comment(_) => self.append_to_doc(tok),
                Token::Doctype { .. } => self.err("UNEXPECTED_DOCTYPE"),
                Token::EndTag { name } if name == "html" => self.err("UNEXPECTED_END_TAG"),
                other => {
                    self.mode = Mode::InBody;
                    self.feed(other, ctl);
                }
            },
        }
    }

    fn finish(&mut self) {
        if self.mode == Mode::Text {
            self.err("EOF_IN_RAWTEXT");
            self.stack.pop();
            self.mode = self.orig_mode;
        }
        // implied end tags: pop everything
        self.stack.clear();
    }
}

/// Parse an HTML document string into a DOM tree. Never panics on input;
/// parse errors are counted, not fatal (HTML error handling is
/// normative-recoverable).
pub fn build(input: &str) -> ParseResult {
    let mut tb = TreeBuilder::new();
    let errors = html::tokenize(input, &mut |tok, ctl| tb.feed(tok, ctl));
    tb.finish();
    ParseResult {
        dom: tb.dom,
        errors: errors + tb.errors,
        kinds: tb.kinds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump_of(input: &str) -> String {
        build(input).dom.dump()
    }

    #[test]
    fn implied_structure() {
        let d = dump_of("<p>hi</p>");
        assert_eq!(
            d,
            "#document\n  <html>\n    <head>\n    <body>\n      <p>\n        \"hi\"\n"
        );
    }

    #[test]
    fn full_document() {
        let d = dump_of(
            "<!doctype html><html><head><title>t</title></head><body><p>x</p></body></html>",
        );
        assert_eq!(
            d,
            "#document\n  <!DOCTYPE html>\n  <html>\n    <head>\n      <title>\n        \"t\"\n    <body>\n      <p>\n        \"x\"\n"
        );
    }

    #[test]
    fn p_autoclose_and_list() {
        let r = build("<div><p>one<p>two<ul><li>i<li>j</ul></div>");
        let d = r.dom.dump();
        assert!(d.contains("<div>"));
        assert!(d.contains("<ul>"));
        // p auto-closed: two p elements are siblings
        let c = r.dom.counts();
        assert_eq!(c.elements, 9); // html head body div p p ul li li
    }

    #[test]
    fn end_br_is_br() {
        let r = build("</br>x");
        let d = r.dom.dump();
        assert!(d.contains("<br>"), "dump={}", d);
        assert!(d.contains("\"x\""));
    }

    #[test]
    fn attrs_lowercase_first_wins() {
        let d = dump_of("<DIV CLASS=\"a\" class=\"b\" ID=1></DIV>");
        assert!(d.contains("<div class=\"a\" id=\"1\">"), "dump={}", d);
    }

    #[test]
    fn attr_escape_in_dump() {
        let d = dump_of("<p title=\"a&amp;b\">x</p>");
        assert!(d.contains("title=\"a&amp;b\""), "dump={}", d);
    }

    #[test]
    fn void_elements_not_pushed() {
        let r = build("<body><br><img src=x><input type=text>");
        // html head body br img input
        assert_eq!(r.dom.counts().elements, 6);
    }

    #[test]
    fn hr_closes_p() {
        let d = dump_of("<p>1<hr>2");
        // hr is a sibling of p, "2" goes to body
        assert!(d.contains("<body>\n      <p>\n        \"1\"\n      <hr>\n      \"2\""), "dump={}", d);
    }

    #[test]
    fn table_subset_no_foster_parenting() {
        let r = build("<table><tr><td>c</table>");
        let d = r.dom.dump();
        // subset: tr is a direct child of table (no implied tbody) — documented
        assert!(d.contains("<table>\n        <tr>"), "dump={}", d);
        assert_eq!(r.dom.counts().elements, 6); // html head body table tr td
    }

    #[test]
    fn script_rawtext_content() {
        let r = build("<script>if(a<b){x=\"</div>\"}</script>");
        let d = r.dom.dump();
        assert!(d.contains("\"if(a<b){x=\\\"</div>\\\"}\""), "dump={}", d);
    }

    #[test]
    fn comments() {
        let r = build("<!--c1--><p>a</p><!--c2-->");
        assert_eq!(r.dom.counts().comments, 2);
    }

    #[test]
    fn comment_before_html_at_document_level() {
        let d = dump_of("<!--top--><html></html>");
        assert!(d.starts_with("#document\n  <!--top-->"), "dump={}", d);
    }

    #[test]
    fn char_refs_in_text() {
        let d = dump_of("<p>a&amp;b &lt; &copy;</p>");
        assert!(d.contains("\"a&b < \u{A9}\""), "dump={}", d);
    }

    #[test]
    fn text_merging() {
        let r = build("<p>a&amp;b c</p>");
        // single text node despite the char ref split
        assert_eq!(r.dom.counts().texts, 1);
    }

    #[test]
    fn deep_nesting_no_overflow() {
        let n = 10_000;
        let mut s = String::new();
        for _ in 0..n {
            s.push_str("<div>");
        }
        for _ in 0..n {
            s.push_str("</div>");
        }
        let r = build(&s);
        let c = r.dom.counts();
        assert_eq!(c.elements, n + 3); // + html head body
        assert_eq!(c.depth, n + 2); // doc0 html1 body2 div#n at n+2
    }

    #[test]
    fn pre_skips_first_newline() {
        let d = dump_of("<pre>\nx</pre>");
        assert!(d.contains("\"x\""), "dump={}", d);
    }

    #[test]
    fn textarea_rcdata() {
        let d = dump_of("<textarea><b>not bold</b> &amp; </textarea>");
        assert!(d.contains("\"<b>not bold</b> & \""), "dump={}", d);
    }

    #[test]
    fn plaintext_forever() {
        let d = dump_of("<plaintext><b>x");
        assert!(d.contains("\"<b>x\""), "dump={}", d);
    }

    #[test]
    fn unmatched_end_tag_is_error_not_fatal() {
        let r = build("</div>ok");
        assert!(r.errors >= 1);
        assert!(r.kinds.contains(&"UNEXPECTED_END_TAG"));
        let d = r.dom.dump();
        assert!(d.contains("\"ok\""));
    }

    #[test]
    fn stray_end_table_pops_through() {
        let r = build("<table><tr><td>c</table>after");
        let d = r.dom.dump();
        assert!(d.contains("\"after\""), "dump={}", d);
        assert_eq!(r.dom.counts().elements, 6);
    }

    #[test]
    fn self_closing_div_is_still_open() {
        // spec: self-closing flag on non-void HTML elements is ignored
        let d = dump_of("<div/>x");
        assert!(d.contains("<div>\n        \"x\""), "dump={}", d);
    }

    #[test]
    fn self_closing_script_stays_open_no_rawtext() {
        // spec: `<script/>` inserts the element but does NOT switch tokenizer;
        // following text falls through implied head close into body
        let d = dump_of("<script/>ok");
        assert!(d.contains("<script>\n    <body>\n      \"ok\""), "dump={}", d);
    }

    #[test]
    fn doctype_name_recorded() {
        let r = build("<!DOCTYPE HTML><p>x</p>");
        assert_eq!(r.dom.counts().doctype, Some("html".into()));
    }

    #[test]
    fn counts_and_depth() {
        let r = build(
            "<!doctype html><html><head><title>t</title></head><body><!--c--><h1>h</h1><p>p</p></body></html>",
        );
        let c = r.dom.counts();
        assert_eq!(c.elements, 6); // html head title body h1 p
        assert_eq!(c.texts, 3); // t, h, p
        assert_eq!(c.comments, 1);
        assert_eq!(c.nodes, 12); // doc + doctype + 6 + 3 + 1
        assert_eq!(c.depth, 4); // doc0 html1 body2 h1 3 text4
        assert_eq!(c.doctype, Some("html".into()));
        assert_eq!(r.errors, 0);
    }

    #[test]
    fn whitespace_text_merged_into_body() {
        let r = build("<body>  \n  <p>x</p>");
        let c = r.dom.counts();
        assert_eq!(c.texts, 2); // ws text + "x"
    }

    #[test]
    fn eof_in_rawtext_is_error() {
        let r = build("<script>unclosed");
        assert!(r.kinds.contains(&"EOF_IN_RAWTEXT"));
        let d = r.dom.dump();
        assert!(d.contains("\"unclosed\""), "dump={}", d);
    }

    #[test]
    fn implied_head_body_from_fragment() {
        let r = build("hello");
        let c = r.dom.counts();
        assert_eq!(c.elements, 3); // html head body
        assert_eq!(c.texts, 1);
        assert_eq!(r.errors, 0);
    }

    #[test]
    fn title_in_head_via_rcdata() {
        let d = dump_of("<head><title>a &amp; b</title></head>");
        assert!(d.contains("\"a & b\""), "dump={}", d);
    }

    #[test]
    fn li_autoclose() {
        let d = dump_of("<ul><li>a<li>b</ul>");
        assert!(d.contains("<li>\n          \"a\""), "dump={}", d);
        let c = build("<ul><li>a<li>b</ul>").dom.counts();
        assert_eq!(c.elements, 6); // html head body ul li li
    }

    #[test]
    fn implied_p_from_stray_end_p() {
        // spec: </p> in body with no open p implies an empty <p>
        let d = dump_of("<body></p>x");
        assert!(d.contains("<p>\n"), "dump={}", d);
        assert!(d.contains("\"x\""));
    }
}
