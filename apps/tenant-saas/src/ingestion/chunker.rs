//! Token-aware chunker.
//!
//! Everything is string-level except the hard split, so a chunk can never cut a
//! UTF-8 character. Output is deterministic, which idempotent ingestion needs.

use std::sync::Arc;

use tiktoken_rs::CoreBPE;

use super::parser::{atx_heading, ParsedDocument};

/// Version of parser + normalizer + chunker, stored per chunk.
pub const EMBEDDING_VERSION: &str = "p4-v1";

#[derive(Debug, Clone)]
pub struct ChunkerConfig {
    pub target_tokens: usize,
    pub overlap_tokens: usize,
    pub max_chunks: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub index: u32,
    pub content: String,
    pub token_count: u32,
    pub page_start: Option<u32>,
    pub page_end: Option<u32>,
    pub heading: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error("too many chunks")]
    TooMany,
    #[error("no content")]
    Empty,
}

pub struct Chunker {
    bpe: Arc<CoreBPE>,
    cfg: ChunkerConfig,
}

/// A unit that is never split further: a paragraph, a sentence, or a hard-split window.
struct Piece {
    text: String,
    /// Exact separator placed before this piece when it is not first in a chunk.
    sep: &'static str,
    tokens: usize,
    page: Option<u32>,
    heading: Option<String>,
}

const PARA_SEP: &str = "\n\n";

impl Chunker {
    pub fn new(bpe: Arc<CoreBPE>, cfg: ChunkerConfig) -> Self {
        let cfg = ChunkerConfig {
            target_tokens: cfg.target_tokens.max(1),
            ..cfg
        };
        Self { bpe, cfg }
    }

    fn count(&self, s: &str) -> usize {
        self.bpe.encode_ordinary(s).len()
    }

    pub fn chunk(&self, doc: &ParsedDocument) -> Result<Vec<Chunk>, ChunkError> {
        let pieces = self.pieces(doc);
        let mut chunks: Vec<Chunk> = Vec::new();

        // Indices into `pieces` currently in the open chunk, and how many of them are new
        // (not carried over as overlap).
        let mut cur: Vec<usize> = Vec::new();
        let mut cur_text = String::new();
        let mut fresh = 0usize;

        for idx in 0..pieces.len() {
            let piece = &pieces[idx];
            if cur.is_empty() {
                cur.push(idx);
                cur_text = piece.text.clone();
                fresh = 1;
                continue;
            }

            let candidate = format!("{cur_text}{}{}", piece.sep, piece.text);
            if self.count(&candidate) <= self.cfg.target_tokens {
                cur.push(idx);
                cur_text = candidate;
                fresh += 1;
                continue;
            }

            if fresh > 0 {
                self.emit(&pieces, &cur, &cur_text, &mut chunks)?;
                let carried = self.overlap(&pieces, &cur);
                cur_text = join(&pieces, &carried);
                cur = carried;
                let candidate = if cur.is_empty() {
                    piece.text.clone()
                } else {
                    format!("{cur_text}{}{}", piece.sep, piece.text)
                };
                if cur.is_empty() || self.count(&candidate) <= self.cfg.target_tokens {
                    cur.push(idx);
                    cur_text = candidate;
                } else {
                    cur = vec![idx];
                    cur_text = piece.text.clone();
                }
            } else {
                // Only overlap so far and the next piece does not fit with it: drop the overlap.
                cur = vec![idx];
                cur_text = piece.text.clone();
            }
            fresh = 1;
        }
        if fresh > 0 {
            self.emit(&pieces, &cur, &cur_text, &mut chunks)?;
        }

        if chunks.is_empty() {
            return Err(ChunkError::Empty);
        }
        Ok(chunks)
    }

    fn emit(
        &self,
        pieces: &[Piece],
        cur: &[usize],
        text: &str,
        out: &mut Vec<Chunk>,
    ) -> Result<(), ChunkError> {
        if text.trim().is_empty() {
            return Ok(());
        }
        if out.len() >= self.cfg.max_chunks {
            return Err(ChunkError::TooMany);
        }
        let pages = cur.iter().filter_map(|&i| pieces[i].page);
        let heading = cur.iter().find_map(|&i| pieces[i].heading.clone());
        out.push(Chunk {
            index: out.len() as u32,
            content: text.to_string(),
            token_count: self.count(text) as u32,
            page_start: pages.clone().min(),
            page_end: pages.max(),
            heading,
        });
        Ok(())
    }

