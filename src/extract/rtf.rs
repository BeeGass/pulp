//! RTF to plain text.
//!
//! A small reader for the reader rules of the RTF 1.9.1 specification. It
//! makes one pass over the bytes and keeps group state on an explicit stack,
//! so neither deep nesting nor a long file can exhaust the call stack or take
//! more than linear time.

use std::borrow::Cow;
use std::collections::HashMap;

use encoding_rs::Encoding;

use crate::error::Error;

/// Deepest group nesting that keeps its own state. Groups nested deeper are
/// skipped whole; real documents nest a few dozen levels at most.
const MAX_DEPTH: usize = 1024;

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Extract plain text from RTF.
///
/// Paragraphs and line breaks end lines, table cells are separated by tabs
/// with one row per line, and footnotes follow the body. Input that does not
/// open with `{\rtf` is not RTF and comes back as decoded bytes.
pub fn extract(bytes: &[u8]) -> Result<String, Error> {
    if let Some(start) = rtf_start(bytes) {
        return Ok(convert(&bytes[start..]));
    }
    let decoded = super::text::decode_bytes(bytes);
    // UTF-16 and UTF-32 files read as RTF only once decoded.
    Ok(match rtf_start(decoded.as_bytes()) {
        Some(start) => convert(&decoded.as_bytes()[start..]),
        None => decoded,
    })
}

/// Offset of the `{\rtf` that opens a document, past a UTF-8 byte order mark
/// and whitespace.
fn rtf_start(bytes: &[u8]) -> Option<usize> {
    let bom = if bytes.starts_with(UTF8_BOM) {
        UTF8_BOM.len()
    } else {
        0
    };
    let start = bom + count_while(&bytes[bom..], u8::is_ascii_whitespace);
    bytes[start..].starts_with(br"{\rtf").then_some(start)
}

/// Convert a document that starts at its opening `{\rtf`.
fn convert(rtf: &[u8]) -> String {
    // RTF is 7-bit, but some writers put UTF-8 text in it as is. When the
    // whole file is valid UTF-8, its raw non-ASCII bytes are read that way.
    let mut reader = Reader::new(std::str::from_utf8(rtf).is_ok());
    for token in Tokens::new(rtf) {
        if !reader.feed(token) {
            break;
        }
    }
    reader.finish()
}

/// One lexical unit of RTF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    /// `{`
    Open,
    /// `}`
    Close,
    /// A control word and its numeric parameter, such as `\par` or `\f1`.
    Word(&'a [u8], Option<i32>),
    /// A backslash and one ASCII character that is not a letter, such as `\~`.
    Symbol(u8),
    /// A byte written as `\'hh`.
    Byte(u8),
    /// Literal text.
    Text(&'a [u8]),
}

/// Splits RTF into tokens.
///
/// Raw line breaks and other control bytes except tab are dropped here. So
/// are the bytes after `\binN`: binary data may hold braces and backslashes,
/// and must never be read as markup, whatever group it sits in.
struct Tokens<'a> {
    rtf: &'a [u8],
    pos: usize,
}

impl<'a> Tokens<'a> {
    fn new(rtf: &'a [u8]) -> Self {
        Self { rtf, pos: 0 }
    }

    /// Read what follows a backslash. Returns `None` when that is the end of
    /// the input, a non-ASCII byte (left to be read as text), or a `\'`
    /// without two hex digits.
    fn control(&mut self) -> Option<Token<'a>> {
        let rtf = self.rtf;
        let first = *rtf.get(self.pos)?;
        if first.is_ascii_alphabetic() {
            return Some(self.word());
        }
        if !first.is_ascii() {
            return None;
        }
        self.pos += 1;
        match first {
            b'\'' => {
                let high = hex_digit(*rtf.get(self.pos)?)?;
                let low = hex_digit(*rtf.get(self.pos + 1)?)?;
                self.pos += 2;
                Some(Token::Byte((high << 4) | low))
            }
            // A backslash before a line break stands for `\par`.
            b'\r' | b'\n' => Some(Token::Word(b"par", None)),
            _ => Some(Token::Symbol(first)),
        }
    }

    /// Read a control word: ASCII letters, an optional signed decimal
    /// parameter, and an optional space that belongs to the word.
    fn word(&mut self) -> Token<'a> {
        let rtf = self.rtf;
        let start = self.pos;
        let name_end = start + count_while(&rtf[start..], u8::is_ascii_alphabetic);
        let name = &rtf[start..name_end];
        let negative = rtf.get(name_end) == Some(&b'-')
            && rtf.get(name_end + 1).is_some_and(u8::is_ascii_digit);
        let digits_start = name_end + usize::from(negative);
        let digits_end = digits_start + count_while(&rtf[digits_start..], u8::is_ascii_digit);
        let param = (digits_end > digits_start).then(|| {
            let value = rtf[digits_start..digits_end]
                .iter()
                .fold(0i32, |value, digit| {
                    value
                        .saturating_mul(10)
                        .saturating_add(i32::from(digit - b'0'))
                });
            if negative { -value } else { value }
        });
        self.pos = digits_end;
        if rtf.get(self.pos) == Some(&b' ') {
            self.pos += 1;
        }
        if name == b"bin" {
            let len = param.map_or(0, |len| usize::try_from(len).unwrap_or(0));
            self.pos = self.pos.saturating_add(len).min(rtf.len());
        }
        Token::Word(name, param)
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        let rtf = self.rtf;
        loop {
            let byte = *rtf.get(self.pos)?;
            self.pos += 1;
            match byte {
                b'{' => return Some(Token::Open),
                b'}' => return Some(Token::Close),
                b'\\' => {
                    if let Some(token) = self.control() {
                        return Some(token);
                    }
                }
                _ if is_text(byte) => {
                    let start = self.pos - 1;
                    self.pos += count_while(&rtf[self.pos..], |&b| is_text(b));
                    return Some(Token::Text(&rtf[start..self.pos]));
                }
                _ => {}
            }
        }
    }
}

