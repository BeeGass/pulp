//! HTML to plain text.

use std::collections::HashMap;

use crate::error::Error;

/// Deepest element nesting handed to html2text.
///
/// html2text sizes its render tree recursively, so a page nested tens of
/// thousands deep overflows the stack and aborts the process, and html5ever
/// and [`Model`] walk the open elements for most tags. The deepest of 62,144
/// real pages (rustdoc, mdBook, kitty and Ghostty docs) nests 23. With
/// strikeout drawn as plain text, html2text renders any common element
/// nested 512 deep around 64 KiB of text in under 40 ms, so the limit bounds
/// the walks: scanning an 8 MiB page built to walk the whole stack on every
/// tag takes 0.6 s at 128 and 2.1 s at 512.
const MAX_HTML2TEXT_DEPTH: usize = 128;

/// Fewest input bytes per element html2text may be made to build.
///
/// html5ever reopens formatting elements that a block closed before every
/// later paragraph, so a page can make it build dozens of elements per tag:
/// an 800 KB page that reopens 64 `<b>`s takes html2text 41 s and 3.5 GB.
/// The densest of the real pages makes one element per 37 bytes, so one per
/// 4 leaves room nine times over, and a page that reopens even two elements
/// per paragraph passes it.
const MIN_BYTES_PER_ELEMENT: usize = 4;

/// Convert HTML bytes to readable text.
///
/// The bytes are decoded once, as [`super::text::decode_bytes`] decodes any
/// text, so UTF-16 and legacy encodings reach the parser as text. A page
/// that [`nesting`] finds deeper than [`MAX_HTML2TEXT_DEPTH`] has its tags
/// stripped in one linear pass; the rest goes to html2text (width 100), and
/// a page html2text rejects is stripped as well.
pub fn extract(bytes: &[u8]) -> Result<String, Error> {
    let html = super::text::decode_bytes(bytes);
    Ok(match nesting(html.as_bytes(), MAX_HTML2TEXT_DEPTH) {
        Nesting::Deep => strip_tags(&html),
        Nesting::Unsure => render_contained(bytes, &html),
        Nesting::Shallow => render(&html),
    })
}

/// html2text's text for `html`, or the stripped text when it fails.
fn render(html: &str) -> String {
    // Strikeout drawn with combining marks adds a character per letter for
    // every enclosing `<s>`, so nested strikeout would multiply the output.
    html2text::config::plain()
        .unicode_strikeout(false)
        .string_from_read(html.as_bytes(), 100)
        .unwrap_or_else(|_| strip_tags(html))
}

/// [`render`], in a child process when this one isolates extractors.
///
/// A page lands here when the parse model finds it shallow but the plain
/// count of open tags does not. Should the model have missed a way html5ever
/// nests the page, the child's timeout or crash costs one file, not this
/// process. The child renders the same bytes with the same code, so the text
/// is the same; if it fails, the page is stripped instead.
fn render_contained(bytes: &[u8], html: &str) -> String {
    #[cfg(not(target_arch = "wasm32"))]
    if super::isolate::should_isolate() {
        return super::isolate::extract_in_child(bytes, crate::classify::Kind::Html)
            .unwrap_or_else(|_| strip_tags(html));
    }
    #[cfg(target_arch = "wasm32")]
    let _ = bytes;
    render(html)
}

/// How deep a page nests, as far as [`nesting`] can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nesting {
    /// Within the limit by the parse model and by the plain count.
    Shallow,
    /// Within the limit by the parse model, past it by the plain count.
    Unsure,
    /// Past the limit by the parse model, or building more elements than
    /// [`MIN_BYTES_PER_ELEMENT`] allows.
    Deep,
}

/// Measure how deep `html` nests against `limit`, in one pass.
///
/// The parse model ([`Model`]) opens and closes elements by the rules
/// html5ever follows: implied end tags, scopes, the adoption agency,
/// reconstructed formatting elements, tables, and SVG and MathML. So it
/// counts the depth html2text will build, and it stops once that passes
/// `limit`. The plain count ([`OpenCount`]) cannot reason depth away, so it
/// backs the model up. Both read the same tokens, split as html5ever's
/// tokenizer splits them ([`Lexer`]).
fn nesting(html: &[u8], limit: usize) -> Nesting {
    let mut model = Model::new(html, limit);
    let mut plain = OpenCount::new();
    let mut lexer = Lexer::new(html);
    let mut text_from = 0;
    while let Some(tag) = lexer.next_tag(model.in_foreign()) {
        if tag.start > text_from {
            model.text(&html[text_from..tag.start]);
        }
        match tag.kind {
            TagKind::Start(start) => match model.start_tag(start) {
                Some(raw) => lexer.skip_raw(start.name, raw),
                None => plain.open(html, start),
            },
            TagKind::End(end) => {
                plain.close(html, end);
                model.end_tag(end);
            }
            TagKind::Doctype { html } => model.doctype(html),
            TagKind::Other => {}
        }
        if model.deep {
            return Nesting::Deep;
        }
        text_from = lexer.at;
    }
    if text_from < html.len() {
        model.text(&html[text_from..]);
    }
    if model.deep {
        Nesting::Deep
    } else if plain.max > limit {
        Nesting::Unsure
    } else {
        Nesting::Shallow
    }
}

/// The plain count: every start tag that is not void opens an element, and
/// an end tag closes one only when an element of its name is open. The most
/// that were open at once bounds the depth from above, whatever implied end
/// tags a parser applies, short of end tags it ignores.
struct OpenCount {
    /// Open elements by name, for names the model knows.
    known: [usize; EL_COUNT],
    /// Open elements by lowercased name, for every other name.
    other: HashMap<Vec<u8>, usize>,
    open: usize,
    max: usize,
}

impl OpenCount {
    fn new() -> Self {
        Self {
            known: [0; EL_COUNT],
            other: HashMap::new(),
            open: 0,
            max: 0,
        }
    }

    fn open(&mut self, html: &[u8], tag: StartTag) {
        if tag.el.is_void() {
            return;
        }
        *self.slot(html, tag.el, tag.name) += 1;
        self.open += 1;
        self.max = self.max.max(self.open);
    }

    fn close(&mut self, html: &[u8], tag: EndTag) {
        let slot = self.slot(html, tag.el, tag.name);
        if *slot > 0 {
            *slot -= 1;
            self.open -= 1;
        }
    }

    fn slot(&mut self, html: &[u8], el: El, name: (usize, usize)) -> &mut usize {
        if el == El::Other {
            let name = html[name.0..name.1].to_ascii_lowercase();
            self.other.entry(name).or_default()
        } else {
            &mut self.known[el as usize]
        }
    }
}

macro_rules! elements {
    ($($el:ident = $name:literal),* $(,)?) => {
        /// Element names the parse model tells apart. Every other name is
        /// `Other`, compared by its bytes.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum El {
            $($el,)*
            Other,
        }

        /// How many element names there are, `Other` included.
        const EL_COUNT: usize = [$(El::$el,)*].len() + 1;

        impl El {
            /// The element a lowercased tag name names.
            fn from_lowercase(name: &[u8]) -> El {
                match name {
                    $($name => El::$el,)*
                    _ => El::Other,
                }
            }
        }
    };
}

elements! {
    A = b"a", Address = b"address", AnnotationXml = b"annotation-xml", Applet = b"applet",
    Area = b"area", Article = b"article", Aside = b"aside", B = b"b", Base = b"base",
    Basefont = b"basefont", Bgsound = b"bgsound", Big = b"big", Blockquote = b"blockquote",
    Body = b"body", Br = b"br", Button = b"button", Caption = b"caption", Center = b"center",
    Code = b"code", Col = b"col", Colgroup = b"colgroup", Dd = b"dd", Desc = b"desc",
    Details = b"details", Dialog = b"dialog", Dir = b"dir", Div = b"div", Dl = b"dl", Dt = b"dt",
    Em = b"em", Embed = b"embed", Fieldset = b"fieldset", Figcaption = b"figcaption",
    Figure = b"figure", Font = b"font", Footer = b"footer", ForeignObject = b"foreignobject",
    Form = b"form", Frame = b"frame", Frameset = b"frameset", H1 = b"h1", H2 = b"h2", H3 = b"h3",
    H4 = b"h4", H5 = b"h5", H6 = b"h6", Head = b"head", Header = b"header", Hgroup = b"hgroup",
    Hr = b"hr", Html = b"html", I = b"i", Iframe = b"iframe", Image = b"image", Img = b"img",
    Input = b"input", Isindex = b"isindex", Keygen = b"keygen", Li = b"li", Link = b"link",
    Listing = b"listing", Main = b"main", Malignmark = b"malignmark", Marquee = b"marquee",
    Math = b"math", Menu = b"menu", Meta = b"meta", Mglyph = b"mglyph", Mi = b"mi", Mn = b"mn",
    Mo = b"mo", Ms = b"ms", Mtext = b"mtext", Nav = b"nav", Nobr = b"nobr", Noembed = b"noembed",
    Noframes = b"noframes", Noscript = b"noscript", Object = b"object", Ol = b"ol",
    Optgroup = b"optgroup", Option = b"option", P = b"p", Param = b"param",
    Plaintext = b"plaintext", Pre = b"pre", Rb = b"rb", Rp = b"rp", Rt = b"rt", Rtc = b"rtc",
    Ruby = b"ruby", S = b"s", Script = b"script", Search = b"search", Section = b"section",
    Select = b"select", Small = b"small", Source = b"source", Span = b"span", Strike = b"strike",
    Strong = b"strong", Style = b"style", Sub = b"sub", Summary = b"summary", Sup = b"sup",
    Svg = b"svg", Table = b"table", Tbody = b"tbody", Td = b"td", Template = b"template",
    Textarea = b"textarea", Tfoot = b"tfoot", Th = b"th", Thead = b"thead", Title = b"title",
    Tr = b"tr", Track = b"track", Tt = b"tt", U = b"u", Ul = b"ul", Var = b"var", Wbr = b"wbr",
    Xmp = b"xmp",
}

impl El {
    /// The element a tag name names, whatever its ASCII case.
    fn from_name(name: &[u8]) -> El {
        // No name the model knows is longer than `annotation-xml`.
        let mut lower = [0u8; 14];
        match lower.get_mut(..name.len()) {
            Some(lower) => {
                lower.copy_from_slice(name);
                lower.make_ascii_lowercase();
                El::from_lowercase(lower)
            }
            None => El::Other,
        }
    }

    /// Elements that never hold content in HTML, so they never nest.
    #[rustfmt::skip]
    fn is_void(self) -> bool {
        use El::*;
        matches!(
            self,
            Area | Base | Basefont | Bgsound | Br | Col | Embed | Frame | Hr | Image | Img | Input
                | Keygen | Link | Meta | Param | Source | Track | Wbr
        )
    }