    /// Trailing pieces of the closed chunk whose joined text is at most `overlap` tokens.
    fn overlap(&self, pieces: &[Piece], closed: &[usize]) -> Vec<usize> {
        let mut taken: Vec<usize> = Vec::new();
        if self.cfg.overlap_tokens == 0 {
            return taken;
        }
        for &i in closed.iter().rev() {
            if pieces[i].tokens > self.cfg.overlap_tokens {
                break;
            }
            let mut trial = vec![i];
            trial.extend(taken.iter().copied());
            if self.count(&join(pieces, &trial)) > self.cfg.overlap_tokens {
                break;
            }
            taken = trial;
        }
        taken
    }

    fn pieces(&self, doc: &ParsedDocument) -> Vec<Piece> {
        let target = self.cfg.target_tokens;
        let mut out = Vec::new();
        for section in &doc.sections {
            for para in section_paragraphs(&section.text) {
                let mk = |text: String, sep: &'static str, tokens: usize| Piece {
                    text,
                    sep,
                    tokens,
                    page: section.page,
                    heading: section.heading.clone(),
                };
                let tokens = self.count(&para);
                if tokens <= target {
                    out.push(mk(para, PARA_SEP, tokens));
                    continue;
                }
                let mut first = true;
                for (sentence, sep) in split_sentences(&para) {
                    let sep = if first { PARA_SEP } else { sep };
                    first = false;
                    let tokens = self.count(&sentence);
                    if tokens <= target {
                        out.push(mk(sentence, sep, tokens));
                        continue;
                    }
                    let mut first_window = true;
                    for window in self.hard_split(&sentence) {
                        let tokens = self.count(&window);
                        let sep = if first_window { sep } else { "" };
                        first_window = false;
                        out.push(mk(window, sep, tokens));
                    }
                }
            }
        }
        out
    }

    /// Splits by token windows of at most `target` tokens, never cutting a character.
    fn hard_split(&self, text: &str) -> Vec<String> {
        let target = self.cfg.target_tokens;
        let tokens = self.bpe.encode_ordinary(text);
        let mut out = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let remaining = tokens.len() - i;
            let max_w = target.min(remaining);
            let decode = |w: usize| self.bpe.decode(tokens[i..i + w].to_vec()).ok();

            // Shrink until the window ends on a character boundary and re-encodes within target.
            let mut chosen = (1..=max_w).rev().find_map(|w| {
                decode(w)
                    .filter(|s| self.count(s) <= target)
                    .map(|s| (w, s))
            });
            // A lone token may be a partial character; grow until the bytes are valid.
            if chosen.is_none() {
                chosen = (max_w + 1..=remaining).find_map(|w| decode(w).map(|s| (w, s)));
            }
            match chosen {
                Some((w, s)) => {
                    out.push(s);
                    i += w;
                }
                None => {
                    // Unreachable for valid input: the full remainder always decodes.
                    let rest = self.bpe.decode(tokens[i..].to_vec()).unwrap_or_default();
                    out.push(rest);
                    break;
                }
            }
        }
        out
    }
}

fn join(pieces: &[Piece], idxs: &[usize]) -> String {
    let mut s = String::new();
    for (k, &i) in idxs.iter().enumerate() {
        if k > 0 {
            s.push_str(pieces[i].sep);
        }
        s.push_str(&pieces[i].text);
    }
    s
}

/// Paragraphs of a section. A heading paragraph merges with the paragraph after it so a
/// heading never ends a chunk on its own.
fn section_paragraphs(text: &str) -> Vec<String> {
    let paras: Vec<&str> = text
        .split("\n\n")
        .map(|p| p.trim_matches('\n'))
        .filter(|p| !p.trim().is_empty())
        .collect();

    let is_heading = |p: &str| !p.contains('\n') && atx_heading(p).is_some();
    let mut out = Vec::new();
    let mut pending = String::new();
    for p in paras {
        if !pending.is_empty() {
            pending.push_str(PARA_SEP);
        }
        pending.push_str(p);
        if !is_heading(p) {
            out.push(std::mem::take(&mut pending));
        }
    }
    if !pending.is_empty() {
        out.push(pending);
    }
    out
}

/// Breaks after `. `, `? `, `! ` or a single `\n`. Returns each sentence with the
/// whitespace that preceded it (the first one gets a placeholder the caller replaces).
fn split_sentences(para: &str) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    let mut start = 0;
    let mut sep: &'static str = PARA_SEP;
    let bytes = para.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let (end, next_start, next_sep) = match b {
            b'.' | b'?' | b'!' if bytes.get(i + 1) == Some(&b' ') => {
                let mut j = i + 1;
                while bytes.get(j) == Some(&b' ') {
                    j += 1;
                }
                (i + 1, j, " ")
            }
            b'\n' => (i, i + 1, "\n"),
            _ => {
                i += 1;
                continue;
            }
        };
        let s = &para[start..end];
        if !s.trim().is_empty() {
            out.push((s.to_string(), sep));
            sep = next_sep;
        } else if out.is_empty() {
            // Leading blank: keep the placeholder separator.
        } else {
            sep = next_sep;
        }
        start = next_start;
        i = next_start;
    }
    let s = &para[start..];
    if !s.trim().is_empty() {
        out.push((s.to_string(), sep));
    }
    out
}