/// Whether a byte is literal text: not markup, and not a control byte other
/// than tab.
fn is_text(byte: u8) -> bool {
    !matches!(byte, b'{' | b'}' | b'\\') && (byte >= b' ' || byte == b'\t')
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn count_while(bytes: &[u8], keep: impl Fn(&u8) -> bool) -> usize {
    bytes.iter().take_while(|&byte| keep(byte)).count()
}

/// Where a group's text goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dest {
    /// Document text.
    Body,
    /// Footnote text. Footnotes are kept but set apart and appended after the
    /// body, each under its number: in place, a note would land in the middle
    /// of a sentence or a table row.
    Note,
    /// The font table, read only for each font's code page.
    FontTable,
    /// Anything that is not document text.
    Skip,
    /// An attachment in an Apple RTFD document, `{\NeXTGraphic name ...}`.
    /// Its text is a file name, and the character after the group stands in
    /// for the attachment; both are dropped.
    Attachment,
    /// A list item's number or bullet (`\listtext`, `\pntext`), which writers
    /// render for readers without list tables. It is written as its words
    /// and one space, `1. `, since a tab would read as a table cell.
    Marker,
}

/// Reader state scoped to one group.
#[derive(Debug, Clone, Copy)]
struct State {
    dest: Dest,
    /// Fallback characters after each `\u`, from `\ucN`.
    uc: usize,
    /// Font from `\fN`; `None` means the default font.
    font: Option<i32>,
}

/// What a font table entry says about its code page.
#[derive(Debug, Clone, Copy, Default)]
struct Font {
    /// From `\fcharsetN`.
    charset: Option<CodePage>,
    /// From `\cpgN`, which names the code page outright.
    cpg: Option<CodePage>,
}

impl Font {
    fn code_page(&self) -> Option<CodePage> {
        self.cpg.or(self.charset)
    }
}

/// Turns tokens into text.
struct Reader {
    /// Raw non-ASCII bytes are UTF-8 rather than code page bytes.
    utf8: bool,
    state: State,
    /// States of the enclosing groups, innermost last.
    saved: Vec<State>,
    /// Groups open past [`MAX_DEPTH`], all skipped.
    overflow: usize,
    /// The previous token was `\*`.
    star: bool,
    fonts: HashMap<i32, Font>,
    /// Font whose entry the font table is reading.
    table_font: Option<i32>,
    /// From `\deffN`.
    default_font: Option<i32>,
    /// From `\ansicpgN`.
    ansi_cpg: Option<CodePage>,
    /// From `\ansi`, `\mac`, `\pc` or `\pca`.
    charset: CodePage,
    /// Code page bytes not yet decoded. A run of them decodes as one, so a
    /// double-byte character written as two `\'hh` stays whole.
    pending: Vec<u8>,
    /// Characters still to skip: the fallback after a `\u`, or the stand-in
    /// after an attachment.
    skip: usize,
    /// High surrogate from a `\u`, waiting for the low one.
    high_surrogate: Option<u32>,
    body: String,
    notes: String,
    note_count: usize,
    /// Text of the list marker being read.
    marker: String,
}

impl Reader {
    fn new(utf8: bool) -> Self {
        Self {
            utf8,
            state: State {
                dest: Dest::Body,
                uc: 1,
                font: None,
            },
            saved: Vec::new(),
            overflow: 0,
            star: false,
            fonts: HashMap::new(),
            table_font: None,
            default_font: None,
            ansi_cpg: None,
            charset: CodePage::Encoding(encoding_rs::WINDOWS_1252),
            pending: Vec::new(),
            skip: 0,
            high_surrogate: None,
            body: String::new(),
            notes: String::new(),
            note_count: 0,
            marker: String::new(),
        }
    }

    /// Apply one token. Returns `false` once the document's outer group has
    /// closed; anything after it is not part of the document.
    fn feed(&mut self, token: Token<'_>) -> bool {
        if self.overflow > 0 {
            match token {
                Token::Open => self.overflow += 1,
                Token::Close => self.overflow -= 1,
                _ => {}
            }
            return true;
        }
        // `\*` marks a destination to skip unless understood. The one read
        // here is `\nesttableprops`: it holds the `\nestrow` that ends a
        // nested table row, and no text.
        if std::mem::take(&mut self.star) && !matches!(token, Token::Word(b"nesttableprops", _)) {
            self.state.dest = Dest::Skip;
        }
        match token {
            Token::Open => self.open_group(),
            Token::Close => return self.close_group(),
            Token::Word(name, param) => self.word(name, param),
            Token::Symbol(symbol) => self.symbol(symbol),
            Token::Byte(byte) => self.byte(byte),
            Token::Text(text) => self.text(text),
        }
        true
    }

    fn open_group(&mut self) {
        self.flush();
        // A brace ends `\u` fallback text early.
        self.skip = 0;
        if self.saved.len() < MAX_DEPTH {
            self.saved.push(self.state);
        } else {
            self.overflow = 1;
        }
    }

    /// Close a group. Returns `false` when it was the outermost one.
    fn close_group(&mut self) -> bool {
        self.flush();
        let closed = self.state.dest;
        if let Some(state) = self.saved.pop() {
            self.state = state;
        }
        // A brace ends `\u` fallback text early.
        self.skip = 0;
        if closed != self.state.dest {
            match closed {
                // The character after an attachment stands in for it, and is
                // skipped the way a fallback is.
                Dest::Attachment => self.skip = 1,
                Dest::Marker => self.end_marker(),
                _ => {}
            }
        }
        !self.saved.is_empty()
    }

