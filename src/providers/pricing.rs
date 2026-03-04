//! Model pricing and cost computation (M12).
//!
//! Each provider owns its pricing via `Provider::pricing()`. This module
//! provides the shared `ModelPricing` struct and `compute_cost()` function.

use super::ApiUsage;

/// Per-model cost rates in USD per million tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    /// Cache write rate (e.g. Anthropic: 125% of input rate). 0.0 if no caching.
    pub cache_write_per_mtok: f64,
    /// Cache read rate (e.g. Anthropic: 10% of input rate). 0.0 if no caching.
    pub cache_read_per_mtok: f64,
}

/// Pure function: compute cost from usage + pricing.
pub fn compute_cost(usage: &ApiUsage, pricing: &ModelPricing) -> f64 {
    (usage.input_tokens as f64 / 1_000_000.0) * pricing.input_per_mtok
        + (usage.output_tokens as f64 / 1_000_000.0) * pricing.output_per_mtok
        + (usage.cache_creation_tokens as f64 / 1_000_000.0) * pricing.cache_write_per_mtok
        + (usage.cache_read_tokens as f64 / 1_000_000.0) * pricing.cache_read_per_mtok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_cost_known_values() {
        let usage = ApiUsage::new(1_000_000, 500_000);
        let pricing = ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.0,
        };
        let cost = compute_cost(&usage, &pricing);
        // 1M input * $3/M + 0.5M output * $15/M = $3 + $7.50 = $10.50
        assert!((cost - 10.5).abs() < 1e-10);
    }

    #[test]
    fn compute_cost_zero_tokens() {
        let usage = ApiUsage::new(0, 0);
        let pricing = ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.0,
        };
        assert!((compute_cost(&usage, &pricing)).abs() < 1e-10);
    }

    #[test]
    fn compute_cost_small_usage() {
        let usage = ApiUsage::new(1000, 200);
        let pricing = ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.0,
        };
        // 0.001M * 3.0 + 0.0002M * 15.0 = 0.003 + 0.003 = 0.006
        let cost = compute_cost(&usage, &pricing);
        assert!((cost - 0.006).abs() < 1e-10);
    }

    #[test]
    fn compute_cost_with_cache_tokens() {
        let usage = ApiUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_creation_tokens: 200_000,
            cache_read_tokens: 800_000,
        };
        let pricing = ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_write_per_mtok: 3.75, // 125% of input
            cache_read_per_mtok: 0.30,  // 10% of input
        };
        // 1M * 3.0 + 0.5M * 15.0 + 0.2M * 3.75 + 0.8M * 0.30
        // = 3.0 + 7.5 + 0.75 + 0.24 = 11.49
        let cost = compute_cost(&usage, &pricing);
        assert!((cost - 11.49).abs() < 1e-10);
    }

    #[test]
    fn compute_cost_cache_with_zero_rates() {
        // Cache tokens present but rates are zero → no extra cost.
        let usage = ApiUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_creation_tokens: 100_000,
            cache_read_tokens: 200_000,
        };
        let pricing = ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_write_per_mtok: 0.0,
            cache_read_per_mtok: 0.0,
        };
        let cost = compute_cost(&usage, &pricing);
        assert!((cost - 10.5).abs() < 1e-10);
    }

    #[test]
    fn api_usage_new_defaults_cache_to_zero() {
        let usage = ApiUsage::new(100, 200);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 200);
        assert_eq!(usage.cache_creation_tokens, 0);
        assert_eq!(usage.cache_read_tokens, 0);
    }
}
