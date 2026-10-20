//! A small, allocation-light XML pull parser.
//!
//! This covers exactly the slice of XML that OOXML and OpenDocument parts use: elements
//! and attributes, character data, CDATA sections, comments, processing instructions and
//! the five predefined entities plus numeric character references. That is deliberately
//! *not* a full XML implementation — no DTDs, no external entities, no custom entity
//! declarations. Refusing those is a feature for a file reader: it removes the whole class
//! of entity-expansion and XXE attacks.
//!
//! Self-closing tags are expanded, so `<c/>` is reported as `Start` followed by `End`.
//! That single rule lets every caller track nesting with a plain depth counter.
//!
//! Deliberately dependency-free so the crate's supply chain stays narrow.

use crate::error::{Error, Result};
use std::borrow::Cow;
use std::str;

/// One step of the document.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event<'a> {
    /// An opening tag.
    Start(Element<'a>),
    /// A closing tag, carrying the element name.
    End(&'a str),
    /// Character data, as it appears in the document (entities not yet resolved).
    Text(&'a str),
    /// The document is exhausted.
    Eof,
}

/// An opening tag: its name plus the raw attribute text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Element<'a> {
    name: &'a str,
    attrs: &'a str,
}

impl<'a> Element<'a> {
    /// The tag name with any namespace prefix removed.
    pub fn local_name(&self) -> &'a str {
        local_name(self.name)
    }

    /// An attribute value exactly as written, without entity resolution.
    ///
    /// Use this for values that cannot contain entities (cell references, indices) to avoid
    /// an allocation entirely.
    pub fn attr_raw(&self, name: &str) -> Option<&'a str> {
        self.attrs().find(|(k, _)| *k == name).map(|(_, v)| v)
    }

    /// An attribute value with entities resolved. Borrows when nothing needed decoding.
    pub fn attr(&self, name: &str) -> Option<Cow<'a, str>> {
        self.attr_raw(name).map(unescape)
    }

    /// An attribute value resolved to an owned string.
    pub fn attr_string(&self, name: &str) -> Option<String> {
        self.attr(name).map(Cow::into_owned)
    }

    /// An attribute value looked up by local name, ignoring any namespace prefix.
    pub fn attr_local(&self, name: &str) -> Option<Cow<'a, str>> {
        self.attrs()
            .find(|(k, _)| local_name(k) == name)
            .map(|(_, v)| unescape(v))
    }

    /// Iterates the raw `(name, value)` pairs.
    pub fn attrs(&self) -> AttrIter<'a> {
        AttrIter { rest: self.attrs }
    }
}

/// Iterator over an element's attributes, yielding raw (unescaped) values.
#[derive(Debug, Clone)]
pub struct AttrIter<'a> {
    rest: &'a str,
}

impl<'a> Iterator for AttrIter<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        let trimmed = self.rest.trim_start();
        if trimmed.is_empty() {
            return None;
        }
        let eq = trimmed.find('=')?;
        let name = trimmed[..eq].trim();
        let after = trimmed[eq + 1..].trim_start();
        let quote = after.as_bytes().first().copied()?;
        if quote != b'"' && quote != b'\'' {
            return None;
        }
        let end = after[1..].find(quote as char)? + 1;
        let value = &after[1..end];
        self.rest = &after[end + 1..];
        Some((name, value))
    }
}