    fn word(&mut self, name: &[u8], param: Option<i32>) {
        self.flush();
        if name == b"u" {
            if let Some(value) = param {
                self.unicode(value);
            }
            return;
        }
        // The spec counts a control word as one fallback character. Here it
        // ends the fallback instead, as in macOS textutil, so a writer that
        // leaves the fallback out does not cost the text a `\par` or `\cell`.
        self.skip = 0;
        if name == b"uc" {
            if let Some(count) = param {
                self.state.uc = usize::try_from(count).unwrap_or(0);
            }
        } else if is_skipped_destination(name) {
            self.state.dest = Dest::Skip;
        } else {
            match self.state.dest {
                Dest::Body | Dest::Note | Dest::Marker => self.text_word(name, param),
                Dest::FontTable => self.font_table_word(name, param),
                Dest::Skip | Dest::Attachment => {}
            }
        }
    }

    /// A control word in document or footnote text.
    fn text_word(&mut self, name: &[u8], param: Option<i32>) {
        match name {
            b"par" | b"sect" | b"page" | b"line" | b"column" => self.put('\n'),
            b"tab" | b"cell" | b"nestcell" => self.put('\t'),
            b"row" | b"nestrow" => self.end_row(),
            b"emdash" => self.put('\u{2014}'),
            b"endash" => self.put('\u{2013}'),
            b"bullet" => self.put('\u{2022}'),
            b"lquote" => self.put('\u{2018}'),
            b"rquote" => self.put('\u{2019}'),
            b"ldblquote" => self.put('\u{201c}'),
            b"rdblquote" => self.put('\u{201d}'),
            b"emspace" => self.put('\u{2003}'),
            b"enspace" => self.put('\u{2002}'),
            b"qmspace" => self.put('\u{2005}'),
            // Joiners change how some scripts read. The direction marks
            // `\ltrmark` and `\rtlmark` do not, and are dropped.
            b"zwj" => self.put('\u{200d}'),
            b"zwnj" => self.put('\u{200c}'),
            b"chftn" => self.footnote_number(),
            b"footnote" => self.start_footnote(),
            b"fonttbl" => self.state.dest = Dest::FontTable,
            b"NeXTGraphic" => self.state.dest = Dest::Attachment,
            b"listtext" | b"pntext" => self.state.dest = Dest::Marker,
            b"f" => self.state.font = param,
            b"plain" => self.state.font = None,
            b"deff" => self.default_font = param,
            b"ansicpg" => self.ansi_cpg = param.and_then(code_page),
            b"ansi" => self.charset = CodePage::Encoding(encoding_rs::WINDOWS_1252),
            b"mac" => self.charset = CodePage::Encoding(encoding_rs::MACINTOSH),
            b"pc" => self.charset = CodePage::Table(&CP437),
            b"pca" => self.charset = CodePage::Table(&CP850),
            _ => {}
        }
    }

    /// A control word in the font table, where `\fN` starts an entry.
    fn font_table_word(&mut self, name: &[u8], param: Option<i32>) {
        match name {
            b"f" => {
                self.table_font = param;
                if let Some(font) = param {
                    self.fonts.insert(font, Font::default());
                }
            }
            b"fcharset" => {
                if let Some(font) = self.table_entry() {
                    font.charset = param.and_then(charset_code_page);
                }
            }
            b"cpg" => {
                if let Some(font) = self.table_entry() {
                    font.cpg = param.and_then(code_page);
                }
            }
            _ => {}
        }
    }

    fn table_entry(&mut self) -> Option<&mut Font> {
        self.fonts.get_mut(&self.table_font?)
    }