    /// html5ever's "special" HTML elements, which stop the searches that
    /// implied and unmatched end tags make.
    #[rustfmt::skip]
    fn is_special(self) -> bool {
        use El::*;
        matches!(
            self,
            Address | Applet | Area | Article | Aside | Base | Basefont | Bgsound | Blockquote
                | Body | Br | Button | Caption | Center | Col | Colgroup | Dd | Details | Dir | Div
                | Dl | Dt | Embed | Fieldset | Figcaption | Figure | Footer | Form | Frame
                | Frameset | H1 | H2 | H3 | H4 | H5 | H6 | Head | Header | Hgroup | Hr | Html
                | Iframe | Img | Input | Isindex | Li | Link | Listing | Main | Marquee | Menu
                | Meta | Nav | Noembed | Noframes | Noscript | Object | Ol | P | Param | Plaintext
                | Pre | Script | Section | Select | Source | Style | Summary | Table | Tbody | Td
                | Template | Textarea | Tfoot | Th | Thead | Title | Tr | Track | Ul | Wbr | Xmp
        )
    }

    fn is_heading(self) -> bool {
        use El::*;
        matches!(self, H1 | H2 | H3 | H4 | H5 | H6)
    }

    /// Elements an end tag or a later sibling may close implicitly.
    fn has_implied_end(self) -> bool {
        use El::*;
        matches!(
            self,
            Dd | Dt | Li | Option | Optgroup | P | Rb | Rp | Rt | Rtc
        )
    }

    /// Start tags that close SVG and MathML elements to get back to HTML.
    #[rustfmt::skip]
    fn leaves_foreign_content(self) -> bool {
        use El::*;
        matches!(
            self,
            B | Big | Blockquote | Body | Br | Center | Code | Dd | Div | Dl | Dt | Em | Embed | H1
                | H2 | H3 | H4 | H5 | H6 | Head | Hr | I | Img | Li | Listing | Menu | Meta | Nobr
                | Ol | P | Pre | Ruby | S | Small | Span | Strong | Strike | Sub | Sup | Table | Tt
                | U | Ul | Var
        )
    }

    /// How the tokenizer reads this HTML element's content, when that
    /// content is not markup.
    fn raw(self) -> Option<Raw> {
        use El::*;
        match self {
            Iframe | Noembed | Noframes | Style | Textarea | Title | Xmp => Some(Raw::Text),
            Script => Some(Raw::Script),
            Plaintext => Some(Raw::Plaintext),
            _ => None,
        }
    }
}

/// How the tokenizer reads an element whose content is not markup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Raw {
    /// Text up to the element's own end tag.
    Text,
    /// Script data, where `<!--` and a nested `<script>` can hide the end tag.
    Script,
    /// Everything to the end of the input.
    Plaintext,
}

/// The markup language an open element belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ns {
    Html,
    Svg,
    MathMl,
}

/// An element the parse model holds open.
#[derive(Clone, Copy, Debug)]
struct Node {
    el: El,
    ns: Ns,
    /// Where the tag name sits in the input, to compare names the model
    /// does not know.
    name: (usize, usize),
    /// Identity, shared with the element's entry in the formatting list.
    id: usize,
    /// A MathML `annotation-xml` whose content is HTML.
    holds_html: bool,
}

impl Node {
    fn is(&self, el: El) -> bool {
        self.ns == Ns::Html && self.el == el
    }

    fn is_special(&self) -> bool {
        self.ns == Ns::Html && self.el.is_special()
    }

    /// MathML token elements, whose content is HTML.
    fn is_mathml_text(&self) -> bool {
        self.ns == Ns::MathMl && matches!(self.el, El::Mi | El::Mo | El::Mn | El::Ms | El::Mtext)
    }

    /// SVG elements whose content is HTML.
    fn is_svg_html(&self) -> bool {
        self.ns == Ns::Svg && matches!(self.el, El::ForeignObject | El::Desc | El::Title)
    }
}

/// The sets of elements that end a search down the open elements for one
/// element, as html5ever defines them.
#[derive(Clone, Copy)]
enum Scope {
    Default,
    ListItem,
    Button,
    Table,
}

impl Scope {
    fn stops_at(self, node: &Node) -> bool {
        let default = || match node.ns {
            Ns::Html => matches!(
                node.el,
                El::Applet
                    | El::Caption
                    | El::Html
                    | El::Table
                    | El::Td
                    | El::Th
                    | El::Marquee
                    | El::Object
                    | El::Select
                    | El::Template
            ),
            Ns::MathMl => node.is_mathml_text(),
            Ns::Svg => node.is_svg_html(),
        };
        match self {
            Scope::Default => default(),
            Scope::ListItem => default() || node.is(El::Ol) || node.is(El::Ul),
            Scope::Button => default() || node.is(El::Button),
            Scope::Table => {
                node.ns == Ns::Html && matches!(node.el, El::Html | El::Table | El::Template)
            }
        }
    }
}

/// The insertion modes that change how the model handles a tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Body,
    Table,
    TableBody,
    Row,
    Cell,
    Caption,
    ColumnGroup,
    /// Directly inside a `<template>`, before its first start tag.
    Template,
}

/// An entry in the list of active formatting elements.
#[derive(Clone, Copy, Debug)]
enum Entry {
    /// Set by cells, captions, and objects; later entries end there.
    Marker,
    Element {
        id: usize,
        el: El,
        name: (usize, usize),
        /// The start tag's attribute text, to tell identical tags apart.
        attrs: (usize, usize),
    },
}

/// The stack of open elements html5ever would build for a page, and its
/// list of active formatting elements, kept without building a tree.
///
/// Each handler follows the html5ever 0.39 rule of the same insertion mode
/// for the tags that open or close elements. Rules that only shape the tree
/// (foster parenting, where the adoption agency moves nodes) or only report
/// errors are left out, since they never change how many elements are open.
struct Model<'h> {
    html: &'h [u8],
    /// Open elements below the implied `<html>` and `<body>`.
    stack: Vec<Node>,
    active: Vec<Entry>,
    next_id: usize,
    /// The form element pointer: the last form opened outside a template.
    form: Option<usize>,
    /// No doctype, or not an HTML one: a table does not close a paragraph.
    quirks: bool,
    /// Whether anything but a doctype has been seen, which fixes `quirks`.
    started: bool,
    /// Whether a body has been inserted, ending the head.
    body_started: bool,
    /// Whether `</head>` has been seen, after which `<noscript>` starts a
    /// body.
    head_closed: bool,
    /// html5ever's "frameset-ok" flag: whether a `<frameset>` may still
    /// replace the body.
    frameset_ok: bool,
    /// A frameset replaced the body; only framesets open from here on.
    in_frameset: bool,
    /// The insertion mode for the content of each open `<template>`.
    template_modes: Vec<Mode>,
    /// Elements made so far, reopened formatting elements included.
    made: usize,
    /// Most elements a page this size may make; see [`MIN_BYTES_PER_ELEMENT`].
    budget: usize,
    limit: usize,
    /// Set once more than `limit` elements were open at once, or more than
    /// `budget` were made.
    deep: bool,
}

impl<'h> Model<'h> {
    fn new(html: &'h [u8], limit: usize) -> Self {
        Self {
            html,
            stack: Vec::new(),
            active: Vec::new(),
            next_id: 0,
            form: None,
            quirks: true,
            started: false,
            body_started: false,
            head_closed: false,
            frameset_ok: true,
            in_frameset: false,
            template_modes: Vec::new(),
            made: 0,
            budget: html.len() / MIN_BYTES_PER_ELEMENT + 4096,
            limit,
            deep: false,
        }
    }

    /// Whether the current element is SVG or MathML, where the tokenizer
    /// reads CDATA sections.
    fn in_foreign(&self) -> bool {
        self.stack.last().is_some_and(|node| node.ns != Ns::Html)
    }

    fn doctype(&mut self, html: bool) {
        if !self.started {
            self.quirks = !html;
        }
        self.started = true;
    }

    fn text(&mut self, text: &[u8]) {
        if self.in_frameset {
            return;
        }
        let blank = text.iter().all(|&b| is_space(b));
        if !blank {
            self.started = true;
            self.body_started = true;
            self.frameset_ok = false;
        }
        let Some(top) = self.stack.last() else {
            self.reconstruct();
            return;
        };
        if top.ns != Ns::Html && !top.is_mathml_text() && !top.is_svg_html() && !top.holds_html {
            return;
        }
        // Blank text directly in a table stays there; anything else is
        // inserted as in a body, reopening formatting elements first.
        let in_table = top.ns == Ns::Html
            && matches!(
                top.el,
                El::Table | El::Tbody | El::Tfoot | El::Thead | El::Tr | El::Colgroup
            );
        if !(blank && in_table) {
            self.reconstruct();
        }
    }

    /// Handle a start tag. Returns how to read the element's content when
    /// it is not markup.
    fn start_tag(&mut self, tag: StartTag) -> Option<Raw> {
        self.started = true;
        if self.in_frameset {
            return self.frameset_start(tag);
        }
        if !self.body_started {
            use El::*;
            self.body_started = match tag.el {
                Html | Head | Base | Basefont | Bgsound | Link | Meta | Noframes | Script
                | Style | Template | Title | Frameset => false,
                Noscript => self.head_closed,
                _ => true,
            };
        }
        let Some(top) = self.stack.last().copied() else {
            return self.html_start(tag);
        };
        let foreign = match top.ns {
            Ns::Html => false,
            Ns::MathMl if top.is_mathml_text() => matches!(tag.el, El::Mglyph | El::Malignmark),
            Ns::MathMl if top.el == El::AnnotationXml => !(tag.el == El::Svg || top.holds_html),
            Ns::Svg if top.is_svg_html() => false,
            _ => true,
        };
        if !foreign {
            return self.html_start(tag);
        }
        let leaves = tag.el.leaves_foreign_content()
            || (tag.el == El::Font
                && [&b"color"[..], b"face", b"size"]
                    .iter()
                    .any(|name| attribute(self.html, tag.attrs, name).is_some()));
        if leaves {
            self.pop_while(|node| {
                node.ns != Ns::Html && !node.is_mathml_text() && !node.is_svg_html()
            });
            return self.html_start(tag);
        }
        if !tag.self_closing {
            let holds_html = tag.el == El::AnnotationXml
                && top.ns == Ns::MathMl
                && attribute(self.html, tag.attrs, b"encoding").is_some_and(|value| {
                    value.eq_ignore_ascii_case(b"text/html")
                        || value.eq_ignore_ascii_case(b"application/xhtml+xml")
                });
            self.push(tag.el, top.ns, tag.name);
            if let Some(node) = self.stack.last_mut() {
                node.holds_html = holds_html;
            }
        }
        None
    }

    fn end_tag(&mut self, tag: EndTag) {
        self.started = true;
        if self.in_frameset {
            if tag.el == El::Frameset {
                self.stack.pop();
            }
            return;
        }
        match tag.el {
            El::Body | El::Html | El::Br => self.body_started = true,
            El::Head => self.head_closed = true,
            _ => {}
        }
        if !self.in_foreign() {
            return self.html_end(tag);
        }
        if matches!(tag.el, El::Br | El::P) {
            self.pop_while(|node| {
                node.ns != Ns::Html && !node.is_mathml_text() && !node.is_svg_html()
            });
            return self.html_end(tag);
        }
        let name = &self.html[tag.name.0..tag.name.1];
        for i in (0..self.stack.len()).rev() {
            let node = self.stack[i];
            if i + 1 < self.stack.len() && node.ns == Ns::Html {
                return self.html_end(tag);
            }
            if self.html[node.name.0..node.name.1].eq_ignore_ascii_case(name) {
                self.stack.truncate(i);
                return;
            }
        }
        // Below every open element is the HTML `<body>`.
        self.html_end(tag);
    }