/// Strips a namespace prefix from a qualified name.
pub fn local_name(name: &str) -> &str {
    match name.rfind(':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// A pull parser over a byte buffer.
pub struct XmlParser<'a> {
    data: &'a [u8],
    pos: usize,
    /// An `End` owed to the caller because the tag was self-closing.
    pending_end: Option<&'a str>,
}

impl<'a> XmlParser<'a> {
    /// Creates a parser over a UTF-8 byte buffer.
    pub fn new(data: &'a [u8]) -> Self {
        // Strip a UTF-8 BOM, which some producers emit ahead of the XML declaration.
        let data = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);
        XmlParser {
            data,
            pos: 0,
            pending_end: None,
        }
    }

    /// Creates a parser over a string.
    #[cfg(test)]
    pub fn new_str(data: &'a str) -> Self {
        XmlParser::new(data.as_bytes())
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn text_from(&self, start: usize, end: usize) -> Result<&'a str> {
        str::from_utf8(&self.data[start..end])
            .map_err(|_| Error::xml(format!("invalid utf-8 at byte {start}")))
    }

    fn find_byte(&self, byte: u8, from: usize) -> Option<usize> {
        self.data[from..]
            .iter()
            .position(|&b| b == byte)
            .map(|i| i + from)
    }

    /// Advances to the next event.
    pub fn next_event(&mut self) -> Result<Event<'a>> {
        if let Some(name) = self.pending_end.take() {
            return Ok(Event::End(name));
        }
        loop {
            if self.pos >= self.data.len() {
                return Ok(Event::Eof);
            }
            match self.peek() {
                Some(b'<') => match self.data.get(self.pos + 1).copied() {
                    Some(b'?') => self.skip_processing_instruction()?,
                    Some(b'!') => {
                        // A CDATA section yields text in the position it occupies, so it is
                        // returned here rather than queued.
                        if let Some(text) = self.skip_declaration()? {
                            if !text.is_empty() {
                                return Ok(Event::Text(text));
                            }
                        }
                    }
                    Some(b'/') => return self.read_end_tag(),
                    Some(c) if is_name_start(c) => return self.read_start_tag(),
                    Some(c) => {
                        return Err(Error::xml(format!(
                            "unexpected {:?} after '<' at byte {}",
                            c as char, self.pos
                        )))
                    }
                    None => return Err(Error::xml("truncated document")),
                },
                _ => {
                    let start = self.pos;
                    let end = self.find_byte(b'<', self.pos).unwrap_or(self.data.len());
                    self.pos = end;
                    if end > start {
                        return Ok(Event::Text(self.text_from(start, end)?));
                    }
                }
            }
        }
    }

    fn skip_processing_instruction(&mut self) -> Result<()> {
        match self.data[self.pos..].windows(2).position(|w| w == b"?>") {
            Some(i) => {
                self.pos += i + 2;
                Ok(())
            }
            None => Err(Error::xml("unterminated processing instruction")),
        }
    }

    /// Skips comments and DOCTYPE declarations, and lifts the content out of a CDATA section.
    fn skip_declaration(&mut self) -> Result<Option<&'a str>> {
        if self.data[self.pos..].starts_with(b"<!--") {
            let from = self.pos + 4;
            match self.data[from..].windows(3).position(|w| w == b"-->") {
                Some(i) => {
                    self.pos = from + i + 3;
                    Ok(None)
                }
                None => Err(Error::xml("unterminated comment")),
            }
        } else if self.data[self.pos..].starts_with(b"<![CDATA[") {
            let from = self.pos + 9;
            match self.data[from..].windows(3).position(|w| w == b"]]>") {
                Some(i) => {
                    let text = self.text_from(from, from + i)?;
                    self.pos = from + i + 3;
                    Ok(Some(text))
                }
                None => Err(Error::xml("unterminated CDATA section")),
            }
        } else {
            // DOCTYPE, possibly with an internal subset. Skipping it rather than resolving it
            // is what makes entity-expansion attacks impossible here.
            let mut depth = 0i32;
            let mut i = self.pos;
            while i < self.data.len() {
                match self.data[i] {
                    b'[' => depth += 1,
                    b']' => depth -= 1,
                    b'>' if depth <= 0 => {
                        self.pos = i + 1;
                        return Ok(None);
                    }
                    _ => {}
                }
                i += 1;
            }
            Err(Error::xml("unterminated declaration"))
        }
    }

    fn read_end_tag(&mut self) -> Result<Event<'a>> {
        let start = self.pos + 2;
        let end = self
            .find_byte(b'>', start)
            .ok_or_else(|| Error::xml("unterminated end tag"))?;
        let raw = self.text_from(start, end)?;
        self.pos = end + 1;
        Ok(Event::End(raw.trim()))
    }

    fn read_start_tag(&mut self) -> Result<Event<'a>> {
        let start = self.pos + 1;
        let mut i = start;
        while i < self.data.len() && is_name_char(self.data[i]) {
            i += 1;
        }
        if i == start {
            return Err(Error::xml(format!("empty tag name at byte {start}")));
        }
        let name = self.text_from(start, i)?;
        let attrs_start = i;
        // Walk to the '>' that closes the tag, honouring quoted attribute values so that a
        // '>' inside a value does not terminate the tag early.
        let mut quote: Option<u8> = None;
        while i < self.data.len() {
            let b = self.data[i];
            match quote {
                Some(q) => {
                    if b == q {
                        quote = None;
                    }
                }
                None => match b {
                    b'"' | b'\'' => quote = Some(b),
                    b'>' => break,
                    _ => {}
                },
            }
            i += 1;
        }
        if i >= self.data.len() {
            return Err(Error::xml(format!("unterminated tag <{name}")));
        }
        let attrs = self.text_from(attrs_start, i)?.trim_end();
        self.pos = i + 1;
        match attrs.strip_suffix('/') {
            Some(rest) => {
                self.pending_end = Some(name);
                Ok(Event::Start(Element {
                    name,
                    attrs: rest.trim_end(),
                }))
            }
            None => Ok(Event::Start(Element { name, attrs })),
        }
    }

    /// The current byte offset into the document.
    ///
    /// A caller reading only a prefix of a document uses this to find where the last element it
    /// cares about ends, so it can cut the prefix there rather than partway through a tag.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Consumes the rest of the element whose `Start` was just returned.
    pub fn skip_element(&mut self) -> Result<()> {
        let mut depth = 1usize;
        loop {
            match self.next_event()? {
                Event::Start(_) => depth += 1,
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Event::Text(_) => {}
                Event::Eof => return Err(Error::xml("unexpected end of document")),
            }
        }
    }

    /// Collects all text inside the element whose `Start` was just returned, and consumes the
    /// matching `End`.
    ///
    /// Text nested inside child elements is folded in, which is what ODS needs for the
    /// `<text:p>` runs inside a cell.
    pub fn inner_text(&mut self) -> Result<String> {
        self.inner_text_cow().map(Cow::into_owned)
    }

    /// [`inner_text`](Self::inner_text), borrowing when the text needs no decoding.
    ///
    /// A worksheet cell's value lives in a `<v>` element holding one plain text run, so the
    /// common case is a single borrow and no allocation at all. A sheet of 200,000 numeric
    /// cells otherwise performs 200,000 `String` allocations for text that is immediately
    /// parsed and dropped.
    ///
    /// The borrow is only taken when the element's whole content turns out to be that one run.
    /// A second run, a nested child, or an entity means the pieces have to be joined, and the
    /// prompt path is abandoned for the accumulating one.
    pub fn inner_text_cow(&mut self) -> Result<Cow<'a, str>> {
        // Set only while the content is still a single unescaped run.
        let mut solo: Option<&'a str> = None;
        let mut joined: Option<String> = None;
        let mut depth = 1usize;
        loop {
            match self.next_event()? {
                Event::Start(_) => {
                    depth += 1;
                    // Nested content: the text is no longer a single run, so take ownership.
                    if let Some(run) = solo.take() {
                        let mut out = String::with_capacity(run.len());
                        out.push_str(run);
                        joined = Some(out);
                    }
                }
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(match (joined, solo) {
                            (Some(text), _) => Cow::Owned(text),
                            (None, Some(run)) => Cow::Borrowed(run),
                            // Nothing was collected: an empty element.
                            (None, None) => Cow::Borrowed(""),
                        });
                    }
                }
                Event::Text(t) => {
                    if let Some(out) = joined.as_mut() {
                        push_unescaped(out, t);
                    } else if solo.is_none() && depth == 1 && !t.contains('&') {
                        solo = Some(t);
                    } else {
                        // Either an entity needs decoding or this is a later run: accumulate.
                        let mut out = String::with_capacity(t.len());
                        if let Some(run) = solo.take() {
                            out.push_str(run);
                        }
                        push_unescaped(&mut out, t);
                        joined = Some(out);
                    }
                }
                Event::Eof => return Err(Error::xml("unexpected end of document")),
            }
        }
    }
}