    fn symbol(&mut self, symbol: u8) {
        if matches!(symbol, b'\\' | b'{' | b'}') {
            // Escaped literals are text, and can be the second byte of a
            // double-byte character.
            self.text(&[symbol]);
            return;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        self.flush();
        match symbol {
            b'*' => self.star = true,
            b'~' => self.put('\u{a0}'),
            b'_' => self.put('\u{2011}'),
            // `\-` marks where a word may break; `\:` and `\|` belong to
            // index entries and formulas.
            _ => {}
        }
    }

    fn byte(&mut self, byte: u8) {
        if self.skip > 0 {
            self.skip -= 1;
        } else if self.writes_text() {
            self.pending.push(byte);
        }
    }

    fn text(&mut self, text: &[u8]) {
        let mut rest = self.skip_fallback(text);
        if !self.writes_text() {
            return;
        }
        while !rest.is_empty() {
            // ASCII joins the code page bytes, where it may finish a
            // double-byte character begun by `\'hh`. So does every other
            // byte, unless the input is UTF-8.
            let coded = if self.utf8 {
                count_while(rest, u8::is_ascii)
            } else {
                rest.len()
            };
            self.pending.extend_from_slice(&rest[..coded]);
            rest = &rest[coded..];
            let wide = count_while(rest, |byte| !byte.is_ascii());
            if wide > 0 {
                self.flush();
                self.put_str(&String::from_utf8_lossy(&rest[..wide]));
                rest = &rest[wide..];
            }
        }
    }

    /// Drop the `\u` fallback characters that start a text run.
    fn skip_fallback<'t>(&mut self, text: &'t [u8]) -> &'t [u8] {
        let mut at = 0;
        while self.skip > 0 && at < text.len() {
            at += 1;
            if self.utf8 {
                // A UTF-8 character is one fallback character, not one per byte.
                at += count_while(&text[at..], |byte| (0x80..0xC0).contains(byte));
            }
            self.skip -= 1;
        }
        &text[at..]
    }

    /// Write the character a `\uN` names, and skip its fallback.
    fn unicode(&mut self, value: i32) {
        self.skip = self.state.uc;
        if !self.writes_text() {
            return;
        }
        // N is a signed 16-bit code unit. A value past U+FFFF, which some
        // writers use instead of a surrogate pair, is taken as a code point.
        let unit = if value < 0 { value + 0x10000 } else { value };
        let Ok(unit) = u32::try_from(unit) else {
            self.put(char::REPLACEMENT_CHARACTER);
            return;
        };
        match unit {
            0xD800..=0xDBFF => {
                self.end_surrogate();
                self.high_surrogate = Some(unit);
            }
            0xDC00..=0xDFFF => {
                let pair = self.high_surrogate.take().and_then(|high| {
                    char::from_u32(0x10000 + ((high - 0xD800) << 10) + (unit - 0xDC00))
                });
                self.put(pair.unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            _ => self.put(char::from_u32(unit).unwrap_or(char::REPLACEMENT_CHARACTER)),
        }
    }

    /// End a table row: drop the tab after its last cell and end the line,
    /// unless a nested row has just ended it.
    fn end_row(&mut self) {
        self.end_surrogate();
        let sink = self.sink();
        if sink.ends_with('\t') {
            sink.pop();
        }
        if !sink.ends_with('\n') {
            sink.push('\n');
        }
    }

    fn start_footnote(&mut self) {
        self.note_count += 1;
        self.state.dest = Dest::Note;
        if !self.notes.is_empty() && !self.notes.ends_with('\n') {
            self.notes.push('\n');
        }
    }

    /// `\chftn`, the automatic footnote number. In the body it refers to the
    /// footnote that follows; inside a footnote, to that footnote.
    fn footnote_number(&mut self) {
        let number = match self.state.dest {
            Dest::Note => self.note_count,
            _ => self.note_count + 1,
        };
        self.put_str(&format!("[{number}]"));
    }

    /// Write the list marker just read as its words and one space, so `1.`
    /// and a tab reads `1. `, and a bullet between tabs reads `• `.
    fn end_marker(&mut self) {
        let marker = std::mem::take(&mut self.marker);
        let words: Vec<&str> = marker.split_whitespace().collect();
        if !words.is_empty() {
            self.put_str(&words.join(" "));
            self.put(' ');
        }
    }

    fn writes_text(&self) -> bool {
        matches!(self.state.dest, Dest::Body | Dest::Note | Dest::Marker)
    }

    fn sink(&mut self) -> &mut String {
        match self.state.dest {
            Dest::Note => &mut self.notes,
            Dest::Marker => &mut self.marker,
            _ => &mut self.body,
        }
    }

    /// Write one character to the current destination. Control characters
    /// other than tab and newline are dropped.
    fn put(&mut self, c: char) {
        if !self.writes_text() {
            return;
        }
        self.end_surrogate();
        // macOS writes a line break within a paragraph as U+2028, which plain
        // text tools do not take for a line end.
        let c = if matches!(c, '\u{2028}' | '\u{2029}') {
            '\n'
        } else {
            c
        };
        if !c.is_control() || c == '\t' || c == '\n' {
            self.sink().push(c);
        }
    }

    fn put_str(&mut self, text: &str) {
        for c in text.chars() {
            self.put(c);
        }
    }

    /// A high surrogate that no low one followed stands for nothing.
    fn end_surrogate(&mut self) {
        if self.high_surrogate.take().is_some() {
            self.sink().push(char::REPLACEMENT_CHARACTER);
        }
    }

    /// Decode the pending code page bytes.
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let mut bytes = std::mem::take(&mut self.pending);
        let code_page = self.code_page();
        if self.state.dest == Dest::Marker && matches!(code_page, CodePage::Symbol) {
            // A glyph from Symbol or Wingdings in a list marker is a bullet.
            // As ANSI it would read as another character: a Wingdings square
            // is 0xA7, the section sign.
            self.put('\u{2022}');
        } else {
            let text = code_page.decode(&bytes);
            self.put_str(&text);
        }
        bytes.clear();
        self.pending = bytes;
    }

    /// Code page of the text: the current font's, else `\ansicpgN`'s, else
    /// the document character set's.
    fn code_page(&self) -> CodePage {
        self.state
            .font
            .or(self.default_font)
            .and_then(|font| self.fonts.get(&font))
            .and_then(Font::code_page)
            .or(self.ansi_cpg)
            .unwrap_or(self.charset)
    }

    fn finish(mut self) -> String {
        self.flush();
        self.end_surrogate();
        let mut text = self.body;
        if !self.notes.is_empty() {
            text.push_str("\n\n");
            text.push_str(&self.notes);
        }
        tidy_lines(&text)
    }
}

/// Destinations that hold no document text, skipped even without `\*`.
fn is_skipped_destination(name: &[u8]) -> bool {
    matches!(
        name,
        // Tables and metadata.
        b"colortbl"
            | b"colorschememapping"
            | b"datastore"
            | b"filetbl"
            | b"generator"
            | b"info"
            | b"latentstyles"
            | b"listoverridetable"
            | b"listtable"
            | b"revtbl"
            | b"rsidtbl"
            | b"stylesheet"
            | b"themedata"
            | b"xmlnstbl"
            // Pictures and object data. An object's `\result` holds what it
            // shows, and is read.
            | b"objclass"
            | b"objdata"
            | b"objname"
            | b"pict"
            // Page furniture.
            | b"header"
            | b"headerf"
            | b"headerl"
            | b"headerr"
            | b"footer"
            | b"footerf"
            | b"footerl"
            | b"footerr"
            | b"ftncn"
            | b"ftnsep"
            | b"ftnsepc"
            | b"aftncn"
            | b"aftnsep"
            | b"aftnsepc"
            // Field codes. `\fldrslt` holds what a field shows, and is read.
            | b"fldinst"
            // Templates for list numbers (the rendered ones, in `\listtext`
            // and `\pntext`, are read), and text for readers without nested
            // tables.
            | b"pntxta"
            | b"pntxtb"
            | b"nonesttables"
            // Comments, bookmarks, and index and contents entries.
            | b"annotation"
            | b"atnauthor"
            | b"atnid"
            | b"bkmkend"
            | b"bkmkstart"
            | b"tc"
            | b"xe"
    )
}

