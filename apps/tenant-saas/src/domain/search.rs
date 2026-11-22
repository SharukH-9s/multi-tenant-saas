//! Search request validation and Reciprocal Rank Fusion. Pure: no I/O.
//!
//! ADR-008 S8: fusion runs in Rust so ties and ordering are deterministic and
//! unit-testable without a database.

use std::collections::{HashMap, HashSet};

use common_config::SearchConfig;
use common_errors::AppError;
use uuid::Uuid;

const MAX_DOCUMENT_IDS: usize = 100;

/// A validated search request.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchQuery {
    pub text: String,
    pub limit: usize,
    /// `None` means no document filter.
    pub document_ids: Option<Vec<Uuid>>,
}

impl SearchQuery {
    /// Trims the text, rejects empty or over-long text (counted in chars) and
    /// rejects a limit outside `1..=max_limit` instead of clamping it. An empty
    /// `document_ids` list means no filter; more than 100 is rejected;
    /// duplicates are removed (first occurrence kept).
    pub fn parse(
        text: &str,
        limit: Option<usize>,
        document_ids: Option<Vec<Uuid>>,
        cfg: &SearchConfig,
    ) -> Result<Self, AppError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(AppError::Validation("query must not be empty".into()));
        }
        if text.chars().count() > cfg.query_max_chars {
            return Err(AppError::Validation(format!(
                "query must be at most {} characters",
                cfg.query_max_chars
            )));
        }

        let limit = limit.unwrap_or(cfg.default_limit);
        if limit == 0 || limit > cfg.max_limit {
            return Err(AppError::Validation(format!(
                "limit must be between 1 and {}",
                cfg.max_limit
            )));
        }

        let document_ids = match document_ids {
            None => None,
            Some(ids) if ids.is_empty() => None,
            Some(ids) if ids.len() > MAX_DOCUMENT_IDS => {
                return Err(AppError::Validation(format!(
                    "document_ids must contain at most {MAX_DOCUMENT_IDS} ids"
                )));
            }
            Some(ids) => {
                let mut seen = HashSet::new();
                Some(ids.into_iter().filter(|id| seen.insert(*id)).collect())
            }
        };

        Ok(Self {
            text: text.to_string(),
            limit,
            document_ids,
        })
    }
}

/// A chunk's position in one branch's result list. `rank` is 1-based.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    pub chunk_id: Uuid,
    pub rank: usize,
}

/// A chunk after fusion.
#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub chunk_id: Uuid,
    pub score: f64,
    pub vector_rank: Option<usize>,
    pub lexical_rank: Option<usize>,
}

impl Fused {
    fn empty(chunk_id: Uuid) -> Self {
        Self {
            chunk_id,
            score: 0.0,
            vector_rank: None,
            lexical_rank: None,
        }
    }

    fn best_rank(&self) -> usize {
        match (self.vector_rank, self.lexical_rank) {
            (Some(v), Some(l)) => v.min(l),
            (Some(r), None) | (None, Some(r)) => r,
            (None, None) => usize::MAX,
        }
    }
}