/// Encoding bound to the embedding model. `text-embedding-3-*` and `ada-002` use
/// `cl100k_base`. An unknown model falls back to `cl100k_base`, so token counts are approximate.
pub fn tokenizer_for_model(model: &str) -> anyhow::Result<CoreBPE> {
    if model.starts_with("text-embedding-3") || model == "text-embedding-ada-002" {
        return tiktoken_rs::cl100k_base();
    }
    match tiktoken_rs::get_bpe_from_model(model) {
        Ok(bpe) => Ok(bpe),
        Err(_) => {
            tracing::warn!(
                model,
                "unknown embedding model; using cl100k_base, token counts are approximate"
            );
            tiktoken_rs::cl100k_base()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingestion::parser::{parse, ParseLimits, Section};

    const TWO_PAGES: &[u8] = include_bytes!("../../tests/fixtures/ingestion/two_pages.pdf");

    fn bpe() -> Arc<CoreBPE> {
        Arc::new(tokenizer_for_model("text-embedding-3-small").unwrap())
    }

    fn chunker(target: usize, overlap: usize, max_chunks: usize) -> Chunker {
        Chunker::new(
            bpe(),
            ChunkerConfig {
                target_tokens: target,
                overlap_tokens: overlap,
                max_chunks,
            },
        )
    }

    fn doc(sections: Vec<Section>) -> ParsedDocument {
        let char_count = sections.iter().map(|s| s.text.chars().count()).sum();
        ParsedDocument {
            sections,
            char_count,
        }
    }

    fn text_doc(text: &str) -> ParsedDocument {
        doc(vec![Section {
            text: text.to_string(),
            page: None,
            heading: None,
        }])
    }

    fn no_ws(s: &str) -> String {
        s.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// 10k words in paragraphs of sentences, every word unique so overlaps are unambiguous.
    fn big_text() -> String {
        let mut paras = Vec::new();
        let mut n = 0;
        for _ in 0..500 {
            let mut sentences = Vec::new();
            for _ in 0..1 {
                let words: Vec<String> = (0..20)
                    .map(|_| {
                        n += 1;
                        format!("w{n}")
                    })
                    .collect();
                sentences.push(format!("{}.", words.join(" ")));
            }
            paras.push(sentences.join(" "));
        }
        paras.join("\n\n")
    }

    #[test]
    fn token_counts_are_within_target_and_exact() {
        let c = chunker(500, 75, 10_000);
        let chunks = c.chunk(&text_doc(&big_text())).unwrap();
        assert!(chunks.len() > 10);
        for ch in &chunks {
            assert!(ch.token_count as usize <= 500, "chunk {}", ch.index);
            assert_eq!(ch.token_count as usize, c.count(&ch.content));
        }
        for (i, ch) in chunks.iter().enumerate() {
            assert_eq!(ch.index as usize, i);
        }
    }

    #[test]
    fn neighbours_share_overlap_within_limit() {
        let c = chunker(500, 75, 10_000);
        let chunks = c.chunk(&text_doc(&big_text())).unwrap();
        let mut with_overlap = 0;
        for pair in chunks.windows(2) {
            let (a, b) = (&pair[0].content, &pair[1].content);
            let shared = b
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(b.len()))
                .rev()
                .find(|&k| k > 0 && a.ends_with(&b[..k]))
                .map(|k| &b[..k])
                .unwrap_or("");
            assert!(c.count(shared) <= 75);
            if !shared.is_empty() {
                with_overlap += 1;
            }
        }
        assert!(with_overlap > chunks.len() / 2, "{with_overlap}");
    }

    #[test]
    fn output_is_deterministic() {
        let c = chunker(500, 75, 10_000);
        let d = text_doc(&big_text());
        assert_eq!(c.chunk(&d).unwrap(), c.chunk(&d).unwrap());
    }

    #[test]
    fn unicode_is_never_cut_or_lost() {
        let c = chunker(7, 0, 100_000);
        let units = ["漢字仮名交じり文", "😀🎉👨‍👩‍👧", "e\u{301}a\u{308}n\u{303}"];
        for unit in units {
            for reps in [1, 2, 3, 5, 8, 13, 21, 40] {
                let text = unit.repeat(reps);
                let chunks = c.chunk(&text_doc(&text)).unwrap();
                let joined: String = chunks.iter().map(|ch| ch.content.as_str()).collect();
                assert!(!joined.contains('\u{FFFD}'));
                assert_eq!(joined, text, "unit {unit:?} reps {reps}");
                for ch in &chunks {
                    assert!(ch.token_count <= 7);
                }
            }
        }
    }

    #[test]
    fn long_unpunctuated_sentence_is_hard_split() {
        let c = chunker(500, 0, 10_000);
        let text: String = (0..3000).map(|i| format!("x{i} ")).collect();
        assert!(c.count(&text) > 3000);
        let chunks = c.chunk(&text_doc(&text)).unwrap();
        assert!(chunks.len() >= 6);
        for ch in &chunks {
            assert!(ch.token_count <= 500);
        }
        let joined: String = chunks.iter().map(|ch| ch.content.as_str()).collect();
        assert_eq!(no_ws(&joined), no_ws(&text));
    }

    #[test]
    fn heading_stays_with_its_paragraph_at_a_boundary() {
        let c = chunker(30, 0, 1000);
        let filler = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron pi rho";
        let body = "Body paragraph text here.";
        for pad in 0..12 {
            let first = format!("{filler}{}", " word".repeat(pad));
            let d = doc(vec![
                Section {
                    text: first,
                    page: None,
                    heading: None,
                },
                Section {
                    text: format!("# Head\n\n{body}"),
                    page: None,
                    heading: Some("Head".into()),
                },
            ]);
            let chunks = c.chunk(&d).unwrap();
            assert!(
                chunks
                    .iter()
                    .all(|ch| !ch.content.trim_end().ends_with("# Head")),
                "pad {pad}: {chunks:?}"
            );
            assert!(
                chunks
                    .iter()
                    .any(|ch| ch.content.contains(&format!("# Head\n\n{body}"))),
                "pad {pad}: {chunks:?}"
            );
        }
    }

    #[test]
    fn pdf_pages_are_tracked() {
        let limits = ParseLimits {
            max_pdf_pages: 10,
            max_chars: 100_000,
        };
        let parsed = parse(TWO_PAGES, "application/pdf", &limits).unwrap();

        let one = chunker(500, 0, 10).chunk(&parsed).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!((one[0].page_start, one[0].page_end), (Some(1), Some(2)));

        let two = chunker(4, 0, 10).chunk(&parsed).unwrap();
        assert_eq!(two.len(), 2);
        assert_eq!((two[0].page_start, two[0].page_end), (Some(1), Some(1)));
        assert_eq!((two[1].page_start, two[1].page_end), (Some(2), Some(2)));
    }

    #[test]
    fn heading_is_first_heading_in_chunk() {
        let d = doc(vec![
            Section {
                text: "intro".into(),
                page: None,
                heading: None,
            },
            Section {
                text: "# A\n\nbody".into(),
                page: None,
                heading: Some("A".into()),
            },
        ]);
        let chunks = chunker(500, 0, 10).chunk(&d).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].heading.as_deref(), Some("A"));
    }

    #[test]
    fn too_many_chunks() {
        let err = chunker(500, 75, 3)
            .chunk(&text_doc(&big_text()))
            .unwrap_err();
        assert!(matches!(err, ChunkError::TooMany));
    }

    #[test]
    fn whitespace_only_is_empty() {
        let err = chunker(500, 75, 10)
            .chunk(&text_doc("  \n\n \t \n\n"))
            .unwrap_err();
        assert!(matches!(err, ChunkError::Empty));
        let err = chunker(500, 75, 10).chunk(&doc(vec![])).unwrap_err();
        assert!(matches!(err, ChunkError::Empty));
    }

    #[test]
    fn overlap_larger_than_last_piece_budget_is_dropped() {
        // Each paragraph is ~10 tokens; overlap 5 can't carry one, so chunks must not overlap.
        let c = chunker(25, 5, 1000);
        let text = (0..10)
            .map(|i| format!("one two three four five six seven eight nine ten{i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let chunks = c.chunk(&text_doc(&text)).unwrap();
        let joined: String = chunks.iter().map(|ch| no_ws(&ch.content)).collect();
        assert_eq!(joined, no_ws(&text));
    }

    #[test]
    fn tokenizer_for_embedding_model_is_cl100k() {
        let bpe = tokenizer_for_model("text-embedding-3-small").unwrap();
        let reference = tiktoken_rs::cl100k_base().unwrap();
        assert_eq!(bpe.encode_ordinary("hello world").len(), 2);
        assert_eq!(
            bpe.encode_ordinary("hello world"),
            reference.encode_ordinary("hello world")
        );
        assert!(tokenizer_for_model("some-unknown-model").is_ok());
    }
}
