//! Entity decoding stays linear, and text survives it.
//!
//! `unescape_into` walks every text run looking for entities. A scan that searched to the end of
//! the text for each stray `&` made the whole pass quadratic, which is a denial of service on a
//! large cell: 1 MB of ampersands became ~10^12 byte comparisons. The scan is now bounded by the
//! longest name an entity can have.

use gridxcel_rs::Reader;

fn ods_package(content: &str) -> Vec<u8> {
    let stored = zipcore::FileOptions::stored();
    let mut w = zipcore::ZipWriter::new(std::io::Cursor::new(Vec::new())).unwrap();
    w.add_file(
        "mimetype",
        b"application/vnd.oasis.opendocument.spreadsheet",
        &stored,
    )
    .unwrap();
    w.add_file("content.xml", content.as_bytes(), &stored)
        .unwrap();
    w.finish().unwrap().into_inner()
}

fn content_with(text: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
 xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
 xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
<office:body><office:spreadsheet><table:table table:name="S">
<table:table-row><table:table-cell office:value-type="string"><text:p>{text}</text:p></table:table-cell></table:table-row>
</table:table></office:spreadsheet></office:body></office:document-content>"#
    )
}

fn cell_text(text: &str) -> String {
    let bytes = ods_package(&content_with(text));
    let mut wb = gridxcel_rs::open_ods_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let range = wb.worksheet_range("S").unwrap();
    range
        .get((0, 0))
        .and_then(|v| v.as_string())
        .unwrap_or_default()
}

fn time_ampersands(n: usize) -> std::time::Duration {
    let text = "&".repeat(n);
    let bytes = ods_package(&content_with(&text));
    let mut wb = gridxcel_rs::open_ods_from_rs(std::io::Cursor::new(bytes)).unwrap();
    let start = std::time::Instant::now();
    let range = wb.worksheet_range("S").unwrap();
    let elapsed = start.elapsed();
    assert_eq!(
        range.get((0, 0)).unwrap().as_string().unwrap().len(),
        n,
        "every ampersand must survive"
    );
    elapsed
}

/// Doubling the input roughly doubles the work rather than quadrupling it.
#[test]
fn a_run_of_stray_ampersands_is_decoded_linearly() {
    // Warm up, so the first measurement does not pay for page faults or lazy statics.
    let _ = time_ampersands(5_000);
    let small = time_ampersands(50_000);
    let large = time_ampersands(100_000);
    let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
    // The old quadratic scan measured ~3.7x here; linear sits near 2x. The bound is loose so a
    // noisy machine does not fail the build, but far below what quadratic would produce.
    assert!(
        ratio < 3.0,
        "doubling the input should roughly double the time, got {ratio:.2}x \
         (50k = {small:?}, 100k = {large:?})"
    );
}

/// Entities still decode, and things that only look like them survive.
#[test]
fn entities_decode_and_lookalikes_survive() {
    assert_eq!(cell_text("a &amp; b"), "a & b");
    assert_eq!(cell_text("&#65;&#x42;"), "AB");
    assert_eq!(cell_text("&lt;tag&gt;"), "<tag>");
    assert_eq!(cell_text("&quot;q&quot; &apos;a&apos;"), "\"q\" 'a'");
    // A stray ampersand is not an entity and must not be swallowed.
    assert_eq!(cell_text("a & b"), "a & b");
    assert_eq!(cell_text("a &notanentity; b"), "a &notanentity; b");
    // An unterminated reference at the very end survives.
    assert_eq!(cell_text("tail&amp"), "tail&amp");
    // A name longer than any real entity is left alone rather than scanned for.
    let long = format!("&{};", "x".repeat(100));
    assert_eq!(cell_text(&long), long);
}

/// Non-ASCII text around an entity is not split on a character boundary.
#[test]
fn multi_byte_text_with_entities_is_intact() {
    assert_eq!(cell_text("日本語 &amp; 中文"), "日本語 & 中文");
    assert_eq!(cell_text("café &amp; crème"), "café & crème");
    assert_eq!(cell_text("&#x1F600; smile"), "\u{1F600} smile");
}
