//! Reciprocal Rank Fusion (RRF) for hybrid FTS + vector search (M6c).
//!
//! All functions here are pure — no DB, no async, fully unit-testable.

use uuid::Uuid;

/// Configuration for the RRF fusion step.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    /// Ranking constant (higher k = less penalty for lower ranks). Default: 60.
    pub rrf_k: u32,
    /// How many results to fetch from each individual ranker before fusion.
    pub pre_fusion_limit: usize,
    /// Minimum normalized RRF score to include in output. Default: 0.0.
    pub min_score: f32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            rrf_k: 60,
            pre_fusion_limit: 50,
            min_score: 0.0,
        }
    }
}

/// A ranked result from a single search method (FTS or vector).
#[derive(Debug, Clone)]
pub struct RankedResult {
    pub memory_id: Uuid,
    /// 1-based rank within its result set (1 = best).
    pub rank: usize,
    /// Confidence of the underlying memory (0.0–1.0). Used for score weighting.
    pub confidence: f32,
}

/// A fused result after applying RRF across multiple result sets.
#[derive(Debug, Clone)]
pub struct ScoredResult {
    pub memory_id: Uuid,
    /// Normalized RRF score × confidence (0.0–1.0, higher is better).
    pub score: f32,
    /// Rank this memory had in the FTS result set (None if absent).
    pub fts_rank: Option<usize>,
    /// Rank this memory had in the vector result set (None if absent).
    pub vector_rank: Option<usize>,
}