/// A text encoding for code page bytes.
#[derive(Debug, Clone, Copy)]
enum CodePage {
    Encoding(&'static Encoding),
    /// A DOS code page that encoding_rs lacks, as its upper 128 characters.
    Table(&'static [char; 128]),
    /// A symbol font such as Symbol or Wingdings (`\fcharset2`), whose bytes
    /// name glyphs rather than characters. They are read as ANSI.
    Symbol,
}

impl CodePage {
    fn decode(self, bytes: &[u8]) -> Cow<'_, str> {
        match self {
            Self::Encoding(encoding) => encoding.decode_without_bom_handling(bytes).0,
            Self::Symbol => {
                encoding_rs::WINDOWS_1252
                    .decode_without_bom_handling(bytes)
                    .0
            }
            Self::Table(high) => bytes
                .iter()
                .map(|&byte| match byte.checked_sub(0x80) {
                    Some(index) => high[usize::from(index)],
                    None => char::from(byte),
                })
                .collect::<String>()
                .into(),
        }
    }
}

/// The encoding for a Windows code page number, as used by `\ansicpgN` and
/// `\cpgN`.
fn code_page(number: i32) -> Option<CodePage> {
    let encoding = match number {
        437 => return Some(CodePage::Table(&CP437)),
        850 => return Some(CodePage::Table(&CP850)),
        866 => encoding_rs::IBM866,
        874 => encoding_rs::WINDOWS_874,
        932 => encoding_rs::SHIFT_JIS,
        936 => encoding_rs::GBK,
        949 => encoding_rs::EUC_KR,
        950 => encoding_rs::BIG5,
        1250 => encoding_rs::WINDOWS_1250,
        1251 => encoding_rs::WINDOWS_1251,
        1252 => encoding_rs::WINDOWS_1252,
        1253 => encoding_rs::WINDOWS_1253,
        1254 => encoding_rs::WINDOWS_1254,
        1255 => encoding_rs::WINDOWS_1255,
        1256 => encoding_rs::WINDOWS_1256,
        1257 => encoding_rs::WINDOWS_1257,
        1258 => encoding_rs::WINDOWS_1258,
        10000 => encoding_rs::MACINTOSH,
        10007 => encoding_rs::X_MAC_CYRILLIC,
        20866 => encoding_rs::KOI8_R,
        21866 => encoding_rs::KOI8_U,
        54936 => encoding_rs::GB18030,
        65001 => encoding_rs::UTF_8,
        _ => return None,
    };
    Some(CodePage::Encoding(encoding))
}

/// The code page for a `\fcharsetN` value. The default charset, 1, has none
/// and leaves the choice to `\ansicpgN`.
fn charset_code_page(charset: i32) -> Option<CodePage> {
    code_page(match charset {
        0 => 1252,
        2 => return Some(CodePage::Symbol),
        77 => 10000,
        // Mac Japanese, Korean and Chinese read as their Windows kin.
        78 | 128 => 932,
        79 | 129 => 949,
        80 | 134 => 936,
        81 | 136 => 950,
        89 => 10007,
        161 => 1253,
        162 => 1254,
        163 => 1258,
        177 => 1255,
        178 => 1256,
        186 => 1257,
        204 => 1251,
        222 => 874,
        238 => 1250,
        // The PC character set, and OEM taken as the US one.
        254 | 255 => 437,
        _ => return None,
    })
}

/// Upper half of code page 437, the IBM PC character set, 16 bytes a row
/// from 0x80.
#[rustfmt::skip]
static CP437: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å',
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ',
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»',
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐',
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧',
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀',
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩',
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

/// Upper half of code page 850, DOS Latin 1, 16 bytes a row from 0x80.
#[rustfmt::skip]
static CP850: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å',
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', 'ø', '£', 'Ø', '×', 'ƒ',
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '®', '¬', '½', '¼', '¡', '«', '»',
    '░', '▒', '▓', '│', '┤', 'Á', 'Â', 'À', '©', '╣', '║', '╗', '╝', '¢', '¥', '┐',
    '└', '┴', '┬', '├', '─', '┼', 'ã', 'Ã', '╚', '╔', '╩', '╦', '╠', '═', '╬', '¤',
    'ð', 'Ð', 'Ê', 'Ë', 'È', 'ı', 'Í', 'Î', 'Ï', '┘', '┌', '█', '▄', '¦', 'Ì', '▀',
    'Ó', 'ß', 'Ô', 'Ò', 'õ', 'Õ', 'µ', 'þ', 'Þ', 'Ú', 'Û', 'Ù', 'ý', 'Ý', '¯', '´',
    '\u{ad}', '±', '‗', '¾', '¶', '§', '÷', '¸', '°', '¨', '·', '¹', '³', '²', '■', '\u{a0}',
];