/// Resolves the five predefined XML entities and numeric character references.
///
/// Returns the input unchanged when there is nothing to decode, which is the common case.
pub fn unescape(raw: &str) -> Cow<'_, str> {
    if !raw.contains('&') {
        return Cow::Borrowed(raw);
    }
    let mut out = String::with_capacity(raw.len());
    unescape_into(&mut out, raw);
    Cow::Owned(out)
}

/// Longest entity name this decoder will consider, excluding the `&` and `;`.
///
/// The five predefined names are far shorter; numeric references are at most `#x10FFFF`. The
/// bound exists so the scan below can stop looking: without it every stray `&` would search to
/// the end of the text, making the whole pass quadratic on content like `a & b & c …`.
const MAX_ENTITY_NAME: usize = 32;

/// Appends `raw` to `out` with entities resolved.
pub fn unescape_into(out: &mut String, raw: &str) {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'&' {
                i += 1;
            }
            out.push_str(&raw[start..i]);
            continue;
        }
        // Look for the closing `;` only within the longest name that could be an entity, so a
        // run of literal ampersands costs a bounded comparison each instead of a scan to the
        // end of the text. Searching bytes keeps both slice boundaries on ASCII, which matters
        // for text that is not ASCII.
        let limit = (i + 1 + MAX_ENTITY_NAME).min(bytes.len());
        let mut semi = None;
        for (offset, &byte) in bytes[i + 1..limit].iter().enumerate() {
            if byte == b';' {
                semi = Some(i + 1 + offset);
                break;
            }
        }
        match semi.and_then(|at| decode_entity(&raw[i + 1..at]).map(|c| (c, at))) {
            Some((c, at)) => {
                out.push(c);
                i = at + 1;
            }
            // A stray `&` that is not an entity survives verbatim.
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let n = entity.strip_prefix('#')?;
            let cp = match n.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => n.parse::<u32>().ok()?,
            };
            char::from_u32(cp)
        }
    }
}