/// Combine FTS and vector result sets using Reciprocal Rank Fusion.
///
/// Algorithm:
/// 1. Accumulate `1 / (k + rank)` per memory across all result sets.
/// 2. Normalize scores to [0, 1] by dividing by the maximum.
/// 3. Multiply by the memory's confidence (confidence weighting).
/// 4. Filter by `config.min_score`, sort descending, truncate to `limit`.
///
/// Memories that appear in both result sets receive a boost from the sum of
/// their per-set RRF contributions.
pub fn reciprocal_rank_fusion(
    fts: &[RankedResult],
    vector: &[RankedResult],
    config: &SearchConfig,
    limit: usize,
) -> Vec<ScoredResult> {
    use std::collections::HashMap;

    // (rrf_score_sum, fts_rank, vector_rank, confidence)
    let mut acc: HashMap<Uuid, (f32, Option<usize>, Option<usize>, f32)> = HashMap::new();

    let k = config.rrf_k as f32;

    for r in fts {
        let entry = acc
            .entry(r.memory_id)
            .or_insert((0.0, None, None, r.confidence));
        entry.0 += 1.0 / (k + r.rank as f32);
        entry.1 = Some(r.rank);
    }

    for r in vector {
        let entry = acc
            .entry(r.memory_id)
            .or_insert((0.0, None, None, r.confidence));
        entry.0 += 1.0 / (k + r.rank as f32);
        entry.2 = Some(r.rank);
        // Use max confidence between the two result sets.
        if r.confidence > entry.3 {
            entry.3 = r.confidence;
        }
    }

    if acc.is_empty() {
        return Vec::new();
    }

    // Normalize to [0, 1].
    let max_score = acc
        .values()
        .map(|(s, ..)| *s)
        .fold(f32::NEG_INFINITY, f32::max);

    let mut results: Vec<ScoredResult> = acc
        .into_iter()
        .map(|(id, (rrf_sum, fts_r, vec_r, conf))| {
            let normalized = if max_score > 0.0 {
                rrf_sum / max_score
            } else {
                0.0
            };
            ScoredResult {
                memory_id: id,
                score: normalized * conf,
                fts_rank: fts_r,
                vector_rank: vec_r,
            }
        })
        .filter(|r| r.score >= config.min_score)
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(limit);
    results
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(n: usize) -> Uuid {
        Uuid::from_u128(n as u128)
    }

    fn fts(memory_id: Uuid, rank: usize) -> RankedResult {
        RankedResult {
            memory_id,
            rank,
            confidence: 1.0,
        }
    }

    fn vec_r(memory_id: Uuid, rank: usize) -> RankedResult {
        RankedResult {
            memory_id,
            rank,
            confidence: 1.0,
        }
    }

    fn cfg() -> SearchConfig {
        SearchConfig::default()
    }

    #[test]
    fn empty_inputs_return_empty_output() {
        let result = reciprocal_rank_fusion(&[], &[], &cfg(), 10);
        assert!(result.is_empty());
    }

    #[test]
    fn fts_only_returns_results_in_rank_order() {
        let fts_results = vec![fts(uid(1), 1), fts(uid(2), 2), fts(uid(3), 3)];
        let result = reciprocal_rank_fusion(&fts_results, &[], &cfg(), 10);

        assert_eq!(result.len(), 3);
        // Score should decrease with rank.
        assert!(result[0].score >= result[1].score);
        assert!(result[1].score >= result[2].score);
        assert_eq!(result[0].memory_id, uid(1));
        // All from FTS, none from vector.
        assert!(result[0].fts_rank.is_some());
        assert!(result[0].vector_rank.is_none());
    }

    #[test]
    fn vector_only_returns_results_in_rank_order() {
        let vec_results = vec![vec_r(uid(10), 1), vec_r(uid(11), 2)];
        let result = reciprocal_rank_fusion(&[], &vec_results, &cfg(), 10);

        assert_eq!(result.len(), 2);
        assert!(result[0].score >= result[1].score);
        assert_eq!(result[0].memory_id, uid(10));
        assert!(result[0].vector_rank.is_some());
        assert!(result[0].fts_rank.is_none());
    }

    #[test]
    fn hybrid_match_boosted_above_single_method() {
        // uid(1): appears in both FTS rank 2 and vector rank 2.
        // uid(2): appears only in FTS rank 1 (best single-method).
        // uid(1) should win because dual-method boost outweighs single-method top rank.
        let fts_results = vec![fts(uid(2), 1), fts(uid(1), 2)];
        let vec_results = vec![vec_r(uid(1), 2)];

        let result = reciprocal_rank_fusion(&fts_results, &vec_results, &cfg(), 10);

        // uid(1) scores 2×(1/(60+2)) = 0.0323; uid(2) scores 1×(1/(60+1)) = 0.0164
        let uid1_pos = result.iter().position(|r| r.memory_id == uid(1)).unwrap();
        let uid2_pos = result.iter().position(|r| r.memory_id == uid(2)).unwrap();
        assert!(
            uid1_pos < uid2_pos,
            "hybrid match should rank above FTS-only top result"
        );
    }

    #[test]
    fn confidence_weighting_applied() {
        let low_conf = RankedResult {
            memory_id: uid(1),
            rank: 1,
            confidence: 0.5,
        };
        let high_conf = RankedResult {
            memory_id: uid(2),
            rank: 2,
            confidence: 1.0,
        };

        let result = reciprocal_rank_fusion(&[low_conf, high_conf], &[], &cfg(), 10);

        // uid(2) has rank 2 but conf 1.0; uid(1) has rank 1 but conf 0.5.
        // uid(1) raw RRF = 1/(60+1) ≈ 0.0164, normalized = 1.0, × 0.5 = 0.5
        // uid(2) raw RRF = 1/(60+2) ≈ 0.0161, normalized ≈ 0.98, × 1.0 = 0.98
        let uid2_pos = result.iter().position(|r| r.memory_id == uid(2)).unwrap();
        let uid1_pos = result.iter().position(|r| r.memory_id == uid(1)).unwrap();
        assert!(
            uid2_pos < uid1_pos,
            "high confidence should outweigh better raw rank"
        );
    }

    #[test]
    fn min_score_filter_excludes_low_scores() {
        let fts_results = vec![fts(uid(1), 1), fts(uid(2), 2)];
        // Both scores are normalized: top = 1.0 × 1.0 = 1.0, second ≈ 0.98.
        // min_score = 0.99 should keep only the top result.
        let config = SearchConfig {
            min_score: 0.99,
            ..Default::default()
        };
        let result = reciprocal_rank_fusion(&fts_results, &[], &config, 10);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].memory_id, uid(1));
    }

    #[test]
    fn limit_truncates_output() {
        let fts_results: Vec<RankedResult> = (1..=10).map(|i| fts(uid(i), i)).collect();
        let result = reciprocal_rank_fusion(&fts_results, &[], &cfg(), 3);

        assert_eq!(result.len(), 3);
    }

    #[test]
    fn normalized_score_max_is_one() {
        let fts_results = vec![fts(uid(1), 1), fts(uid(2), 2)];
        let result = reciprocal_rank_fusion(&fts_results, &[], &cfg(), 10);

        // The top result should have score == confidence (1.0) after normalization.
        assert!(
            (result[0].score - 1.0).abs() < 1e-6,
            "top score should be 1.0 after normalization"
        );
    }
}