/// Reciprocal Rank Fusion: `score = sum(1 / (k + rank))` over the branches a
/// chunk appears in. Order: score desc, best single rank asc, chunk_id asc.
/// If a chunk repeats inside one list, its best rank is used once.
pub fn rrf(vector: &[Candidate], lexical: &[Candidate], k: u32, limit: usize) -> Vec<Fused> {
    let mut by_chunk: HashMap<Uuid, Fused> = HashMap::new();

    for c in vector {
        let e = by_chunk
            .entry(c.chunk_id)
            .or_insert_with(|| Fused::empty(c.chunk_id));
        e.vector_rank = Some(e.vector_rank.map_or(c.rank, |r| r.min(c.rank)));
    }
    for c in lexical {
        let e = by_chunk
            .entry(c.chunk_id)
            .or_insert_with(|| Fused::empty(c.chunk_id));
        e.lexical_rank = Some(e.lexical_rank.map_or(c.rank, |r| r.min(c.rank)));
    }

    let k = f64::from(k);
    let mut out: Vec<Fused> = by_chunk
        .into_values()
        .map(|mut f| {
            f.score = [f.vector_rank, f.lexical_rank]
                .into_iter()
                .flatten()
                .map(|r| 1.0 / (k + r as f64))
                .sum();
            f
        })
        .collect();

    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(a.best_rank().cmp(&b.best_rank()))
            .then(a.chunk_id.cmp(&b.chunk_id))
    });
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn list(ids: &[u128]) -> Vec<Candidate> {
        ids.iter()
            .enumerate()
            .map(|(i, n)| Candidate {
                chunk_id: id(*n),
                rank: i + 1,
            })
            .collect()
    }

    fn cfg() -> SearchConfig {
        SearchConfig::default()
    }

    #[test]
    fn chunk_in_both_lists_at_rank_one_scores_double_and_wins() {
        let out = rrf(&list(&[1, 2]), &list(&[1, 3]), 60, 10);
        assert_eq!(out[0].chunk_id, id(1));
        assert!((out[0].score - 2.0 / 61.0).abs() < 1e-12);
        assert_eq!(out[0].vector_rank, Some(1));
        assert_eq!(out[0].lexical_rank, Some(1));
    }

    #[test]
    fn equal_scores_break_by_chunk_id() {
        let out = rrf(&list(&[9]), &list(&[5]), 60, 10);
        assert_eq!(out[0].score, out[1].score);
        assert_eq!(out[0].chunk_id, id(5));
        assert_eq!(out[1].chunk_id, id(9));
    }

    #[test]
    fn both_lists_at_rank_two_beat_rank_one_in_one_list() {
        // chunk 7: rank 2 in both = 2/62; chunk 1: rank 1 in one list = 1/61
        let out = rrf(&list(&[1, 7]), &list(&[2, 7]), 60, 10);
        assert_eq!(out[0].chunk_id, id(7));
        assert!((out[0].score - 2.0 / 62.0).abs() < 1e-12);
        assert!((out[1].score - 1.0 / 61.0).abs() < 1e-12);
    }

    #[test]
    fn empty_vector_list_gives_lexical_order() {
        let out = rrf(&[], &list(&[4, 3, 2]), 60, 10);
        let ids: Vec<_> = out.iter().map(|f| f.chunk_id).collect();
        assert_eq!(ids, vec![id(4), id(3), id(2)]);
        assert!(out.iter().all(|f| f.vector_rank.is_none()));
    }

    #[test]
    fn both_empty_gives_empty() {
        assert!(rrf(&[], &[], 60, 10).is_empty());
    }

    #[test]
    fn limit_truncates_after_sorting() {
        let out = rrf(&list(&[1, 2, 3]), &list(&[4, 5]), 60, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].chunk_id, id(1));
        assert_eq!(out[1].chunk_id, id(4));
    }

    #[test]
    fn same_input_gives_identical_output() {
        let v = list(&[1, 2, 3, 4]);
        let l = list(&[4, 3, 9, 1]);
        assert_eq!(rrf(&v, &l, 60, 10), rrf(&v, &l, 60, 10));
    }

    #[test]
    fn duplicate_within_one_list_keeps_best_rank_once() {
        let dup = vec![
            Candidate {
                chunk_id: id(1),
                rank: 3,
            },
            Candidate {
                chunk_id: id(1),
                rank: 1,
            },
        ];
        let out = rrf(&dup, &[], 60, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].vector_rank, Some(1));
        assert!((out[0].score - 1.0 / 61.0).abs() < 1e-12);
    }

    #[test]
    fn parse_trims_and_applies_default_limit() {
        let q = SearchQuery::parse("  hello  ", None, None, &cfg()).unwrap();
        assert_eq!(q.text, "hello");
        assert_eq!(q.limit, cfg().default_limit);
        assert_eq!(q.document_ids, None);
    }

    #[test]
    fn parse_rejects_bad_input() {
        let c = cfg();
        let too_long = "a".repeat(c.query_max_chars + 1);
        let many: Vec<Uuid> = (0..101).map(id).collect();
        let cases = vec![
            SearchQuery::parse("  ", None, None, &c),
            SearchQuery::parse(&too_long, None, None, &c),
            SearchQuery::parse("q", Some(0), None, &c),
            SearchQuery::parse("q", Some(c.max_limit + 1), None, &c),
            SearchQuery::parse("q", None, Some(many), &c),
        ];
        for r in cases {
            assert!(matches!(r, Err(AppError::Validation(_))), "{r:?}");
        }
    }

    #[test]
    fn parse_empty_document_ids_means_no_filter_and_dedupes() {
        let c = cfg();
        let q = SearchQuery::parse("q", None, Some(vec![]), &c).unwrap();
        assert_eq!(q.document_ids, None);
        let q = SearchQuery::parse("q", None, Some(vec![id(2), id(1), id(2)]), &c).unwrap();
        assert_eq!(q.document_ids, Some(vec![id(2), id(1)]));
    }

    #[test]
    fn parse_counts_chars_not_bytes() {
        let c = cfg();
        let exact = "é".repeat(c.query_max_chars);
        assert!(SearchQuery::parse(&exact, None, None, &c).is_ok());
        let over = "é".repeat(c.query_max_chars + 1);
        assert!(SearchQuery::parse(&over, None, None, &c).is_err());
    }
}