fn push_unescaped(out: &mut String, raw: &str) {
    if raw.contains('&') {
        unescape_into(out, raw);
    } else {
        out.push_str(raw);
    }
}

/// Decodes OOXML's `_xHHHH_` escape sequences, which represent characters XML cannot carry.
///
/// An underscore that is meant literally is itself escaped as `_x005F_`, so
/// `_x005F_x0041_` decodes to the literal text `_x0041_` rather than to `A`.
pub fn decode_ooxml_escapes(raw: &str) -> Cow<'_, str> {
    if !raw.contains("_x") {
        return Cow::Borrowed(raw);
    }
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_'
            && i + 7 <= bytes.len()
            && matches!(bytes[i + 1], b'x' | b'X')
            && bytes[i + 6] == b'_'
        {
            if let Ok(code) = u16::from_str_radix(&raw[i + 2..i + 6], 16) {
                if let Some(c) = char::from_u32(u32::from(code)) {
                    out.push(c);
                    i += 7;
                    continue;
                }
            }
        }
        let ch = raw[i..].chars().next().unwrap_or('_');
        out.push(ch);
        i += ch.len_utf8();
    }
    Cow::Owned(out)
}

fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b':'
}

fn is_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'-' | b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collects a whole document into a comparable shape.
    fn events(xml: &str) -> Vec<String> {
        let mut parser = XmlParser::new_str(xml);
        let mut out = Vec::new();
        loop {
            match parser.next_event().unwrap() {
                Event::Start(el) => {
                    let mut attrs: Vec<String> =
                        el.attrs().map(|(k, v)| format!("{k}={v}")).collect();
                    attrs.sort();
                    out.push(format!("<{} {}>", el.name, attrs.join(" ")));
                }
                Event::End(name) => out.push(format!("</{name}>")),
                Event::Text(text) => out.push(format!("text:{text}")),
                Event::Eof => break,
            }
        }
        out
    }

    #[test]
    fn reads_elements_and_attributes() {
        assert_eq!(
            events(r#"<a x="1" y='two'/>"#),
            vec!["<a x=1 y=two>".to_string(), "</a>".to_string()]
        );
    }

    #[test]
    fn reports_self_closing_tags_as_start_and_end() {
        assert_eq!(
            events("<row><c r=\"A1\"/></row>"),
            vec![
                "<row >".to_string(),
                "<c r=A1>".to_string(),
                "</c>".to_string(),
                "</row>".to_string(),
            ]
        );
    }

    #[test]
    fn reads_nested_text() {
        assert_eq!(
            events("<a>one<b>two</b>three</a>"),
            vec![
                "<a >".to_string(),
                "text:one".to_string(),
                "<b >".to_string(),
                "text:two".to_string(),
                "</b>".to_string(),
                "text:three".to_string(),
                "</a>".to_string(),
            ]
        );
    }

    #[test]
    fn skips_comments_and_processing_instructions() {
        assert_eq!(
            events("<?xml version=\"1.0\"?><!-- note --><a><!--x-->1</a>"),
            vec!["<a >".to_string(), "text:1".to_string(), "</a>".to_string()]
        );
    }

    #[test]
    fn skips_a_doctype_including_its_internal_subset() {
        let xml = "<!DOCTYPE a [ <!ENTITY x \"y\"> ]><a>1</a>";
        assert_eq!(
            events(xml),
            vec!["<a >".to_string(), "text:1".to_string(), "</a>".to_string()]
        );
    }

    #[test]
    fn lifts_cdata_out_as_text() {
        let mut parser = XmlParser::new_str("<a><![CDATA[<not a tag>]]></a>");
        assert!(matches!(parser.next_event().unwrap(), Event::Start(_)));
        match parser.next_event().unwrap() {
            Event::Text(t) => assert_eq!(t, "<not a tag>"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn inner_text_joins_nested_runs() {
        let mut parser = XmlParser::new_str("<p>hello <b>bold</b> world</p>");
        assert!(matches!(parser.next_event().unwrap(), Event::Start(_)));
        assert_eq!(parser.inner_text().unwrap(), "hello bold world");
    }

    #[test]
    fn inner_text_resolves_entities() {
        let mut parser = XmlParser::new_str("<t>a &amp; b &#65; &#x42; &lt;</t>");
        assert!(matches!(parser.next_event().unwrap(), Event::Start(_)));
        assert_eq!(parser.inner_text().unwrap(), "a & b A B <");
    }

    #[test]
    fn skip_element_consumes_whole_subtrees() {
        let mut parser = XmlParser::new_str("<a><b><c/></b></a><d/>");
        assert!(matches!(parser.next_event().unwrap(), Event::Start(_)));
        parser.skip_element().unwrap();
        // The next event is the sibling, not something from inside <a>.
        match parser.next_event().unwrap() {
            Event::Start(el) => assert_eq!(el.local_name(), "d"),
            other => panic!("expected <d>, got {other:?}"),
        }
    }

    #[test]
    fn a_gt_inside_an_attribute_does_not_end_the_tag() {
        let mut parser = XmlParser::new_str(r#"<a b="1 > 2" c="3"/>"#);
        match parser.next_event().unwrap() {
            Event::Start(el) => {
                assert_eq!(el.attr_raw("b"), Some("1 > 2"));
                assert_eq!(el.attr_raw("c"), Some("3"));
            }
            other => panic!("expected a start tag, got {other:?}"),
        }
    }

    #[test]
    fn attribute_lookup_ignores_namespace_prefixes() {
        let mut parser = XmlParser::new_str(r#"<c office:value-type="float"/>"#);
        match parser.next_event().unwrap() {
            Event::Start(el) => {
                assert_eq!(el.attr_local("value-type").as_deref(), Some("float"));
                assert_eq!(el.attr_raw("value-type"), None);
                assert_eq!(el.local_name(), "c");
            }
            other => panic!("expected a start tag, got {other:?}"),
        }
    }

    #[test]
    fn a_utf8_bom_is_ignored() {
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice(b"<a/>");
        let mut parser = XmlParser::new(&data);
        match parser.next_event().unwrap() {
            Event::Start(el) => assert_eq!(el.local_name(), "a"),
            other => panic!("expected a start tag, got {other:?}"),
        }
    }

    #[test]
    fn entities_decode() {
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape("a &amp; b"), "a & b");
        assert_eq!(unescape("&#65;&#x42;"), "AB");
        // A stray ampersand survives rather than swallowing the rest.
        assert_eq!(unescape("a & b"), "a & b");
        assert_eq!(unescape("a &notanentity; b"), "a &notanentity; b");
    }

    #[test]
    fn ooxml_escapes_decode() {
        assert_eq!(decode_ooxml_escapes("a_x000D_b"), "a\rb");
        assert_eq!(decode_ooxml_escapes("_x0041_"), "A");
        assert_eq!(decode_ooxml_escapes("plain"), "plain");
        // An escaped underscore keeps the following sequence literal.
        assert_eq!(decode_ooxml_escapes("_x005F_x0041_"), "_x0041_");
    }

    #[test]
    fn malformed_documents_error_rather_than_panic() {
        for xml in [
            "<a",
            "<a b=\"unterminated>",
            "<!-- unterminated",
            "<![CDATA[ unterminated",
            "<a></a",
        ] {
            let mut parser = XmlParser::new_str(xml);
            let mut failed = false;
            loop {
                match parser.next_event() {
                    Ok(Event::Eof) => break,
                    Ok(_) => {}
                    Err(_) => {
                        failed = true;
                        break;
                    }
                }
            }
            assert!(failed, "{xml:?} should have been rejected");
        }
    }

    #[test]
    fn truncated_input_without_a_closing_tag_errors() {
        let mut parser = XmlParser::new_str("<a><b>");
        assert!(matches!(parser.next_event().unwrap(), Event::Start(_)));
        assert!(parser.skip_element().is_err());
    }
}