    /// The insertion mode, found the way html5ever resets it: from the
    /// nearest open element that sets one.
    fn mode(&self) -> Mode {
        for node in self.stack.iter().rev() {
            if node.ns != Ns::Html {
                continue;
            }
            match node.el {
                El::Td | El::Th => return Mode::Cell,
                El::Tr => return Mode::Row,
                El::Tbody | El::Thead | El::Tfoot => return Mode::TableBody,
                El::Caption => return Mode::Caption,
                El::Colgroup => return Mode::ColumnGroup,
                El::Table => return Mode::Table,
                El::Template => return self.template_modes.last().copied().unwrap_or(Mode::Body),
                _ => {}
            }
        }
        Mode::Body
    }

    fn html_start(&mut self, tag: StartTag) -> Option<Raw> {
        match self.mode() {
            Mode::Body => self.body_start(tag),
            Mode::Table => self.table_start(tag),
            Mode::TableBody => self.table_body_start(tag),
            Mode::Row => self.row_start(tag),
            Mode::Cell => self.cell_start(tag),
            Mode::Caption => self.caption_start(tag),
            Mode::ColumnGroup => self.column_group_start(tag),
            Mode::Template => self.template_start(tag),
        }
    }

    fn html_end(&mut self, tag: EndTag) {
        match self.mode() {
            Mode::Body => self.body_end(tag),
            Mode::Table => self.table_end(tag),
            Mode::TableBody => self.table_body_end(tag),
            Mode::Row => self.row_end(tag),
            Mode::Cell => self.cell_end(tag),
            Mode::Caption => self.caption_end(tag),
            Mode::ColumnGroup => self.column_group_end(tag),
            Mode::Template => {
                if tag.el == El::Template {
                    self.close_template();
                }
            }
        }
    }

    fn body_start(&mut self, tag: StartTag) -> Option<Raw> {
        use El::*;
        match tag.el {
            // Ignored in a body, merged into the root, or opened and closed.
            Html | Head | Caption | Col | Colgroup | Frame | Tbody | Td | Tfoot | Th | Thead
            | Tr | Base | Basefont | Bgsound | Link | Meta | Param | Source | Track => {}
            Body => self.frameset_ok = false,
            Frameset => {
                // Honoured before a body exists, or while nothing has ruled
                // a frameset out; it then replaces everything open.
                let honoured = if self.body_started {
                    self.frameset_ok
                } else {
                    !self.has_open(Template)
                };
                if honoured {
                    self.stack.clear();
                    self.in_frameset = true;
                    self.push_tag(tag);
                }
            }
            Noembed | Noframes | Script | Style | Title => return tag.el.raw(),
            Iframe | Textarea => {
                self.frameset_ok = false;
                return tag.el.raw();
            }
            Template => {
                self.frameset_ok = false;
                self.push_tag(tag);
                self.active.push(Entry::Marker);
                self.template_modes.push(Mode::Template);
            }
            Address | Article | Aside | Blockquote | Center | Details | Dialog | Dir | Div | Dl
            | Fieldset | Figcaption | Figure | Footer | Header | Hgroup | Main | Menu | Nav
            | Ol | P | Search | Section | Summary | Ul => {
                self.close_p_in_button_scope();
                self.push_tag(tag);
            }
            Pre | Listing => {
                self.close_p_in_button_scope();
                self.push_tag(tag);
                self.frameset_ok = false;
            }
            H1 | H2 | H3 | H4 | H5 | H6 => {
                self.close_p_in_button_scope();
                if self
                    .stack
                    .last()
                    .is_some_and(|node| node.ns == Ns::Html && node.el.is_heading())
                {
                    self.stack.pop();
                }
                self.push_tag(tag);
            }
            Form => {
                let in_template = self.has_open(Template);
                if self.form.is_some() && !in_template {
                    return None;
                }
                self.close_p_in_button_scope();
                let id = self.push_tag(tag);
                if !in_template {
                    self.form = Some(id);
                }
            }
            Li | Dd | Dt => {
                self.frameset_ok = false;
                self.close_list_item(tag.el);
                self.close_p_in_button_scope();
                self.push_tag(tag);
            }
            Plaintext => {
                self.close_p_in_button_scope();
                self.push_tag(tag);
                return Some(Raw::Plaintext);
            }
            Button => {
                if self.in_scope(Scope::Default, Button) {
                    self.generate_implied_end(None);
                    self.pop_until(Button);
                }
                self.reconstruct();
                self.push_tag(tag);
                self.frameset_ok = false;
            }
            A => {
                self.close_open_link();
                self.reconstruct();
                self.push_formatting(tag);
            }
            B | Big | Code | Em | Font | I | S | Small | Strike | Strong | Tt | U => {
                self.reconstruct();
                self.push_formatting(tag);
            }
            Nobr => {
                self.reconstruct();
                if self.in_scope(Scope::Default, Nobr) {
                    self.adoption_agency(EndTag {
                        el: Nobr,
                        name: tag.name,
                    });
                    self.reconstruct();
                }
                self.push_formatting(tag);
            }
            Applet | Marquee | Object => {
                self.reconstruct();
                self.push_tag(tag);
                self.active.push(Entry::Marker);
                self.frameset_ok = false;
            }
            Table => {
                if !self.quirks {
                    self.close_p_in_button_scope();
                }
                self.push_tag(tag);
                self.frameset_ok = false;
            }
            Area | Br | Embed | Image | Img | Keygen | Wbr => {
                self.reconstruct();
                self.frameset_ok = false;
            }
            Input => {
                if self.in_scope(Scope::Default, Select) {
                    self.pop_until(Select);
                }
                self.reconstruct();
                let hidden = attribute(self.html, tag.attrs, b"type")
                    .is_some_and(|value| value.eq_ignore_ascii_case(b"hidden"));
                if !hidden {
                    self.frameset_ok = false;
                }
            }
            Hr => {
                self.close_p_in_button_scope();
                if self.in_scope(Scope::Default, Select) {
                    self.generate_implied_end(None);
                }
                self.frameset_ok = false;
            }
            Xmp => {
                self.close_p_in_button_scope();
                self.reconstruct();
                self.frameset_ok = false;
                return Some(Raw::Text);
            }
            Select => {
                if self.in_scope(Scope::Default, Select) {
                    self.pop_until(Select);
                } else {
                    self.reconstruct();
                    self.push_tag(tag);
                    self.frameset_ok = false;
                }
            }
            Option | Optgroup => {
                if self.in_scope(Scope::Default, Select) {
                    let except = (tag.el == Option).then_some(Optgroup);
                    self.generate_implied_end(except);
                } else if self.stack.last().is_some_and(|node| node.is(Option)) {
                    self.stack.pop();
                }
                self.reconstruct();
                self.push_tag(tag);
            }
            Rb | Rtc | Rp | Rt => {
                if self.in_scope(Scope::Default, Ruby) {
                    let except = matches!(tag.el, Rp | Rt).then_some(Rtc);
                    self.generate_implied_end(except);
                }
                self.push_tag(tag);
            }
            Math | Svg => {
                self.reconstruct();
                if !tag.self_closing {
                    let ns = if tag.el == Math { Ns::MathMl } else { Ns::Svg };
                    self.push(tag.el, ns, tag.name);
                }
            }
            _ => {
                self.reconstruct();
                self.push_tag(tag);
            }
        }
        None
    }

    fn body_end(&mut self, tag: EndTag) {
        use El::*;
        match tag.el {
            Template => self.close_template(),
            Body | Html => {}
            Address | Article | Aside | Blockquote | Button | Center | Details | Dialog | Dir
            | Div | Dl | Fieldset | Figcaption | Figure | Footer | Header | Hgroup | Listing
            | Main | Menu | Nav | Ol | Pre | Search | Section | Select | Summary | Ul => {
                if self.in_scope(Scope::Default, tag.el) {
                    self.generate_implied_end(None);
                    self.pop_until(tag.el);
                }
            }
            Form => self.close_form(),
            P => {
                if self.in_scope(Scope::Button, P) {
                    self.close_p();
                }
            }
            Li | Dd | Dt => {
                let scope = if tag.el == Li {
                    Scope::ListItem
                } else {
                    Scope::Default
                };
                if self.in_scope(scope, tag.el) {
                    self.generate_implied_end(Some(tag.el));
                    self.pop_until(tag.el);
                }
            }
            H1 | H2 | H3 | H4 | H5 | H6 => {
                let heading = |node: &Node| node.ns == Ns::Html && node.el.is_heading();
                if self.in_scope_where(Scope::Default, heading) {
                    self.generate_implied_end(None);
                    self.pop_through(heading);
                }
            }
            A | B | Big | Code | Em | Font | I | Nobr | S | Small | Strike | Strong | Tt | U => {
                self.adoption_agency(tag);
            }
            Applet | Marquee | Object => {
                if self.in_scope(Scope::Default, tag.el) {
                    self.generate_implied_end(None);
                    self.pop_until(tag.el);
                    self.clear_to_marker();
                }
            }
            // `</br>` is read as `<br>`.
            Br => {
                self.reconstruct();
                self.frameset_ok = false;
            }
            _ => self.close_any(tag),
        }
    }

    fn table_start(&mut self, tag: StartTag) -> Option<Raw> {
        use El::*;
        let table_context = |node: &Node| Scope::Table.stops_at(node);
        match tag.el {
            Caption => {
                self.pop_until_current(table_context);
                self.active.push(Entry::Marker);
                self.push_tag(tag);
            }
            Colgroup | Tbody | Tfoot | Thead => {
                self.pop_until_current(table_context);
                self.push_tag(tag);
            }
            // The `<col>` itself is void.
            Col => {
                self.pop_until_current(table_context);
                self.push(Colgroup, Ns::Html, (0, 0));
            }
            Td | Th | Tr => {
                self.pop_until_current(table_context);
                self.push(Tbody, Ns::Html, (0, 0));
                return self.table_body_start(tag);
            }
            Table => {
                if self.in_scope(Scope::Table, Table) {
                    self.pop_until(Table);
                    return self.start_tag(tag);
                }
            }
            Script | Style | Template => return self.body_start(tag),
            Form => {
                if self.form.is_none() && !self.has_open(Template) {
                    self.form = Some(self.new_id());
                }
            }
            _ => return self.body_start(tag),
        }
        None
    }

    fn table_end(&mut self, tag: EndTag) {
        use El::*;
        match tag.el {
            Table => {
                if self.in_scope(Scope::Table, Table) {
                    self.pop_until(Table);
                }
            }
            Body | Caption | Col | Colgroup | Html | Tbody | Td | Tfoot | Th | Thead | Tr => {}
            Template => self.close_template(),
            _ => self.body_end(tag),
        }
    }

    fn table_body_start(&mut self, tag: StartTag) -> Option<Raw> {
        use El::*;
        let body_context = |node: &Node| {
            node.ns == Ns::Html && matches!(node.el, Tbody | Tfoot | Thead | Template | Html)
        };
        match tag.el {
            Tr => {
                self.pop_until_current(body_context);
                self.push_tag(tag);
                None
            }
            Td | Th => {
                self.pop_until_current(body_context);
                self.push(Tr, Ns::Html, (0, 0));
                self.row_start(tag)
            }
            Caption | Col | Colgroup | Tbody | Tfoot | Thead => {
                let outer =
                    |node: &Node| node.ns == Ns::Html && matches!(node.el, Table | Tbody | Tfoot);
                if self.in_scope_where(Scope::Table, outer) {
                    self.pop_until_current(body_context);
                    self.stack.pop();
                    return self.table_start(tag);
                }
                None
            }
            _ => self.table_start(tag),
        }
    }

