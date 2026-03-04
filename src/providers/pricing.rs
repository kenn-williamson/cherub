//! Model pricing tables and cost computation (M12).
//!
//! Hard-coded pricing for known models. Returns `None` for unknown models,
//! which the caller treats as zero cost (unknown models are still tracked
//! by token count, just not priced).

use super::ApiUsage;

/// Per-model cost rates in USD per million tokens.
#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

/// Pure function: compute cost from usage + pricing.
pub fn compute_cost(usage: &ApiUsage, pricing: &ModelPricing) -> f64 {
    (usage.input_tokens as f64 / 1_000_000.0) * pricing.input_per_mtok
        + (usage.output_tokens as f64 / 1_000_000.0) * pricing.output_per_mtok
}

/// Look up pricing by model name. Returns `None` for unknown models.
///
/// Prices are per million tokens in USD. Updated as of 2025-05.
/// Source: <https://docs.anthropic.com/en/docs/about-claude/models>
pub fn lookup_pricing(model: &str) -> Option<ModelPricing> {
    // Match by prefix to handle dated model versions (e.g. "claude-sonnet-4-20250514").
    // More specific matches first.
    if model.starts_with("claude-opus-4") {
        Some(ModelPricing {
            input_per_mtok: 15.0,
            output_per_mtok: 75.0,
        })
    } else if model.starts_with("claude-sonnet-4") {
        Some(ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        })
    } else if model.starts_with("claude-haiku-4") {
        Some(ModelPricing {
            input_per_mtok: 0.80,
            output_per_mtok: 4.0,
        })
    } else if model.starts_with("claude-3-5-sonnet") || model.starts_with("claude-3.5-sonnet") {
        Some(ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        })
    } else if model.starts_with("claude-3-5-haiku") || model.starts_with("claude-3.5-haiku") {
        Some(ModelPricing {
            input_per_mtok: 0.80,
            output_per_mtok: 4.0,
        })
    } else if model.starts_with("claude-3-opus") {
        Some(ModelPricing {
            input_per_mtok: 15.0,
            output_per_mtok: 75.0,
        })
    } else if model.starts_with("claude-3-sonnet") {
        Some(ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        })
    } else if model.starts_with("claude-3-haiku") {
        Some(ModelPricing {
            input_per_mtok: 0.25,
            output_per_mtok: 1.25,
        })
    } else if model.starts_with("gpt-4o-mini") {
        Some(ModelPricing {
            input_per_mtok: 0.15,
            output_per_mtok: 0.60,
        })
    } else if model.starts_with("gpt-4o") {
        Some(ModelPricing {
            input_per_mtok: 2.50,
            output_per_mtok: 10.0,
        })
    } else if model.starts_with("gpt-4-turbo") {
        Some(ModelPricing {
            input_per_mtok: 10.0,
            output_per_mtok: 30.0,
        })
    } else if model.starts_with("gemini-1.5-pro") {
        Some(ModelPricing {
            input_per_mtok: 1.25,
            output_per_mtok: 5.0,
        })
    } else if model.starts_with("gemini-1.5-flash") {
        Some(ModelPricing {
            input_per_mtok: 0.075,
            output_per_mtok: 0.30,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_cost_known_values() {
        let usage = ApiUsage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
        };
        let pricing = ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        };
        let cost = compute_cost(&usage, &pricing);
        // 1M input * $3/M + 0.5M output * $15/M = $3 + $7.50 = $10.50
        assert!((cost - 10.5).abs() < 1e-10);
    }

    #[test]
    fn compute_cost_zero_tokens() {
        let usage = ApiUsage {
            input_tokens: 0,
            output_tokens: 0,
        };
        let pricing = ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        };
        assert!((compute_cost(&usage, &pricing)).abs() < 1e-10);
    }

    #[test]
    fn compute_cost_small_usage() {
        let usage = ApiUsage {
            input_tokens: 1000,
            output_tokens: 200,
        };
        let pricing = ModelPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        };
        // 0.001M * 3.0 + 0.0002M * 15.0 = 0.003 + 0.003 = 0.006
        let cost = compute_cost(&usage, &pricing);
        assert!((cost - 0.006).abs() < 1e-10);
    }

    #[test]
    fn lookup_known_models() {
        assert!(lookup_pricing("claude-sonnet-4-20250514").is_some());
        assert!(lookup_pricing("claude-opus-4-20250514").is_some());
        assert!(lookup_pricing("claude-haiku-4-20250514").is_some());
        assert!(lookup_pricing("claude-3-5-sonnet-20241022").is_some());
        assert!(lookup_pricing("gpt-4o").is_some());
        assert!(lookup_pricing("gpt-4o-mini").is_some());
        assert!(lookup_pricing("gemini-1.5-pro").is_some());
    }

    #[test]
    fn lookup_unknown_model_returns_none() {
        assert!(lookup_pricing("llama-3-70b").is_none());
        assert!(lookup_pricing("totally-unknown").is_none());
    }

    #[test]
    fn sonnet_4_pricing_values() {
        let p = lookup_pricing("claude-sonnet-4-20250514").unwrap();
        assert!((p.input_per_mtok - 3.0).abs() < 1e-10);
        assert!((p.output_per_mtok - 15.0).abs() < 1e-10);
    }

    #[test]
    fn opus_4_pricing_values() {
        let p = lookup_pricing("claude-opus-4-20250514").unwrap();
        assert!((p.input_per_mtok - 15.0).abs() < 1e-10);
        assert!((p.output_per_mtok - 75.0).abs() < 1e-10);
    }
}
