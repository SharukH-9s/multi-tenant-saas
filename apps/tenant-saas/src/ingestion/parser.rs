//! Turns raw document bytes into normalized text sections.
//!
//! Synchronous and CPU-bound. The ingestion service runs it inside
//! `spawn_blocking` under a timeout and a semaphore permit.

use super::normalize::{normalize, normalize_newlines, strip_control_chars};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub text: String,
    pub page: Option<u32>,
    pub heading: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDocument {
    pub sections: Vec<Section>,
    pub char_count: usize,
}

#[derive(Debug, Clone)]
pub struct ParseLimits {
    pub max_pdf_pages: u32,
    pub max_chars: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("unsupported format")]
    Unsupported,
    #[error("document too large")]
    TooLarge,
    #[error("document is empty")]
    Empty,
    /// Static text only, never parser output.
    #[error("corrupt document: {0}")]
    Corrupt(&'static str),
}

pub fn parse(
    bytes: &[u8],
    mime_type: &str,
    limits: &ParseLimits,
) -> Result<ParsedDocument, ParseError> {
    let sections = match mime_type {
        "text/plain" => parse_text(bytes)?,
        "text/markdown" => parse_markdown(bytes)?,
        "application/pdf" => parse_pdf(bytes, limits)?,
        _ => return Err(ParseError::Unsupported),
    };

    let sections: Vec<Section> = sections
        .into_iter()
        .filter(|s| !s.text.is_empty())
        .collect();
    let char_count: usize = sections.iter().map(|s| s.text.chars().count()).sum();
    if char_count > limits.max_chars {
        return Err(ParseError::TooLarge);
    }
    if sections.is_empty() {
        return Err(ParseError::Empty);
    }
    Ok(ParsedDocument {
        sections,
        char_count,
    })
}

fn decode(bytes: &[u8]) -> Result<&str, ParseError> {
    std::str::from_utf8(bytes).map_err(|_| ParseError::Corrupt("invalid utf-8"))
}

fn parse_text(bytes: &[u8]) -> Result<Vec<Section>, ParseError> {
    Ok(vec![Section {
        text: normalize(decode(bytes)?, false),
        page: None,
        heading: None,
    }])
}

fn parse_markdown(bytes: &[u8]) -> Result<Vec<Section>, ParseError> {
    // Line endings and control characters first so heading detection sees clean lines.
    let text = strip_control_chars(&normalize_newlines(decode(bytes)?));

    let mut sections = Vec::new();
    let mut buf = String::new();
    let mut heading: Option<String> = None;
    let mut fence: Option<(char, usize)> = None;

    let mut flush = |buf: &mut String, heading: &Option<String>| {
        sections.push(Section {
            text: normalize(buf, false),
            page: None,
            heading: heading.clone(),
        });
        buf.clear();
    };

    for line in text.split('\n') {
        if let Some(marker) = fence_marker(line) {
            match fence {
                None => fence = Some(marker),
                Some((c, n)) if marker.0 == c && marker.1 >= n && line.trim().len() == marker.1 => {
                    fence = None
                }
                Some(_) => {}
            }
        } else if fence.is_none() {
            if let Some(title) = atx_heading(line) {
                flush(&mut buf, &heading);
                heading = Some(title);
            }
        }
        buf.push_str(line);
        buf.push('\n');
    }
    flush(&mut buf, &heading);
    Ok(sections)
}

/// Returns the fence character and run length for a line opening or closing a code fence.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let c = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = rest.chars().take_while(|x| *x == c).count();
    (n >= 3).then_some((c, n))
}

/// Matches `^#{1,6} ` and returns the heading text without the hashes.
pub(crate) fn atx_heading(line: &str) -> Option<String> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) || line.as_bytes().get(hashes) != Some(&b' ') {
        return None;
    }
    let title = line[hashes..].trim().trim_end_matches('#').trim_end();
    Some(title.to_string())
}