    fn table_body_end(&mut self, tag: EndTag) {
        use El::*;
        let body_context = |node: &Node| {
            node.ns == Ns::Html && matches!(node.el, Tbody | Tfoot | Thead | Template | Html)
        };
        match tag.el {
            Tbody | Tfoot | Thead => {
                if self.in_scope(Scope::Table, tag.el) {
                    self.pop_until_current(body_context);
                    self.stack.pop();
                }
            }
            Table => {
                let outer =
                    |node: &Node| node.ns == Ns::Html && matches!(node.el, Table | Tbody | Tfoot);
                if self.in_scope_where(Scope::Table, outer) {
                    self.pop_until_current(body_context);
                    self.stack.pop();
                    self.table_end(tag);
                }
            }
            Body | Caption | Col | Colgroup | Html | Td | Th | Tr => {}
            _ => self.table_end(tag),
        }
    }

    fn row_start(&mut self, tag: StartTag) -> Option<Raw> {
        use El::*;
        match tag.el {
            Td | Th => {
                self.pop_until_current(|node| {
                    node.ns == Ns::Html && matches!(node.el, Tr | Template | Html)
                });
                self.push_tag(tag);
                self.active.push(Entry::Marker);
                None
            }
            Caption | Col | Colgroup | Tbody | Tfoot | Thead | Tr => {
                if self.in_scope(Scope::Table, Tr) {
                    self.close_row();
                    return self.table_body_start(tag);
                }
                None
            }
            _ => self.table_start(tag),
        }
    }

    fn row_end(&mut self, tag: EndTag) {
        use El::*;
        match tag.el {
            Tr => {
                if self.in_scope(Scope::Table, Tr) {
                    self.close_row();
                }
            }
            Table => {
                if self.in_scope(Scope::Table, Tr) {
                    self.close_row();
                    self.table_body_end(tag);
                }
            }
            Tbody | Tfoot | Thead => {
                if self.in_scope(Scope::Table, tag.el) && self.in_scope(Scope::Table, Tr) {
                    self.close_row();
                    self.table_body_end(tag);
                }
            }
            Body | Caption | Col | Colgroup | Html | Td | Th => {}
            _ => self.table_end(tag),
        }
    }

    fn cell_start(&mut self, tag: StartTag) -> Option<Raw> {
        use El::*;
        match tag.el {
            Caption | Col | Colgroup | Tbody | Td | Tfoot | Th | Thead | Tr => {
                if self.in_scope_where(Scope::Table, |node| node.is(Td) || node.is(Th)) {
                    self.close_cell();
                    return self.row_start(tag);
                }
                None
            }
            _ => self.body_start(tag),
        }
    }

    fn cell_end(&mut self, tag: EndTag) {
        use El::*;
        match tag.el {
            Td | Th => {
                if self.in_scope(Scope::Table, tag.el) {
                    self.generate_implied_end(None);
                    self.pop_until(tag.el);
                    self.clear_to_marker();
                }
            }
            Body | Caption | Col | Colgroup | Html => {}
            Table | Tbody | Tfoot | Thead | Tr => {
                if self.in_scope(Scope::Table, tag.el) {
                    self.close_cell();
                    self.row_end(tag);
                }
            }
            _ => self.body_end(tag),
        }
    }

    fn caption_start(&mut self, tag: StartTag) -> Option<Raw> {
        use El::*;
        match tag.el {
            Caption | Col | Colgroup | Tbody | Td | Tfoot | Th | Thead | Tr => {
                if self.in_scope(Scope::Table, Caption) {
                    self.close_caption();
                    return self.table_start(tag);
                }
                None
            }
            _ => self.body_start(tag),
        }
    }

    fn caption_end(&mut self, tag: EndTag) {
        use El::*;
        match tag.el {
            Caption => {
                if self.in_scope(Scope::Table, Caption) {
                    self.close_caption();
                }
            }
            Table => {
                if self.in_scope(Scope::Table, Caption) {
                    self.close_caption();
                    self.table_end(tag);
                }
            }
            Body | Col | Colgroup | Html | Tbody | Td | Tfoot | Th | Thead | Tr => {}
            _ => self.body_end(tag),
        }
    }

    fn column_group_start(&mut self, tag: StartTag) -> Option<Raw> {
        match tag.el {
            El::Html | El::Col => None,
            El::Template => self.body_start(tag),
            _ => {
                if self.stack.last().is_some_and(|node| node.is(El::Colgroup)) {
                    self.stack.pop();
                    return self.table_start(tag);
                }
                None
            }
        }
    }

    fn column_group_end(&mut self, tag: EndTag) {
        match tag.el {
            El::Col => {}
            El::Template => self.close_template(),
            _ => {
                if self.stack.last().is_some_and(|node| node.is(El::Colgroup)) {
                    self.stack.pop();
                    if tag.el != El::Colgroup {
                        self.table_end(tag);
                    }
                }
            }
        }
    }

    /// A start tag directly inside a `<template>`, which picks the mode the
    /// template's content is parsed in.
    fn template_start(&mut self, tag: StartTag) -> Option<Raw> {
        use El::*;
        let mode = match tag.el {
            Base | Basefont | Bgsound | Link | Meta | Noframes | Script | Style | Template
            | Title => return self.body_start(tag),
            Caption | Colgroup | Tbody | Tfoot | Thead => Mode::Table,
            Col => Mode::ColumnGroup,
            Tr => Mode::TableBody,
            Td | Th => Mode::Row,
            _ => Mode::Body,
        };
        if let Some(current) = self.template_modes.last_mut() {
            *current = mode;
        }
        self.html_start(tag)
    }

    /// Once a frameset has replaced the body, only framesets open; every
    /// other tag, and all text, is ignored.
    fn frameset_start(&mut self, tag: StartTag) -> Option<Raw> {
        match tag.el {
            El::Frameset if !self.stack.is_empty() => {
                self.push_tag(tag);
                None
            }
            El::Noframes => Some(Raw::Text),
            _ => None,
        }
    }

    fn new_id(&mut self) -> usize {
        self.next_id += 1;
        self.next_id
    }

    fn push(&mut self, el: El, ns: Ns, name: (usize, usize)) -> usize {
        self.made += 1;
        if self.made > self.budget {
            self.deep = true;
        }
        let id = self.new_id();
        self.stack.push(Node {
            el,
            ns,
            name,
            id,
            holds_html: false,
        });
        if self.stack.len() > self.limit {
            self.deep = true;
        }
        id
    }

    fn push_tag(&mut self, tag: StartTag) -> usize {
        self.push(tag.el, Ns::Html, tag.name)
    }

    fn has_open(&self, el: El) -> bool {
        self.stack.iter().any(|node| node.is(el))
    }

    fn in_scope(&self, scope: Scope, el: El) -> bool {
        self.in_scope_where(scope, |node| node.is(el))
    }

    /// Whether an open element matching `found` sits above every element
    /// that ends `scope`.
    fn in_scope_where(&self, scope: Scope, found: impl Fn(&Node) -> bool) -> bool {
        for node in self.stack.iter().rev() {
            if found(node) {
                return true;
            }
            if scope.stops_at(node) {
                return false;
            }
        }
        false
    }

    fn pop_while(&mut self, pop: impl Fn(&Node) -> bool) {
        while self.stack.last().is_some_and(&pop) {
            self.stack.pop();
        }
    }

    fn pop_until_current(&mut self, keep: impl Fn(&Node) -> bool) {
        self.pop_while(|node| !keep(node));
    }

    /// Pop elements until one matching `last` has been popped.
    fn pop_through(&mut self, last: impl Fn(&Node) -> bool) {
        while let Some(node) = self.stack.pop() {
            if last(&node) {
                break;
            }
        }
    }

    fn pop_until(&mut self, el: El) {
        self.pop_through(|node| node.is(el));
    }

    /// Pop the elements an end tag closes on its way, except `except`.
    fn generate_implied_end(&mut self, except: Option<El>) {
        self.pop_while(|node| {
            node.ns == Ns::Html && node.el.has_implied_end() && Some(node.el) != except
        });
    }

    fn close_p(&mut self) {
        self.generate_implied_end(Some(El::P));
        self.pop_until(El::P);
    }

    fn close_p_in_button_scope(&mut self) {
        if self.in_scope(Scope::Button, El::P) {
            self.close_p();
        }
    }

    /// A new `<li>` closes the nearest open one, and `<dd>` or `<dt>` the
    /// nearest of either, unless a special element other than `<address>`,
    /// `<div>`, or `<p>` comes first.
    fn close_list_item(&mut self, el: El) {
        for i in (0..self.stack.len()).rev() {
            let node = self.stack[i];
            let closes = node.ns == Ns::Html
                && if el == El::Li {
                    node.el == El::Li
                } else {
                    matches!(node.el, El::Dd | El::Dt)
                };
            if closes {
                self.stack.truncate(i);
                return;
            }
            if node.is_special() && !matches!(node.el, El::Address | El::Div | El::P) {
                return;
            }
        }
    }

    fn close_row(&mut self) {
        self.pop_until_current(|node| {
            node.ns == Ns::Html && matches!(node.el, El::Tr | El::Template | El::Html)
        });
        self.stack.pop();
    }

    fn close_cell(&mut self) {
        self.generate_implied_end(None);
        self.pop_through(|node| node.is(El::Td) || node.is(El::Th));
        self.clear_to_marker();
    }

    fn close_caption(&mut self) {
        self.generate_implied_end(None);
        self.pop_until(El::Caption);
        self.clear_to_marker();
    }

    fn close_template(&mut self) {
        if self.has_open(El::Template) {
            self.pop_until(El::Template);
            self.clear_to_marker();
            self.template_modes.pop();
        }
    }

    fn close_form(&mut self) {
        if self.has_open(El::Template) {
            if self.in_scope(Scope::Default, El::Form) {
                self.pop_until(El::Form);
            }
            return;
        }
        let Some(id) = self.form.take() else {
            return;
        };
        if self.in_scope_where(Scope::Default, |node| node.id == id) {
            self.generate_implied_end(None);
            if let Some(at) = self.stack.iter().rposition(|node| node.id == id) {
                self.stack.remove(at);
            }
        }
    }

    /// An end tag with no rule of its own closes the nearest open element
    /// of its name, unless a special element comes first.
    fn close_any(&mut self, tag: EndTag) {
        for i in (0..self.stack.len()).rev() {
            let node = self.stack[i];
            if self.names(&node, tag) {
                self.stack.truncate(i);
                return;
            }
            if node.is_special() {
                return;
            }
        }
    }

    /// Whether `node` is the HTML element `tag` names.
    fn names(&self, node: &Node, tag: EndTag) -> bool {
        node.ns == Ns::Html
            && node.el == tag.el
            && (tag.el != El::Other
                || self.html[node.name.0..node.name.1]
                    .eq_ignore_ascii_case(&self.html[tag.name.0..tag.name.1]))
    }

