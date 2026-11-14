//! Text normalization applied to each parsed section.

/// Normalizes one section. Idempotent: `normalize(normalize(x)) == normalize(x)`.
///
/// 1. `\r\n` and `\r` become `\n`.
/// 2. Control characters other than `\n` and `\t` are removed (including NUL and `U+FEFF`).
/// 3. Trailing whitespace is trimmed on each line.
/// 4. Three or more consecutive newlines collapse to two.
/// 5. PDF only: a hyphenated line break between lowercase letters is joined.
/// 6. The section is trimmed.
pub fn normalize(input: &str, is_pdf: bool) -> String {
    let s = normalize_newlines(input);
    let s = strip_control_chars(&s);
    let s = trim_line_ends(&s);
    let s = collapse_newlines(&s);
    let s = if is_pdf { join_hyphenated(&s) } else { s };
    s.trim().to_string()
}

pub(crate) fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

pub(crate) fn strip_control_chars(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\n' || c == '\t' || (!c.is_control() && c != '\u{FEFF}'))
        .collect()
}

pub(crate) fn trim_line_ends(s: &str) -> String {
    s.split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn collapse_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0usize;
    for c in s.chars() {
        if c == '\n' {
            run += 1;
            if run <= 2 {
                out.push(c);
            }
        } else {
            run = 0;
            out.push(c);
        }
    }
    out
}

pub(crate) fn join_hyphenated(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '-'
            && chars.get(i + 1) == Some(&'\n')
            && out.chars().next_back().is_some_and(char::is_lowercase)
            && chars.get(i + 2).is_some_and(|n| n.is_lowercase())
        {
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_crlf_and_cr() {
        assert_eq!(normalize("a\r\nb\rc", false), "a\nb\nc");
    }

    #[test]
    fn removes_control_chars_keeps_newline_and_tab() {
        assert_eq!(normalize("\u{FEFF}a\0b\u{7}c\td\ne", false), "abc\td\ne");
    }

    #[test]
    fn trims_trailing_whitespace_per_line() {
        assert_eq!(normalize("a  \nb\t\n  c", false), "a\nb\n  c");
    }

    #[test]
    fn collapses_three_or_more_newlines() {
        assert_eq!(normalize("a\n\n\n\n\nb\n\nc", false), "a\n\nb\n\nc");
    }

    #[test]
    fn whitespace_only_lines_count_as_blank() {
        assert_eq!(normalize("a\n \n\t\n \nb", false), "a\n\nb");
    }

    #[test]
    fn joins_hyphenated_break_in_pdf_only() {
        assert_eq!(normalize("exam-\nple", true), "example");
        assert_eq!(normalize("exam-\nple", false), "exam-\nple");
    }

    #[test]
    fn hyphen_join_requires_lowercase_both_sides() {
        assert_eq!(normalize("Exam-\nPle", true), "Exam-\nPle");
        assert_eq!(normalize("3-\nple", true), "3-\nple");
        assert_eq!(normalize("exam-\n\nple", true), "exam-\n\nple");
    }

    #[test]
    fn hyphen_join_chains() {
        assert_eq!(normalize("a-\nb-\nc", true), "abc");
    }

    #[test]
    fn trims_section() {
        assert_eq!(normalize("\n\n  hi  \n\n", false), "hi");
    }

    #[test]
    fn whitespace_only_becomes_empty() {
        assert_eq!(normalize(" \r\n\t \u{FEFF}\0", false), "");
    }

    #[test]
    fn is_idempotent() {
        let samples = [
            "a\r\n\r\n\r\n\r\nb  \r\n",
            "exam-\nple and fo-\nllow-\nup\n\n\n\nx",
            "\u{FEFF}\0 \n \t\n x \u{7}\n",
            "ab-\n\u{0}cd",
            "x\n \n \n \ny",
            "",
        ];
        for s in samples {
            for pdf in [false, true] {
                let once = normalize(s, pdf);
                assert_eq!(normalize(&once, pdf), once, "input {s:?} pdf={pdf}");
            }
        }
    }
}