/// Trim the end of each line, drop blank lines at the start and end, and keep
/// at most one blank line in a row.
fn tidy_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank = false;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank = !out.is_empty();
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        out.push_str(line);
        blank = false;
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn text(rtf: &str) -> String {
        extract(rtf.as_bytes()).expect("rtf extract")
    }

    #[test]
    fn test_extract_with_simple_rtf_returns_plain_text() {
        let rtf = r"{\rtf1\ansi{\fonttbl\f0\fswiss Helvetica;}\f0\pard Hello {\b world}.\par }";
        let text = extract(rtf.as_bytes()).expect("rtf extract");
        assert_eq!(text, "Hello world.");
    }

    #[test]
    fn test_extract_with_invalid_rtf_returns_decoded_bytes() {
        let text = extract(b"not rtf at all").expect("fallback decode");
        assert_eq!(text, "not rtf at all");
    }

    #[test]
    fn test_extract_with_paragraphs_table_and_line_break_returns_lines() {
        let rtf = r"{\rtf1\ansi\deff0{\fonttbl{\f0 Helvetica;}}
\pard First paragraph.\par
Second paragraph.\par
\trowd\cellx2000\cellx4000
\intbl Name\cell Age\cell\row
\trowd\cellx2000\cellx4000
\intbl Ada\cell 36\cell\row
\pard After the table.\line Same paragraph, new line.\par
}";
        assert_eq!(
            text(rtf),
            "First paragraph.\nSecond paragraph.\nName\tAge\nAda\t36\n\
             After the table.\nSame paragraph, new line."
        );
    }

    #[test]
    fn test_extract_with_blank_paragraphs_and_breaks_returns_one_blank_line_at_most() {
        let rtf = r"{\rtf1\ansi \par\par One.\par\par\par\par Two.\sect Three.\page Four.\column Five.\par\par}";
        assert_eq!(text(rtf), "One.\n\nTwo.\nThree.\nFour.\nFive.");
    }

    #[test]
    fn test_extract_with_unicode_line_separator_returns_newline() {
        // How macOS TextEdit writes a line break inside a paragraph.
        let rtf = "{\\rtf1\\ansi\\ansicpg1252\\cocoartf2822 Line one\\uc0\\u8232 Line two\\\n}";
        assert_eq!(text(rtf), "Line one\nLine two");
    }

    #[test]
    fn test_extract_with_escaped_and_raw_line_breaks_returns_par_breaks_only() {
        // A backslash before a raw line break means `\par`; a raw line break
        // alone is not text.
        let rtf = "{\\rtf1\\ansi One\\\nTwo\\\r\nThree\r\nstill three\\par}";
        assert_eq!(text(rtf), "One\nTwo\nThreestill three");
    }

    #[test]
    fn test_extract_with_nested_table_returns_inner_rows_on_their_own_lines() {
        // Word's layout: inner cells end with `\nestcell`, `\nestrow` sits in
        // a `\*\nesttableprops` group, and `\nonesttables` holds text for
        // readers without nested tables.
        let rtf = r"{\rtf1\ansi
\pard\intbl\itap1 Outer\cell
\pard\intbl\itap2 A\nestcell{\nonesttables\par}B\nestcell{\nonesttables\par}
{\*\nesttableprops\trowd\cellx1000\cellx2000\nestrow}{\nonesttables\par}
\pard\intbl\itap2 C\nestcell D\nestcell
{\*\nesttableprops\trowd\cellx1000\cellx2000\nestrow}{\nonesttables\par}
\pard\intbl\itap1 \cell\trowd\cellx3000\cellx6000\row
\pard After.\par}";
        assert_eq!(text(rtf), "Outer\tA\tB\nC\tD\nAfter.");
    }

    #[test]
    fn test_extract_with_cp1252_hex_escapes_returns_decoded_characters() {
        let rtf = r"{\rtf1\ansi\ansicpg1252 \'93quoted\'94 caf\'e9 costs \'80 5\par}";
        assert_eq!(
            text(rtf),
            "\u{201c}quoted\u{201d} caf\u{e9} costs \u{20ac} 5"
        );
    }

    #[test]
    fn test_extract_with_shift_jis_font_returns_japanese() {
        // Font 1 is Shift-JIS. The last character's second byte is a literal
        // `e` (0x65).
        let rtf = r"{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0\fnil\fcharset0 Arial;}
{\f1\fnil\fcharset128 MS Mincho;}}\f1 \'82\'a0\'82\'a2\'83e\f0  caf\'e9\par}";
        assert_eq!(text(rtf), "\u{3042}\u{3044}\u{30c6} caf\u{e9}");
    }

    #[test]
    fn test_extract_with_code_page_sources_returns_text_in_each() {
        // A font's charset wins over `\ansicpgN`, `\cpgN` wins over the
        // charset, and `\plain` returns to the default font.
        let rtf = r"{\rtf1\ansi\ansicpg1251\deff0{\fonttbl{\f0\fnil Default;}
{\f1\fnil\fcharset161 Greek;}{\f2\fnil\fcharset0\cpg1250 Czech;}}
\'e0 {\f1\'e1} {\f2\'e8} \f1\plain\'e0\par}";
        assert_eq!(text(rtf), "\u{430} \u{3b1} \u{10d} \u{430}");
    }

    #[test]
    fn test_extract_with_mac_and_pc_character_sets_returns_their_characters() {
        assert_eq!(text(r"{\rtf1\mac caf\'8e\par}"), "caf\u{e9}");
        assert_eq!(text(r"{\rtf1\pc \'8e\'b0\par}"), "\u{c4}\u{2591}");
        assert_eq!(text(r"{\rtf1\pca \'9b\par}"), "\u{f8}");
    }

    #[test]
    fn test_extract_with_unicode_escape_returns_character_without_fallback() {
        let rtf = "{\\rtf1\\ansi caf\\u233? \\u8364\\'80 euro\\par}";
        assert_eq!(text(rtf), "caf\u{e9} \u{20ac} euro");
    }

    #[test]
    fn test_extract_with_uc2_returns_character_without_two_byte_fallback() {
        // `\uc` is scoped to its group: after it, one byte is skipped again.
        let rtf = "{\\rtf1\\ansi {\\uc2\\u26412\\'96\\'7b}\\u26412?\\par}";
        assert_eq!(text(rtf), "\u{672c}\u{672c}");
    }

    #[test]
    fn test_extract_with_negative_unicode_value_returns_character() {
        // U+9AD8 is past 32767, so it is written as 0x9AD8 - 65536.
        let rtf = "{\\rtf1\\ansi \\u-25896?\\par}";
        assert_eq!(text(rtf), "\u{9ad8}");
    }

    #[test]
    fn test_extract_with_surrogate_pair_returns_one_character() {
        let rtf = "{\\rtf1\\ansi A\\u55357?\\u56832?B\\u-10179?\\u-8704?C\\u55357?D\\par}";
        assert_eq!(text(rtf), "A\u{1f600}B\u{1f600}C\u{fffd}D");
    }

    #[test]
    fn test_extract_with_missing_unicode_fallback_returns_following_markup() {
        // A control word or a brace ends the fallback, so neither the `\par`
        // nor the text next to a group is lost.
        let rtf = "{\\rtf1\\ansi A\\u8364\\par B{\\u8364}C\\u8364{D} {\\uc0 E\\u8364 F}\\par}";
        assert_eq!(text(rtf), "A\u{20ac}\nB\u{20ac}C\u{20ac}D E\u{20ac}F");
    }

    #[test]
    fn test_extract_with_wordpad_document_returns_quotes_euro_and_link_text() {
        let rtf = "{\\rtf1\\ansi\\ansicpg1252\\deff0\\nouicompat{\\fonttbl{\\f0\\fnil\\fcharset0 Calibri;}}\n\
                   {\\*\\generator Riched20 10.0.19041}\\viewkind4\\uc1 \n\
                   \\pard\\sa200\\sl276\\slmult1\\f0\\fs22\\lang9 He said \\ldblquote hi\\rdblquote  and \
                   \\'93quoted\\'94 caf\\'e9 \\u8364? euro.\\par\n\
                   Next {\\field{\\*\\fldinst{HYPERLINK \"https://x.test\"}}{\\fldrslt{link text}}} end.\\par\n}\n";
        assert_eq!(
            text(rtf),
            "He said \u{201c}hi\u{201d} and \u{201c}quoted\u{201d} caf\u{e9} \u{20ac} euro.\n\
             Next link text end."
        );
    }

    #[test]
    fn test_extract_with_fields_returns_results_without_instructions() {
        let rtf = r#"{\rtf1\ansi Next {\field{\*\fldinst{HYPERLINK "https://x.test"}}{\fldrslt{link text}}} and page {\field{\fldinst PAGE}{\fldrslt 7}}.\par}"#;
        assert_eq!(text(rtf), "Next link text and page 7.");
    }

    #[test]
    fn test_extract_with_ignorable_destinations_returns_text_without_them() {
        let rtf = r"{\rtf1\ansi{\*\generator Riched20 10.0;}Kept{\*\bkmkstart mark} text{\*\unknown {nested} words}.\par}";
        assert_eq!(text(rtf), "Kept text.");
    }

    #[test]
    fn test_extract_with_document_tables_returns_body_only() {
        let rtf = r"{\rtf1\ansi\deff0{\fonttbl{\f0\froman Times;}{\f1\fswiss Arial;}}
{\colortbl;\red0\green0\blue0;\red255\green0\blue0;}
{\stylesheet{\s0 Normal;}{\s1\sbasedon0 heading 1;}}
{\info{\title Secret title}{\author Someone}{\creatim\yr2024\mo1\dy2}}
{\header \pard Page header\par}{\footerr \pard Right footer\par}
\pard\plain\s1 Heading\par
{\pn\pnlvlbody\pndec{\pntxtb (}{\pntxta )}}Item\par}";
        assert_eq!(text(rtf), "Heading\nItem");
    }

    #[test]
    fn test_extract_with_word_numbered_list_returns_numbers_before_items() {
        // Word writes each item's number in `\listtext` for readers without
        // list tables; the list table itself stays hidden.
        let rtf = r"{\rtf1\ansi\deff0{\fonttbl{\f0\froman\fcharset0 Times New Roman;}}
{\*\listtable{\list\listtemplateid1{\listlevel\levelnfc0{\leveltext\'02\'00.;}
{\levelnumbers\'01;}}\listid1}}{\*\listoverridetable{\listoverride\listid1\ls1}}
\pard\plain\f0 Steps:\par
{\listtext\pard\plain\f0\fs24 1.\tab}\pard\plain\fi-360\li720\ls1\f0 Open the file.\par
{\listtext\pard\plain\f0\fs24 2.\tab}\pard\plain\fi-360\li720\ls1\f0 Save it.\par
{\listtext\pard\plain\f0\fs24 a)\tab}\pard\plain\fi-360\li1440\ilvl1\ls1\f0 Check it.\par
\pard\plain\f0 Done.\par}";
        assert_eq!(
            text(rtf),
            "Steps:\n1. Open the file.\n2. Save it.\na) Check it.\nDone."
        );
    }

    #[test]
    fn test_extract_with_bulleted_lists_returns_bullets_before_items() {
        // Word and WordPad draw bullets from Symbol (0xB7) and Wingdings
        // (0xA7, the section sign in ANSI); TextEdit writes a Unicode bullet
        // between tabs.
        let rtf = "{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0\\fswiss\\fcharset0 Arial;}\
                   {\\f1\\fnil\\fcharset2 Symbol;}{\\f2\\fnil\\fcharset2 Wingdings;}}\n\
                   {\\listtext\\pard\\plain\\f1 \\'b7\\tab}\\pard\\ls1\\f0 Word bullet\\par\n\
                   {\\listtext\\pard\\plain\\f2 \\'a7\\tab}\\pard\\ls1\\ilvl1\\f0 Wingdings square\\par\n\
                   {\\pntext\\f1\\'B7\\tab}{\\*\\pn\\pnlvlblt\\pnf1{\\pntxtb\\'B7}}\\pard\\f0 WordPad bullet\\par\n\
                   \\ls2{\\listtext\t\\uc0\\u8226 \t}TextEdit bullet\\\n}";
        assert_eq!(
            text(rtf),
            "\u{2022} Word bullet\n\u{2022} Wingdings square\n\u{2022} WordPad bullet\n\
             \u{2022} TextEdit bullet"
        );
    }

    #[test]
    fn test_extract_with_footnotes_returns_notes_after_body() {
        let rtf = r"{\rtf1\ansi Claim{\super\chftn}{\footnote\pard{\super\chftn} First source.} holds{\super\chftn}{\footnote\pard{\super\chftn} Second\par source.}.\par Next.\par}";
        assert_eq!(
            text(rtf),
            "Claim[1] holds[2].\nNext.\n\n[1] First source.\n[2] Second\nsource."
        );
    }

    #[test]
    fn test_extract_with_rtfd_attachment_returns_text_without_it() {
        // How TextEdit writes an image: a group naming the file, then one
        // byte standing in for the image.
        let rtf = b"{\\rtf1\\ansi\\ansicpg1252\\cocoartf1187 See {{\\NeXTGraphic classic.tif \
                    \\width240 \\height300 \\noorient\n}\xac} here.\\\n}";
        assert_eq!(extract(rtf).expect("rtf extract"), "See  here.");
    }

    #[test]
    fn test_extract_with_binary_data_returns_text_around_it() {
        // The binary bytes hold a brace, a backslash and a NUL that must not
        // be read as markup or text.
        let rtf = b"{\\rtf1\\ansi Before {\\pict\\bin4 }\\{\x00}after\\bin3 {}\\ end\\par}";
        assert_eq!(extract(rtf).expect("rtf extract"), "Before after end");
    }

    #[test]
    fn test_extract_with_escaped_characters_returns_literals() {
        let rtf = r"{\rtf1\ansi \\ \{braces\} no\~break non\_breaking opt\-ional\par}";
        assert_eq!(
            text(rtf),
            "\\ {braces} no\u{a0}break non\u{2011}breaking optional"
        );
    }

    #[test]
    fn test_extract_with_unbalanced_braces_returns_text() {
        // A file cut short keeps its text. A stray `}` closes the document,
        // as it does for macOS textutil.
        assert_eq!(text(r"{\rtf1\ansi {\b bold {\i text"), "bold text");
        assert_eq!(text(r"{\rtf1\ansi One.\par}} Two.\par}"), "One.");
        assert_eq!(text(r"{\rtf1\ansi Inside.\par}After"), "Inside.");
    }

    #[test]
    fn test_extract_with_deep_nesting_returns_shallow_text_quickly() {
        let depth = 100_000;
        let started = Instant::now();
        let deep = format!(
            r"{{\rtf1\ansi start {}deep{} end\par}}",
            "{".repeat(depth),
            "}".repeat(depth)
        );
        // Text below MAX_DEPTH groups is skipped with its groups.
        assert_eq!(text(&deep), "start  end");
        let unclosed = format!(r"{{\rtf1\ansi text{}", "{".repeat(depth));
        assert_eq!(text(&unclosed), "text");
        let within = format!(
            r"{{\rtf1\ansi {}kept{}}}",
            "{".repeat(1000),
            "}".repeat(1000)
        );
        assert_eq!(text(&within), "kept");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn test_extract_with_large_input_returns_in_linear_time() {
        // Ten megabytes of short units, each with an escape, a `\u`, a table
        // row, a group, a paragraph and a footnote. One pass takes about a
        // second unoptimized. Copying the text so far at each row end took
        // over 40 seconds, and the bound leaves room for a loaded machine.
        let unit = "x\\'e9\\u8364?\\cell\\row{\\b y}\\par z\\chftn{\\footnote n}";
        let units = 200_000;
        let rtf = format!("{{\\rtf1\\ansi {}}}", unit.repeat(units));
        let started = Instant::now();
        let got = text(&rtf);
        let elapsed = started.elapsed();
        assert_eq!(got.matches("x\u{e9}\u{20ac}\ny\nz[").count(), units);
        assert!(got.len() < rtf.len(), "{} >= {}", got.len(), rtf.len());
        assert!(elapsed < Duration::from_secs(20), "{elapsed:?}");
    }

    #[test]
    fn test_extract_with_raw_utf8_text_returns_characters() {
        // A `\u` fallback written as a raw UTF-8 character is skipped whole.
        let rtf = "{\\rtf1\\ansi caf\u{e9} \u{65e5}\u{672c} \\'e9t\u{e9} na\\u239\u{ef}ve\\par}";
        assert_eq!(
            text(rtf),
            "caf\u{e9} \u{65e5}\u{672c} \u{e9}t\u{e9} na\u{ef}ve"
        );
    }

    #[test]
    fn test_extract_with_raw_ansi_bytes_returns_code_page_text() {
        // Not UTF-8, so raw bytes are in code page 1251 like `\'hh` ones.
        let rtf = b"{\\rtf1\\ansi\\ansicpg1251 \xcf\xf0\xe8\xe2\xe5\xf2 \\'ec\xe8\xf0\\par}";
        assert_eq!(extract(rtf).expect("rtf extract"), "Привет мир");
    }

    #[test]
    fn test_extract_with_utf16_rtf_returns_plain_text() {
        let rtf = "{\\rtf1\\ansi caf\u{e9} \\'e9\\par}";
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(rtf.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(extract(&bytes).expect("rtf extract"), "caf\u{e9} \u{e9}");
    }

    #[test]
    fn test_extract_with_bom_and_leading_whitespace_returns_plain_text() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(b"\r\n  {\\rtf1\\ansi Hello\\par}");
        assert_eq!(extract(&bytes).expect("rtf extract"), "Hello");
    }

    #[test]
    fn test_extract_with_named_characters_returns_unicode() {
        let rtf = r"{\rtf1\ansi \lquote a\rquote  \ldblquote b\rdblquote  c\emdash d\endash e\bullet f\emspace g\enspace h\qmspace i\zwj j\zwnj k\ltrmark l\rtlmark m\tab n\par}";
        assert_eq!(
            text(rtf),
            "\u{2018}a\u{2019} \u{201c}b\u{201d} c\u{2014}d\u{2013}e\u{2022}f\u{2003}g\u{2002}h\
             \u{2005}i\u{200d}j\u{200c}klm\tn"
        );
    }
}