    fn is_open(&self, id: usize) -> bool {
        self.stack.iter().rev().any(|node| node.id == id)
    }

    fn entry_at(&self, id: usize) -> Option<usize> {
        self.active
            .iter()
            .rposition(|entry| matches!(entry, Entry::Element { id: at, .. } if *at == id))
    }

    fn clear_to_marker(&mut self) {
        while let Some(entry) = self.active.pop() {
            if matches!(entry, Entry::Marker) {
                break;
            }
        }
    }

    /// Open a formatting element and list it, dropping the earliest of three
    /// identical ones already listed (the "Noah's Ark" clause).
    fn push_formatting(&mut self, tag: StartTag) {
        let html = self.html;
        let attrs = &html[tag.attrs.0..tag.attrs.1];
        let mut earliest = None;
        let mut same = 0;
        for (i, entry) in self.active.iter().enumerate().rev() {
            match *entry {
                Entry::Marker => break,
                Entry::Element {
                    el, attrs: other, ..
                } if el == tag.el && html[other.0..other.1] == *attrs => {
                    earliest = Some(i);
                    same += 1;
                }
                Entry::Element { .. } => {}
            }
        }
        if same >= 3
            && let Some(i) = earliest
        {
            self.active.remove(i);
        }
        let id = self.push_tag(tag);
        self.active.push(Entry::Element {
            id,
            el: tag.el,
            name: tag.name,
            attrs: tag.attrs,
        });
    }

    /// Reopen listed formatting elements that are no longer open, as
    /// html5ever does before inserting text and most elements.
    fn reconstruct(&mut self) {
        let reopen_from = |model: &Self, i: usize| match model.active[i] {
            Entry::Marker => false,
            Entry::Element { id, .. } => !model.is_open(id),
        };
        let Some(last) = self.active.len().checked_sub(1) else {
            return;
        };
        if !reopen_from(self, last) {
            return;
        }
        let mut first = last;
        while first > 0 && reopen_from(self, first - 1) {
            first -= 1;
        }
        for i in first..self.active.len() {
            if let Entry::Element {
                el, name, attrs, ..
            } = self.active[i]
            {
                let id = self.push(el, Ns::Html, name);
                self.active[i] = Entry::Element {
                    id,
                    el,
                    name,
                    attrs,
                };
            }
            if self.deep {
                return;
            }
        }
    }

    /// `<a>` while a link is still listed closes that link first.
    fn close_open_link(&mut self) {
        let open = self.active.iter().rev().find_map(|entry| match *entry {
            Entry::Marker => Some(None),
            Entry::Element { id, el: El::A, .. } => Some(Some(id)),
            Entry::Element { .. } => None,
        });
        let Some(Some(id)) = open else {
            return;
        };
        self.adoption_agency(EndTag {
            el: El::A,
            name: (0, 0),
        });
        if let Some(i) = self.entry_at(id) {
            self.active.remove(i);
        }
        if let Some(i) = self.stack.iter().rposition(|node| node.id == id) {
            self.stack.remove(i);
        }
    }

    /// html5ever's adoption agency algorithm for a formatting end tag, kept
    /// to its effect on the open elements and the formatting list.
    fn adoption_agency(&mut self, tag: EndTag) {
        let subject = tag.el;
        if let Some(top) = self.stack.last()
            && top.is(subject)
            && self.entry_at(top.id).is_none()
        {
            self.stack.pop();
            return;
        }
        for _ in 0..8 {
            let listed = self
                .active
                .iter()
                .enumerate()
                .rev()
                .find_map(|(i, entry)| match *entry {
                    Entry::Marker => Some(None),
                    Entry::Element { id, el, .. } if el == subject => Some(Some((i, id))),
                    Entry::Element { .. } => None,
                });
            let Some(Some((entry, fmt))) = listed else {
                return self.close_any(tag);
            };
            let Some(fmt_at) = self.stack.iter().rposition(|node| node.id == fmt) else {
                self.active.remove(entry);
                return;
            };
            if !self.in_scope_where(Scope::Default, |node| node.id == fmt) {
                return;
            }
            let Some(block_at) =
                (fmt_at + 1..self.stack.len()).find(|&i| self.stack[i].is_special())
            else {
                self.stack.truncate(fmt_at);
                self.active.remove(entry);
                return;
            };
            let block = self.stack[block_at].id;
            // Where the formatting element's replacement goes in the list:
            // in its place, or after the first element reopened above it.
            let mut after = None;
            let mut at = block_at;
            let mut steps = 0;
            loop {
                steps += 1;
                at -= 1;
                let node = self.stack[at];
                if node.id == fmt {
                    break;
                }
                let listed = self.entry_at(node.id);
                if steps > 3 || listed.is_none() {
                    if let Some(i) = listed {
                        self.active.remove(i);
                    }
                    self.stack.remove(at);
                    continue;
                }
                let id = self.new_id();
                self.stack[at].id = id;
                if let Some(Entry::Element { id: listed_id, .. }) =
                    listed.and_then(|i| self.active.get_mut(i))
                {
                    *listed_id = id;
                }
                if after.is_none() {
                    after = Some(id);
                }
            }
            let Some(old) = self.entry_at(fmt) else {
                return;
            };
            let Entry::Element {
                el, name, attrs, ..
            } = self.active[old]
            else {
                return;
            };
            let id = self.new_id();
            let replacement = Entry::Element {
                id,
                el,
                name,
                attrs,
            };
            match after.and_then(|prev| self.entry_at(prev)) {
                Some(prev) => {
                    self.active.insert(prev + 1, replacement);
                    if let Some(old) = self.entry_at(fmt) {
                        self.active.remove(old);
                    }
                }
                None => self.active[old] = replacement,
            }
            if let Some(i) = self.stack.iter().rposition(|node| node.id == fmt) {
                self.stack.remove(i);
            }
            let Some(block_at) = self.stack.iter().position(|node| node.id == block) else {
                return;
            };
            self.stack.insert(
                block_at + 1,
                Node {
                    el,
                    ns: Ns::Html,
                    name,
                    id,
                    holds_html: false,
                },
            );
        }
    }
}

/// A start tag, with its parts as byte ranges of the input.
#[derive(Clone, Copy, Debug)]
struct StartTag {
    el: El,
    name: (usize, usize),
    /// Everything between the name and the closing `>`.
    attrs: (usize, usize),
    /// Ends in `/>`, which only SVG and MathML elements honour.
    self_closing: bool,
}

#[derive(Clone, Copy, Debug)]
struct EndTag {
    el: El,
    name: (usize, usize),
}

#[derive(Clone, Copy, Debug)]
enum TagKind {
    Start(StartTag),
    End(EndTag),
    /// A doctype, and whether it names `html`.
    Doctype {
        html: bool,
    },
    /// A comment, CDATA section, processing instruction, stray `</>`, or a
    /// tag the input ends inside of. None of them opens an element.
    Other,
}

/// One piece of markup found by [`Lexer::next_tag`], which leaves the
/// lexer's `at` just past it.
#[derive(Clone, Copy, Debug)]
struct Tag {
    /// Offset of the `<`.
    start: usize,
    kind: TagKind,
}

/// Splits HTML into markup and text the way html5ever's tokenizer does, so
/// that quotes, comments, and raw text hide exactly the markup they hide
/// from the parser.
struct Lexer<'h> {
    html: &'h [u8],
    /// Where the next search starts.
    at: usize,
}

impl<'h> Lexer<'h> {
    fn new(html: &'h [u8]) -> Self {
        Self { html, at: 0 }
    }

    /// The next piece of markup, leaving `at` just past it. The text before
    /// it runs from the old `at` to its `start`. `foreign` says the current
    /// element is SVG or MathML, where `<![CDATA[` opens a CDATA section.
    fn next_tag(&mut self, foreign: bool) -> Option<Tag> {
        let html = self.html;
        let mut from = self.at;
        loop {
            let start = from + html.get(from..)?.iter().position(|&b| b == b'<')?;
            let (end, kind) = match html.get(start + 1) {
                Some(b'!') => self.declaration(start, foreign),
                Some(b'?') => (markup_end(html, start + 2), TagKind::Other),
                Some(b'/') => match html.get(start + 2) {
                    Some(b) if b.is_ascii_alphabetic() => {
                        let (name, end, _) = tag_name_and_end(html, start + 2);
                        let el = El::from_name(&html[name.0..name.1]);
                        match end {
                            Some(end) => (end, TagKind::End(EndTag { el, name })),
                            None => (html.len(), TagKind::Other),
                        }
                    }
                    Some(b'>') => (start + 3, TagKind::Other),
                    Some(_) => (markup_end(html, start + 2), TagKind::Other),
                    None => {
                        from = start + 1;
                        continue;
                    }
                },
                Some(b) if b.is_ascii_alphabetic() => {
                    let (name, end, self_closing) = tag_name_and_end(html, start + 1);
                    let el = El::from_name(&html[name.0..name.1]);
                    match end {
                        Some(end) => {
                            let attrs = (name.1, (end - 1).max(name.1));
                            let tag = StartTag {
                                el,
                                name,
                                attrs,
                                self_closing,
                            };
                            (end, TagKind::Start(tag))
                        }
                        None => (html.len(), TagKind::Other),
                    }
                }
                // A `<` that starts nothing is text.
                _ => {
                    from = start + 1;
                    continue;
                }
            };
            self.at = end;
            return Some(Tag { start, kind });
        }
    }

    /// The end of the `<!` markup at `start`, and what it is.
    fn declaration(&self, start: usize, foreign: bool) -> (usize, TagKind) {
        let html = self.html;
        let rest = &html[start + 2..];
        if rest.starts_with(b"--") {
            return (comment_end(html, start + 4), TagKind::Other);
        }
        if foreign && rest.starts_with(b"[CDATA[") {
            let end = find(&html[start + 9..], b"]]>").map_or(html.len(), |at| start + 9 + at + 3);
            return (end, TagKind::Other);
        }
        let end = markup_end(html, start + 2);
        let doctype = rest.len() >= 7 && rest[..7].eq_ignore_ascii_case(b"doctype");
        if !doctype {
            return (end, TagKind::Other);
        }
        let name = rest[7..]
            .iter()
            .skip_while(|&&b| is_space(b))
            .take_while(|&&b| !is_space(b) && b != b'>')
            .copied()
            .collect::<Vec<u8>>();
        let kind = TagKind::Doctype {
            html: name.eq_ignore_ascii_case(b"html"),
        };
        (end, kind)
    }

    /// Skip the content of the element whose start tag, named at `name`,
    /// was just read, together with its end tag.
    fn skip_raw(&mut self, name: (usize, usize), raw: Raw) {
        self.raw_content(name, raw);
    }

    /// Like [`Lexer::skip_raw`], and returns the content's range.
    fn raw_content(&mut self, name: (usize, usize), raw: Raw) -> (usize, usize) {
        let html = self.html;
        let from = self.at;
        let close = match raw {
            Raw::Text => raw_text_close(html, from, &html[name.0..name.1]),
            Raw::Script => script_close(html, from),
            Raw::Plaintext => None,
        };
        let Some(close) = close else {
            self.at = html.len();
            return (from, html.len());
        };
        // The end tag may carry attributes, so it is read like any other.
        self.at = close;
        self.next_tag(false);
        (from, close)
    }
}