fn parse_pdf(bytes: &[u8], limits: &ParseLimits) -> Result<Vec<Section>, ParseError> {
    let pages = pdf_extract::extract_text_from_mem_by_pages(bytes)
        .map_err(|_| ParseError::Corrupt("pdf parse error"))?;
    if pages.len() as u64 > u64::from(limits.max_pdf_pages) {
        return Err(ParseError::TooLarge);
    }
    Ok(pages
        .iter()
        .enumerate()
        .map(|(i, text)| Section {
            text: normalize(text, true),
            page: Some(i as u32 + 1),
            heading: None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &[u8] = include_bytes!("../../tests/fixtures/ingestion/simple.txt");
    const HEADINGS: &[u8] = include_bytes!("../../tests/fixtures/ingestion/headings.md");
    const TWO_PAGES: &[u8] = include_bytes!("../../tests/fixtures/ingestion/two_pages.pdf");
    const EMPTY_PDF: &[u8] = include_bytes!("../../tests/fixtures/ingestion/empty_text_layer.pdf");
    const CORRUPT: &[u8] = include_bytes!("../../tests/fixtures/ingestion/corrupt.pdf");
    const CRLF: &[u8] = include_bytes!("../../tests/fixtures/ingestion/crlf.txt");
    const BOM: &[u8] = include_bytes!("../../tests/fixtures/ingestion/bom_control_chars.txt");

    fn limits() -> ParseLimits {
        ParseLimits {
            max_pdf_pages: 100,
            max_chars: 1_000_000,
        }
    }

    #[test]
    fn plain_text_is_one_section_without_page() {
        let doc = parse(SIMPLE, "text/plain", &limits()).unwrap();
        assert_eq!(doc.sections.len(), 1);
        assert_eq!(doc.sections[0].page, None);
        assert_eq!(doc.sections[0].heading, None);
        assert!(doc.sections[0].text.starts_with("Hello"));
        assert_eq!(doc.char_count, doc.sections[0].text.chars().count());
    }

    #[test]
    fn invalid_utf8_is_corrupt() {
        for mime in ["text/plain", "text/markdown"] {
            let err = parse(&[0xff, 0xfe, 0xfd], mime, &limits()).unwrap_err();
            assert!(matches!(err, ParseError::Corrupt("invalid utf-8")));
        }
    }

    #[test]
    fn markdown_splits_at_headings_and_ignores_fenced_hashes() {
        let doc = parse(HEADINGS, "text/markdown", &limits()).unwrap();
        let headings: Vec<_> = doc.sections.iter().map(|s| s.heading.as_deref()).collect();
        assert_eq!(
            headings,
            vec![None, Some("Title"), Some("Second"), Some("Third")]
        );
        assert!(doc.sections[1].text.starts_with("# Title"));
        let second = &doc.sections[2];
        assert!(second.text.contains("# not a heading"));
        assert!(second.text.contains("```"));
    }

    #[test]
    fn markdown_hash_without_space_is_not_a_heading() {
        let doc = parse(b"#nope\nbody", "text/markdown", &limits()).unwrap();
        assert_eq!(doc.sections.len(), 1);
        assert_eq!(doc.sections[0].heading, None);
    }

    #[test]
    fn pdf_gives_one_section_per_page() {
        let doc = parse(TWO_PAGES, "application/pdf", &limits()).unwrap();
        assert_eq!(doc.sections.len(), 2);
        assert_eq!(doc.sections[0].page, Some(1));
        assert_eq!(doc.sections[1].page, Some(2));
        assert!(doc.sections[0].text.contains("First page"));
        assert!(doc.sections[1].text.contains("Second page"));
    }

    #[test]
    fn pdf_without_text_layer_is_empty() {
        let err = parse(EMPTY_PDF, "application/pdf", &limits()).unwrap_err();
        assert!(matches!(err, ParseError::Empty), "{err:?}");
    }

    #[test]
    fn corrupt_pdf_is_corrupt() {
        let err = parse(CORRUPT, "application/pdf", &limits()).unwrap_err();
        assert!(
            matches!(err, ParseError::Corrupt("pdf parse error")),
            "{err:?}"
        );
    }

    #[test]
    fn pdf_page_limit_is_enforced() {
        let l = ParseLimits {
            max_pdf_pages: 1,
            ..limits()
        };
        let err = parse(TWO_PAGES, "application/pdf", &l).unwrap_err();
        assert!(matches!(err, ParseError::TooLarge));
    }

    #[test]
    fn crlf_is_normalized() {
        let doc = parse(CRLF, "text/plain", &limits()).unwrap();
        assert!(!doc.sections[0].text.contains('\r'));
        assert!(doc.sections[0].text.contains("line one\nline two"));
    }

    #[test]
    fn bom_and_control_chars_are_removed() {
        let doc = parse(BOM, "text/plain", &limits()).unwrap();
        let t = &doc.sections[0].text;
        assert!(t.starts_with("Visible"));
        assert!(!t
            .chars()
            .any(|c| c == '\u{FEFF}' || c == '\0' || c == '\u{7}'));
    }

    #[test]
    fn max_chars_is_enforced() {
        let l = ParseLimits {
            max_chars: 5,
            ..limits()
        };
        let err = parse(b"123456", "text/plain", &l).unwrap_err();
        assert!(matches!(err, ParseError::TooLarge));
        assert!(parse(b"12345", "text/plain", &l).is_ok());
    }

    #[test]
    fn unknown_mime_is_unsupported() {
        let err = parse(b"x", "image/png", &limits()).unwrap_err();
        assert!(matches!(err, ParseError::Unsupported));
    }

    #[test]
    fn whitespace_only_is_empty() {
        let err = parse(b" \r\n\t\n", "text/plain", &limits()).unwrap_err();
        assert!(matches!(err, ParseError::Empty));
    }
}