/// Whether `b` is whitespace to the HTML tokenizer (which reads `\r` as a
/// line break).
fn is_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\x0C' | b'\r' | b' ')
}

/// Offset just past the first `>` at or after `from`, or the end of input.
fn markup_end(html: &[u8], from: usize) -> usize {
    html.get(from..)
        .and_then(|rest| rest.iter().position(|&b| b == b'>'))
        .map_or(html.len(), |at| from + at + 1)
}

/// Offset just past a comment whose `<!--` ends at `from`: `<!-->` and
/// `<!--->` close at once; otherwise the first `-->` or `--!>` does.
fn comment_end(html: &[u8], from: usize) -> usize {
    let rest = &html[from.min(html.len())..];
    if rest.starts_with(b">") {
        return from + 1;
    }
    if rest.starts_with(b"->") {
        return from + 2;
    }
    let mut at = 0;
    while let Some(dash) = rest
        .get(at..)
        .and_then(|tail| tail.iter().position(|&b| b == b'-'))
    {
        let dash = at + dash;
        if rest[dash..].starts_with(b"-->") {
            return from + dash + 3;
        }
        if rest[dash..].starts_with(b"--!>") {
            return from + dash + 4;
        }
        at = dash + 1;
    }
    html.len()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Read a tag's name from `from` and its attributes after it, the way the
/// tokenizer's tag states do. Returns the name's range, the offset past the
/// closing `>` (`None` when the input ends first, which drops the tag), and
/// whether the tag closed with `/>`.
///
/// A quote only opens a value right after `=`, so `title=it's` is one value
/// and a `>` inside quotes does not end the tag.
fn tag_name_and_end(html: &[u8], from: usize) -> ((usize, usize), Option<usize>, bool) {
    let name_end = html[from..]
        .iter()
        .position(|&b| is_space(b) || b == b'/' || b == b'>')
        .map_or(html.len(), |at| from + at);
    let name = (from, name_end);
    #[derive(Clone, Copy)]
    enum State {
        BeforeName,
        Name,
        AfterName,
        BeforeValue,
        Quoted(u8),
        Unquoted,
        AfterQuoted,
        SelfClosing,
    }
    let mut state = State::BeforeName;
    let mut at = name_end;
    while let Some(&b) = html.get(at) {
        let space = is_space(b);
        state = match state {
            State::BeforeName | State::AfterName if space => state,
            State::BeforeName | State::Name | State::AfterName | State::AfterQuoted
                if b == b'>' =>
            {
                return (name, Some(at + 1), false);
            }
            State::BeforeName | State::Name | State::AfterName | State::AfterQuoted
                if b == b'/' =>
            {
                State::SelfClosing
            }
            State::Name | State::AfterName if b == b'=' => State::BeforeValue,
            State::Name if space => State::AfterName,
            State::BeforeName | State::Name | State::AfterName => State::Name,
            State::BeforeValue if space => State::BeforeValue,
            State::BeforeValue if b == b'"' || b == b'\'' => State::Quoted(b),
            State::BeforeValue if b == b'>' => return (name, Some(at + 1), false),
            State::BeforeValue => State::Unquoted,
            State::Quoted(quote) if b == quote => State::AfterQuoted,
            State::Quoted(quote) => State::Quoted(quote),
            State::Unquoted if space => State::BeforeName,
            State::Unquoted if b == b'>' => return (name, Some(at + 1), false),
            State::Unquoted => State::Unquoted,
            State::AfterQuoted if space => State::BeforeName,
            State::AfterQuoted => State::Name,
            State::SelfClosing if b == b'>' => return (name, Some(at + 1), true),
            // Anything else after `/` is read again as the start of a name.
            State::SelfClosing => {
                state = State::BeforeName;
                continue;
            }
        };
        at += 1;
    }
    (name, None, false)
}

/// Whether an end tag for `name` starts at `at`: `</name` followed by
/// whitespace, `/`, or `>`, in any ASCII case.
fn is_end_tag_for(html: &[u8], at: usize, name: &[u8]) -> bool {
    let after = at + 2 + name.len();
    html.get(at..at + 2) == Some(b"</")
        && html
            .get(at + 2..after)
            .is_some_and(|found| found.eq_ignore_ascii_case(name))
        && html
            .get(after)
            .is_some_and(|&b| is_space(b) || b == b'/' || b == b'>')
}

/// Offset of the end tag that closes a raw text element named `name`,
/// searching from `from`.
fn raw_text_close(html: &[u8], from: usize, name: &[u8]) -> Option<usize> {
    let mut at = from;
    while let Some(off) = html.get(at..)?.iter().position(|&b| b == b'<') {
        let start = at + off;
        if is_end_tag_for(html, start, name) {
            return Some(start);
        }
        at = start + 1;
    }
    None
}

/// Offset of the `</script>` that ends script data starting at `from`.
///
/// Inside `<!--`, a `<script>` tag starts a stretch where `</script>` only
/// ends that nested tag, until `-->` ends the escape, as the tokenizer's
/// script data states read it.
fn script_close(html: &[u8], from: usize) -> Option<usize> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Data,
        Escaped,
        DoubleEscaped,
    }
    let is_script_tag = |at: usize| {
        html.get(at..at + 6)
            .is_some_and(|name| name.eq_ignore_ascii_case(b"script"))
            && html
                .get(at + 6)
                .is_some_and(|&b| is_space(b) || b == b'/' || b == b'>')
    };
    let mut state = State::Data;
    let mut dashes = 0usize;
    let mut at = from;
    while let Some(&b) = html.get(at) {
        match state {
            State::Data => {
                if b == b'<' {
                    if is_end_tag_for(html, at, b"script") {
                        return Some(at);
                    }
                    if html[at + 1..].starts_with(b"!--") {
                        state = State::Escaped;
                        dashes = 2;
                        at += 4;
                        continue;
                    }
                }
            }
            State::Escaped | State::DoubleEscaped => match b {
                b'-' => dashes += 1,
                b'>' if dashes >= 2 => {
                    state = State::Data;
                    dashes = 0;
                }
                b'<' => {
                    dashes = 0;
                    if state == State::Escaped {
                        if is_end_tag_for(html, at, b"script") {
                            return Some(at);
                        }
                        if is_script_tag(at + 1) {
                            state = State::DoubleEscaped;
                        }
                    } else if html.get(at + 1) == Some(&b'/') && is_script_tag(at + 2) {
                        state = State::Escaped;
                    }
                }
                _ => dashes = 0,
            },
        }
        at += 1;
    }
    None
}

/// The value of the first attribute named `wanted` in a start tag's
/// attribute text, read as the tokenizer reads attributes.
fn attribute<'h>(html: &'h [u8], attrs: (usize, usize), wanted: &[u8]) -> Option<&'h [u8]> {
    let attrs = &html[attrs.0..attrs.1];
    let mut at = 0;
    while at < attrs.len() {
        while attrs.get(at).is_some_and(|&b| is_space(b) || b == b'/') {
            at += 1;
        }
        if at >= attrs.len() {
            break;
        }
        // A name's first character is part of it, even `=`.
        let name_start = at;
        at += 1;
        while attrs
            .get(at)
            .is_some_and(|&b| !is_space(b) && !matches!(b, b'/' | b'=' | b'>'))
        {
            at += 1;
        }
        let name = &attrs[name_start..at];
        while attrs.get(at).is_some_and(|&b| is_space(b)) {
            at += 1;
        }
        let mut value: &[u8] = b"";
        if attrs.get(at) == Some(&b'=') {
            at += 1;
            while attrs.get(at).is_some_and(|&b| is_space(b)) {
                at += 1;
            }
            match attrs.get(at) {
                Some(&quote) if quote == b'"' || quote == b'\'' => {
                    let start = at + 1;
                    let end = attrs[start..]
                        .iter()
                        .position(|&b| b == quote)
                        .map_or(attrs.len(), |off| start + off);
                    value = &attrs[start..end];
                    at = end + 1;
                }
                _ => {
                    let start = at;
                    while attrs.get(at).is_some_and(|&b| !is_space(b) && b != b'>') {
                        at += 1;
                    }
                    value = &attrs[start..at];
                }
            }
        }
        if name.eq_ignore_ascii_case(wanted) {
            return Some(value);
        }
    }
    None
}

impl El {
    /// Elements that stand apart as blocks in stripped text.
    #[rustfmt::skip]
    fn is_block(self) -> bool {
        use El::*;
        matches!(
            self,
            Address | Article | Aside | Blockquote | Br | Caption | Center | Details | Dialog | Dir
                | Div | Dl | Fieldset | Figcaption | Figure | Footer | Form | H1 | H2 | H3 | H4
                | H5 | H6 | Header | Hgroup | Hr | Listing | Main | Menu | Nav | Ol | P | Pre
                | Search | Section | Summary | Table | Ul
        )
    }

    /// Elements that start a line of their own in stripped text, without a
    /// blank line before it.
    fn is_line(self) -> bool {
        use El::*;
        matches!(self, Dd | Dt | Li | Option | Tr)
    }
}

/// Readable text from HTML by dropping tags, in one linear pass.
///
/// Script and style bodies and comments are dropped, blocks are set apart,
/// list items and table rows start lines, table cells are separated by
/// ` | `, entities are decoded, and runs of blank lines collapse. Tags are
/// split as html5ever splits them, so the text is the text the parser sees.
fn strip_tags(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len() / 2);
    let mut lexer = Lexer::new(bytes);
    let mut text_from = 0;
    while let Some(tag) = lexer.next_tag(false) {
        push_text(&mut out, &html[text_from..tag.start]);
        match tag.kind {
            TagKind::Start(start) => {
                start_break(&mut out, start.el);
                if matches!(start.el, El::Td | El::Th) {
                    let kept = out.trim_end_matches(' ').len();
                    out.truncate(kept);
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push_str(" | ");
                    }
                }
                if let Some(raw) = start.el.raw() {
                    let (from, to) = lexer.raw_content(start.name, raw);
                    if matches!(start.el, El::Title | El::Textarea | El::Plaintext) {
                        push_text(&mut out, &html[from..to]);
                        out.push('\n');
                    }
                }
            }
            TagKind::End(end) => start_break(&mut out, end.el),
            TagKind::Doctype { .. } | TagKind::Other => {}
        }
        text_from = lexer.at;
    }
    push_text(&mut out, &html[text_from.min(html.len())..]);
    collapse_blank_lines(&out)
}

/// The line break a start or end tag of `el` puts in stripped text: blocks
/// always break, so they stand apart; line elements only end a line.
fn start_break(out: &mut String, el: El) {
    if el.is_block() || (el.is_line() && !out.is_empty() && !out.ends_with('\n')) {
        out.push('\n');
    }
}

/// Append text with whitespace runs folded to one space and entities decoded.
fn push_text(out: &mut String, text: &str) {
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('&') {
            if let Some((decoded, used)) = decode_entity(after) {
                out.push(decoded);
                rest = &after[used..];
                continue;
            }
            out.push('&');
            rest = after;
            continue;
        }
        let mut chars = rest.chars();
        let c = chars.next().unwrap_or(' ');
        if c.is_whitespace() {
            if !out.ends_with(' ') && !out.ends_with('\n') && !out.is_empty() {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
        rest = chars.as_str();
    }
}

/// Decode the character reference at the start of `after` (the text after
/// `&`). Returns the character and how many bytes of `after` it used.
///
/// Numeric references and the named ones of HTML 4 are decoded; a name
/// needs its `;`. The search for the `;` is bounded, so text full of bare
/// `&` stays linear.
fn decode_entity(after: &str) -> Option<(char, usize)> {
    let end = after.as_bytes().iter().take(33).position(|&b| b == b';')?;
    let body = &after[..end];
    let c = if let Some(num) = body.strip_prefix('#') {
        let (digits, radix) = match num.strip_prefix(['x', 'X']) {
            Some(hex) => (hex, 16),
            None => (num, 10),
        };
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        let code = u32::from_str_radix(digits, radix).unwrap_or(u32::MAX);
        numeric_reference(code)
    } else {
        let at = ENTITIES
            .binary_search_by(|(name, _)| name.as_bytes().cmp(body.as_bytes()))
            .ok()?;
        match ENTITIES[at].1 {
            // A no-break space reads as a plain one.
            '\u{a0}' => ' ',
            c => c,
        }
    };
    Some((c, end + 1))
}

/// The character a numeric reference stands for, with the Windows-1252
/// remapping HTML applies to 0x80-0x9F.
fn numeric_reference(code: u32) -> char {
    const WINDOWS_1252: [u32; 32] = [
        0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160,
        0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013,
        0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
    ];
    let code = match code {
        0x80..=0x9F => WINDOWS_1252[(code - 0x80) as usize],
        code => code,
    };
    match char::from_u32(code) {
        Some('\0') | None => char::REPLACEMENT_CHARACTER,
        Some(c) => c,
    }
}

/// HTML 4's named character references, sorted by name for binary search.
#[rustfmt::skip]
const ENTITIES: &[(&str, char)] = &[
    ("AElig", 'Æ'), ("Aacute", 'Á'), ("Acirc", 'Â'), ("Agrave", 'À'), ("Alpha", 'Α'),
    ("Aring", 'Å'), ("Atilde", 'Ã'), ("Auml", 'Ä'), ("Beta", 'Β'), ("Ccedil", 'Ç'), ("Chi", 'Χ'),
    ("Dagger", '‡'), ("Delta", 'Δ'), ("ETH", 'Ð'), ("Eacute", 'É'), ("Ecirc", 'Ê'),
    ("Egrave", 'È'), ("Epsilon", 'Ε'), ("Eta", 'Η'), ("Euml", 'Ë'), ("Gamma", 'Γ'),
    ("Iacute", 'Í'), ("Icirc", 'Î'), ("Igrave", 'Ì'), ("Iota", 'Ι'), ("Iuml", 'Ï'), ("Kappa", 'Κ'),
    ("Lambda", 'Λ'), ("Mu", 'Μ'), ("Ntilde", 'Ñ'), ("Nu", 'Ν'), ("OElig", 'Œ'), ("Oacute", 'Ó'),
    ("Ocirc", 'Ô'), ("Ograve", 'Ò'), ("Omega", 'Ω'), ("Omicron", 'Ο'), ("Oslash", 'Ø'),
    ("Otilde", 'Õ'), ("Ouml", 'Ö'), ("Phi", 'Φ'), ("Pi", 'Π'), ("Prime", '″'), ("Psi", 'Ψ'),
    ("Rho", 'Ρ'), ("Scaron", 'Š'), ("Sigma", 'Σ'), ("THORN", 'Þ'), ("Tau", 'Τ'), ("Theta", 'Θ'),
    ("Uacute", 'Ú'), ("Ucirc", 'Û'), ("Ugrave", 'Ù'), ("Upsilon", 'Υ'), ("Uuml", 'Ü'), ("Xi", 'Ξ'),
    ("Yacute", 'Ý'), ("Yuml", 'Ÿ'), ("Zeta", 'Ζ'), ("aacute", 'á'), ("acirc", 'â'), ("acute", '´'),
    ("aelig", 'æ'), ("agrave", 'à'), ("alefsym", 'ℵ'), ("alpha", 'α'), ("amp", '&'), ("and", '∧'),
    ("ang", '∠'), ("apos", '\u{27}'), ("aring", 'å'), ("asymp", '≈'), ("atilde", 'ã'),
    ("auml", 'ä'), ("bdquo", '„'), ("beta", 'β'), ("brvbar", '¦'), ("bull", '•'), ("cap", '∩'),
    ("ccedil", 'ç'), ("cedil", '¸'), ("cent", '¢'), ("chi", 'χ'), ("circ", 'ˆ'), ("clubs", '♣'),
    ("cong", '≅'), ("copy", '©'), ("crarr", '↵'), ("cup", '∪'), ("curren", '¤'), ("dArr", '⇓'),
    ("dagger", '†'), ("darr", '↓'), ("deg", '°'), ("delta", 'δ'), ("diams", '♦'), ("divide", '÷'),
    ("eacute", 'é'), ("ecirc", 'ê'), ("egrave", 'è'), ("empty", '∅'), ("emsp", '\u{2003}'),
    ("ensp", '\u{2002}'), ("epsilon", 'ε'), ("equiv", '≡'), ("eta", 'η'), ("eth", 'ð'),
    ("euml", 'ë'), ("euro", '€'), ("exist", '∃'), ("fnof", 'ƒ'), ("forall", '∀'), ("frac12", '½'),
    ("frac14", '¼'), ("frac34", '¾'), ("frasl", '⁄'), ("gamma", 'γ'), ("ge", '≥'), ("gt", '>'),
    ("hArr", '⇔'), ("harr", '↔'), ("hearts", '♥'), ("hellip", '…'), ("iacute", 'í'),
    ("icirc", 'î'), ("iexcl", '¡'), ("igrave", 'ì'), ("image", 'ℑ'), ("infin", '∞'), ("int", '∫'),
    ("iota", 'ι'), ("iquest", '¿'), ("isin", '∈'), ("iuml", 'ï'), ("kappa", 'κ'), ("lArr", '⇐'),
    ("lambda", 'λ'), ("lang", '〈'), ("laquo", '«'), ("larr", '←'), ("lceil", '⌈'), ("ldquo", '“'),
    ("le", '≤'), ("lfloor", '⌊'), ("lowast", '∗'), ("loz", '◊'), ("lrm", '\u{200e}'),
    ("lsaquo", '‹'), ("lsquo", '‘'), ("lt", '<'), ("macr", '¯'), ("mdash", '—'), ("micro", 'µ'),
    ("middot", '·'), ("minus", '−'), ("mu", 'μ'), ("nabla", '∇'), ("nbsp", '\u{a0}'),
    ("ndash", '–'), ("ne", '≠'), ("ni", '∋'), ("not", '¬'), ("notin", '∉'), ("nsub", '⊄'),
    ("ntilde", 'ñ'), ("nu", 'ν'), ("oacute", 'ó'), ("ocirc", 'ô'), ("oelig", 'œ'), ("ograve", 'ò'),
    ("oline", '‾'), ("omega", 'ω'), ("omicron", 'ο'), ("oplus", '⊕'), ("or", '∨'), ("ordf", 'ª'),
    ("ordm", 'º'), ("oslash", 'ø'), ("otilde", 'õ'), ("otimes", '⊗'), ("ouml", 'ö'), ("para", '¶'),
    ("part", '∂'), ("permil", '‰'), ("perp", '⊥'), ("phi", 'φ'), ("pi", 'π'), ("piv", 'ϖ'),
    ("plusmn", '±'), ("pound", '£'), ("prime", '′'), ("prod", '∏'), ("prop", '∝'), ("psi", 'ψ'),
    ("quot", '"'), ("rArr", '⇒'), ("radic", '√'), ("rang", '〉'), ("raquo", '»'), ("rarr", '→'),
    ("rceil", '⌉'), ("rdquo", '”'), ("real", 'ℜ'), ("reg", '®'), ("rfloor", '⌋'), ("rho", 'ρ'),
    ("rlm", '\u{200f}'), ("rsaquo", '›'), ("rsquo", '’'), ("sbquo", '‚'), ("scaron", 'š'),
    ("sdot", '⋅'), ("sect", '§'), ("shy", '\u{ad}'), ("sigma", 'σ'), ("sigmaf", 'ς'), ("sim", '∼'),
    ("spades", '♠'), ("sub", '⊂'), ("sube", '⊆'), ("sum", '∑'), ("sup", '⊃'), ("sup1", '¹'),
    ("sup2", '²'), ("sup3", '³'), ("supe", '⊇'), ("szlig", 'ß'), ("tau", 'τ'), ("there4", '∴'),
    ("theta", 'θ'), ("thetasym", 'ϑ'), ("thinsp", '\u{2009}'), ("thorn", 'þ'), ("tilde", '˜'),
    ("times", '×'), ("trade", '™'), ("uArr", '⇑'), ("uacute", 'ú'), ("uarr", '↑'), ("ucirc", 'û'),
    ("ugrave", 'ù'), ("uml", '¨'), ("upsih", 'ϒ'), ("upsilon", 'υ'), ("uuml", 'ü'),
    ("weierp", '℘'), ("xi", 'ξ'), ("yacute", 'ý'), ("yen", '¥'), ("yuml", 'ÿ'), ("zeta", 'ζ'),
    ("zwj", '\u{200d}'), ("zwnj", '\u{200c}'),
];

/// Trim each line's edges and keep at most one blank line in a row.
fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 || out.is_empty() {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    let trimmed = out.trim_end().len();
    out.truncate(trimmed);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nesting_depth_exceeds(html: &[u8], limit: usize) -> bool {
        nesting(html, limit) == Nesting::Deep
    }

    #[test]
    fn test_extract_with_tagged_html_returns_stripped_text() {
        let html = b"<html><body><h1>Title</h1><p>Hello <b>world</b></p></body></html>";
        let got = extract(html).expect("extract");
        assert!(!got.contains("<h1>"), "{got:?}");
        assert!(!got.contains("<p>"), "{got:?}");
        assert!(!got.contains("<b>"), "{got:?}");
        assert!(got.contains("Title"), "{got:?}");
        assert!(got.contains("Hello"), "{got:?}");
        assert!(got.contains("world"), "{got:?}");
    }

    #[test]
    fn test_extract_with_deeply_nested_divs_returns_text_without_html2text() {
        let depth = 20_000;
        let html = format!(
            "{}deep text &amp; more{}",
            "<div>".repeat(depth),
            "</div>".repeat(depth)
        );
        assert!(nesting_depth_exceeds(html.as_bytes(), MAX_HTML2TEXT_DEPTH));
        let got = extract(html.as_bytes()).expect("extract");
        assert_eq!(got, "deep text & more");
    }

    #[test]
    fn test_extract_with_unclosed_formatting_tags_returns_text_without_html2text() {
        let html = format!("<p>{}</p>", "<b><i><u><s><em>t".repeat(3000));
        assert!(nesting_depth_exceeds(html.as_bytes(), MAX_HTML2TEXT_DEPTH));
        let got = extract(html.as_bytes()).expect("extract");
        assert_eq!(got, "t".repeat(3000));
    }

    #[test]
    fn test_nesting_depth_exceeds_with_flat_real_world_markup_returns_false() {
        let page = format!(
            "<!doctype html><html><head><title>x < y</title><meta charset=utf-8>\
             <script>if (a < b && c > d) {{ document.write('<div>'); }}</script></head>\
             <body>{}<ul>{}</ul><img src=a.png><br><input value='>'></body></html>",
            "<p>para".repeat(5000),
            "<li>item".repeat(5000),
        );
        assert!(!nesting_depth_exceeds(page.as_bytes(), 16));
    }

    /// Levels comfortably past any depth limit worth choosing.
    const DEEP: usize = 3 * 512;

    #[test]
    fn test_nesting_depth_exceeds_with_self_closed_non_void_tags_returns_true() {
        let divs = "<div/>t".repeat(DEEP);
        assert!(nesting_depth_exceeds(divs.as_bytes(), MAX_HTML2TEXT_DEPTH));
        let inline = "<b/><i/><u/><s/><em/>t".repeat(DEEP / 5);
        assert!(nesting_depth_exceeds(
            inline.as_bytes(),
            MAX_HTML2TEXT_DEPTH
        ));
    }

    #[test]
    fn test_nesting_depth_exceeds_with_unmatched_end_tags_returns_true() {
        let html = "<b></x><div></span>t".repeat(DEEP);
        assert!(nesting_depth_exceeds(html.as_bytes(), MAX_HTML2TEXT_DEPTH));
    }

    #[test]
    fn test_nesting_depth_exceeds_with_li_dd_and_rt_chains_returns_true() {
        let lists = "<li><dd>t".repeat(DEEP);
        assert!(nesting_depth_exceeds(lists.as_bytes(), MAX_HTML2TEXT_DEPTH));
        let ruby = "<rt>t".repeat(DEEP);
        assert!(nesting_depth_exceeds(ruby.as_bytes(), MAX_HTML2TEXT_DEPTH));
    }

    #[test]
    fn test_nesting_depth_exceeds_with_apostrophe_in_unquoted_attribute_returns_true() {
        let html = format!("<div title=it's>{}t", "<s>".repeat(DEEP));
        assert!(nesting_depth_exceeds(html.as_bytes(), MAX_HTML2TEXT_DEPTH));
    }

    #[test]
    fn test_nesting_depth_exceeds_with_bang_comment_close_returns_true() {
        let html = format!("<!-- note --!>{}t", "<s>".repeat(DEEP));
        assert!(nesting_depth_exceeds(html.as_bytes(), MAX_HTML2TEXT_DEPTH));
    }

    #[test]
    fn test_nesting_depth_exceeds_with_flat_table_of_unclosed_rows_returns_false() {
        let html = format!("<table>{}</table>", "<tr><td>cell".repeat(5000));
        assert!(!nesting_depth_exceeds(html.as_bytes(), MAX_HTML2TEXT_DEPTH));
    }

    #[test]
    fn test_nesting_with_formatting_reopened_before_every_paragraph_returns_deep() {
        // Every `<b>` closed with the `<div>` is reopened in each paragraph,
        // so html5ever builds 17 elements for every 8 bytes.
        let opened: String = (0..16).map(|i| format!("<b id={i}>")).collect();
        let html = format!("<div>{opened}</div>{}", "<p>t</p>".repeat(2000));
        assert_eq!(nesting(html.as_bytes(), MAX_HTML2TEXT_DEPTH), Nesting::Deep);
        let got = extract(html.as_bytes()).expect("extract");
        assert_eq!(got.matches('t').count(), 2000);
    }

    #[test]
    fn test_extract_with_strikeout_nested_under_depth_limit_keeps_output_linear() {
        let text = "t".repeat(4096);
        let html = format!("{}{text}", "<s>".repeat(MAX_HTML2TEXT_DEPTH - 8));
        assert!(!nesting_depth_exceeds(html.as_bytes(), MAX_HTML2TEXT_DEPTH));
        let got = extract(html.as_bytes()).expect("extract");
        assert!(!got.contains('\u{336}'), "strikeout marks in output");
        assert_eq!(got.chars().filter(|c| *c == 't').count(), text.len());
        assert!(got.len() < 2 * text.len(), "{} bytes out", got.len());
    }

    #[test]
    fn test_strip_tags_with_megabyte_of_bare_ampersands_returns_them_quickly() {
        let html = format!("<p>{}</p>", "&".repeat(1024 * 1024));
        let started = std::time::Instant::now();
        let got = strip_tags(&html);
        assert_eq!(got.len(), 1024 * 1024);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn test_extract_with_utf16_html_returns_text_without_markup() {
        let page = "<html><head><title>Menu</title></head><body><p>Caf&eacute; &amp; tea</p>\
                    <script>var hidden = 1;</script></body></html>";
        let mut le = vec![0xFF, 0xFE];
        le.extend(page.encode_utf16().flat_map(u16::to_le_bytes));
        let be: Vec<u8> = page.encode_utf16().flat_map(u16::to_be_bytes).collect();
        for bytes in [le, be] {
            let got = extract(&bytes).expect("extract");
            assert!(got.contains("Café & tea"), "{got:?}");
            for markup in ["<p>", "&amp;", "var hidden", "\u{fffd}"] {
                assert!(!got.contains(markup), "{markup:?} in {got:?}");
            }
        }
    }

    #[test]
    fn test_strip_tags_with_table_cells_entities_and_fake_script_end_returns_readable_text() {
        let html = "<table><tr><th>Name</th><th>Age</th></tr><tr><td>Ada</td> <td>36</td></tr>\
                    </table><p>&eacute; &mdash; &copy; &hellip; &rsquo; &#8364; &#x2014;</p>\
                    <script>var s = \"</scripts> <b>not text</b>\";</script><p>tail</p>";
        assert_eq!(
            strip_tags(html),
            "Name | Age\nAda | 36\n\né — © … ’ € —\n\ntail"
        );
    }

    #[test]
    fn test_decode_entity_with_every_table_name_finds_it_by_binary_search() {
        assert!(ENTITIES.windows(2).all(|pair| pair[0].0 < pair[1].0));
        for (name, c) in ENTITIES {
            let expected = if *c == '\u{a0}' { ' ' } else { *c };
            assert_eq!(
                decode_entity(&format!("{name};")),
                Some((expected, name.len() + 1))
            );
        }
    }

    #[test]
    fn test_strip_tags_with_script_style_comments_and_entities_returns_readable_text() {
        let html = "<html><head><style>p { color: red }</style><title>T&#233;st</title></head>\
                    <body><!-- hidden <p>nope</p> --><h1>Head</h1><p>one&nbsp;two &lt;tag&gt; \
                    &unknown; a < b</p><script>var x = '<p>';</script><div>last</div></body></html>";
        assert_eq!(
            strip_tags(html),
            "Tést\n\nHead\n\none two <tag> &unknown; a < b\n\nlast"
        );
    }

    /// Every tag the model has a rule for, in lowercase and in SVG's case.
    const NAMES: &str = "a b i s u em strong font nobr span div p li ul ol dd dt dl table tr td \
                         th tbody thead tfoot caption colgroup col select option optgroup rt rp \
                         rb rtc ruby h1 h2 button form template svg math path g foreignObject \
                         desc title mi mtext annotation-xml mglyph applet object marquee pre \
                         address x custom-el br img hr input body html head script style \
                         textarea xmp iframe noscript noembed noframes image menu center \
                         blockquote code small big tt strike sub sup var section main article \
                         listing summary details frameset frame";

    /// xorshift64, so the generated pages are the same on every run.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    /// A page of a few runs of random tags, text, and comments, some runs
    /// repeated many times over to build depth.
    fn generated_page(rng: &mut Rng, names: &[&str]) -> String {
        let attrs = [
            "",
            " id=7",
            " class=x",
            " color=red",
            " encoding=text/html",
            " title=it's",
            " a=\"x>y\"",
            " type=hidden",
        ];
        let other = [
            "t",
            " ",
            "<!-- c -->",
            "<!-- c --!>",
            "<!-->",
            "<![CDATA[ <div> ]]>",
            "</sarcasm>",
        ];
        let mut page = String::new();
        if rng.below(2) == 0 {
            page.push_str("<!DOCTYPE html>");
        }
        for _ in 0..1 + rng.below(4) {
            let mut run = String::new();
            for _ in 0..1 + rng.below(8) {
                let name = names[rng.below(names.len())];
                match rng.below(10) {
                    0..=4 => {
                        let attr = attrs[rng.below(attrs.len())];
                        let slash = if rng.below(6) == 0 { "/" } else { "" };
                        run.push_str(&format!("<{name}{attr}{slash}>"));
                    }
                    5..=7 => run.push_str(&format!("</{name}>")),
                    _ => run.push_str(other[rng.below(other.len())]),
                }
            }
            let times = if rng.below(3) == 0 {
                1 + rng.below(120)
            } else {
                1
            };
            page.push_str(&run.repeat(times));
        }
        page
    }

    /// The most elements the model holds open at once, with no limit.
    fn model_depth(html: &[u8]) -> usize {
        let mut model = Model::new(html, usize::MAX);
        let mut lexer = Lexer::new(html);
        let mut text_from = 0;
        let mut deepest = 0;
        while let Some(tag) = lexer.next_tag(model.in_foreign()) {
            if tag.start > text_from {
                model.text(&html[text_from..tag.start]);
                deepest = deepest.max(model.stack.len());
            }
            match tag.kind {
                TagKind::Start(start) => {
                    if let Some(raw) = model.start_tag(start) {
                        lexer.skip_raw(start.name, raw);
                    }
                }
                TagKind::End(end) => model.end_tag(end),
                TagKind::Doctype { html } => model.doctype(html),
                TagKind::Other => {}
            }
            deepest = deepest.max(model.stack.len());
            text_from = lexer.at;
        }
        if text_from < html.len() {
            model.text(&html[text_from..]);
        }
        deepest.max(model.stack.len())
    }

    /// The depth of the element tree html5ever builds for html2text,
    /// template contents included.
    fn tree_depth(html: &[u8]) -> usize {
        let dom = html2text::config::plain().parse_html(html).unwrap();
        let mut deepest = 0;
        let mut todo = vec![(dom.document.clone(), 0usize)];
        while let Some((node, depth)) = todo.pop() {
            let depth = match node.data {
                html2text::Element {
                    ref template_contents,
                    ..
                } => {
                    if let Some(contents) = template_contents.borrow().as_ref() {
                        todo.push((contents.clone(), depth + 1));
                    }
                    depth + 1
                }
                _ => depth,
            };
            deepest = deepest.max(depth);
            for child in node.children.borrow().iter() {
                todo.push((child.clone(), depth));
            }
        }
        deepest
    }

    #[test]
    fn test_nesting_with_generated_pages_never_trails_html5ever() {
        let names: Vec<&str> = NAMES.split_whitespace().collect();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..1000 {
            let html = generated_page(&mut rng, &names);
            let model = model_depth(html.as_bytes());
            let tree = tree_depth(html.as_bytes());
            // Past `<html>`, `<body>`, and a leaf that is never pushed, the
            // tree may run deeper than the stack only where an element left
            // the stack but stayed in the tree above what opened inside it
            // (a closed `<form>`, a misnested `<a>`); that at most doubles it.
            assert!(
                tree <= 2 * (model + 3),
                "tree {tree}, model {model}: {}",
                &html[..html.len().min(2000)]
            );
        }
    }
}
