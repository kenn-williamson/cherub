//! Failover provider: wraps multiple providers and tries them in order.
//!
//! On `CherubError::Provider`, the next provider is attempted. Non-Provider
//! errors (e.g., `NotPermitted`, `InvalidInvocation`) propagate immediately.
//! A circuit breaker per provider avoids hammering providers that are down.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::{Instrument, info, info_span, warn};

use super::{ApiUsage, Message, Provider, ToolDefinition};
use crate::error::CherubError;

const DEFAULT_FAILURE_THRESHOLD: u32 = 3;
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);

/// Per-provider circuit breaker state.
///
/// Two logical states: closed (healthy) and open (tripped). After the cooldown
/// expires, the circuit is implicitly half-open — the next request probes the
/// provider. Success resets to closed; failure re-trips to open.
struct CircuitState {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
}

impl CircuitState {
    fn new() -> Self {
        Self {
            consecutive_failures: 0,
            opened_at: None,
        }
    }

    /// Returns true if the circuit is open and the cooldown has not yet expired.
    fn is_open(&self, cooldown: Duration) -> bool {
        match self.opened_at {
            Some(opened) => opened.elapsed() < cooldown,
            None => false,
        }
    }

    /// Record a failure. If the threshold is reached, trip the circuit.
    fn record_failure(&mut self, threshold: u32) {
        self.consecutive_failures += 1;
        if self.consecutive_failures >= threshold {
            self.opened_at = Some(Instant::now());
        }
    }

    /// Record a success. Resets failure count and closes the circuit.
    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.opened_at = None;
    }
}

/// Provider that tries multiple child providers in order, with circuit breaker
/// logic to skip providers that are consistently failing.
///
/// `std::sync::Mutex` is justified: the `Provider` trait requires `&self` for
/// `complete()`, but circuit breaker state must be mutable. The critical section
/// is tiny (read/write a counter + timestamp), no async work while holding the lock.
pub struct FailoverProvider {
    providers: Vec<Box<dyn Provider>>,
    provider_names: Vec<String>,
    circuits: Mutex<Vec<CircuitState>>,
    last_success_idx: Mutex<usize>,
    failure_threshold: u32,
    cooldown: Duration,
}

impl FailoverProvider {
    /// Create a new failover provider wrapping the given providers.
    ///
    /// # Panics
    ///
    /// Panics if `providers` is empty or if `providers` and `names` have different lengths.
    pub fn new(providers: Vec<Box<dyn Provider>>, names: Vec<String>) -> Self {
        assert!(
            !providers.is_empty(),
            "failover requires at least one provider"
        );
        assert_eq!(
            providers.len(),
            names.len(),
            "provider and name counts must match"
        );
        let circuits = (0..providers.len()).map(|_| CircuitState::new()).collect();
        Self {
            providers,
            provider_names: names,
            circuits: Mutex::new(circuits),
            last_success_idx: Mutex::new(0),
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
            cooldown: DEFAULT_COOLDOWN,
        }
    }

    /// Override the failure threshold (number of consecutive failures to trip circuit).
    pub fn with_failure_threshold(mut self, threshold: u32) -> Self {
        self.failure_threshold = threshold;
        self
    }

    /// Override the cooldown duration before a tripped circuit is probed again.
    pub fn with_cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }
}

#[async_trait]
impl Provider for FailoverProvider {
    async fn complete(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<(Message, Option<ApiUsage>), CherubError> {
        async {
            let mut last_error = None;

            for (idx, provider) in self.providers.iter().enumerate() {
                // Check circuit breaker — skip if open.
                {
                    let circuits = self.circuits.lock().expect("circuit mutex poisoned");
                    if circuits[idx].is_open(self.cooldown) {
                        info!(
                            provider = %self.provider_names[idx],
                            "skipping provider (circuit open)"
                        );
                        continue;
                    }
                }

                info!(
                    provider = %self.provider_names[idx],
                    idx,
                    "attempting provider"
                );

                match provider.complete(system, messages, tools).await {
                    Ok(result) => {
                        // Record success.
                        {
                            let mut circuits =
                                self.circuits.lock().expect("circuit mutex poisoned");
                            circuits[idx].record_success();
                        }
                        {
                            let mut last = self
                                .last_success_idx
                                .lock()
                                .expect("last_success mutex poisoned");
                            *last = idx;
                        }
                        info!(
                            provider = %self.provider_names[idx],
                            "provider succeeded"
                        );
                        return Ok(result);
                    }
                    Err(CherubError::Provider(ref msg)) => {
                        // Transient provider error — record failure, try next.
                        warn!(
                            provider = %self.provider_names[idx],
                            error = %msg,
                            "provider failed, trying next"
                        );
                        {
                            let mut circuits =
                                self.circuits.lock().expect("circuit mutex poisoned");
                            circuits[idx].record_failure(self.failure_threshold);
                            if circuits[idx].opened_at.is_some() {
                                warn!(
                                    provider = %self.provider_names[idx],
                                    threshold = self.failure_threshold,
                                    "circuit breaker tripped"
                                );
                            }
                        }
                        last_error = Some(CherubError::Provider(msg.clone()));
                    }
                    Err(e) => {
                        // Non-Provider error — propagate immediately (e.g., NotPermitted).
                        return Err(e);
                    }
                }
            }

            // All providers failed (or were circuit-broken).
            Err(last_error.unwrap_or_else(|| {
                CherubError::Provider("all providers circuit-broken".to_owned())
            }))
        }
        .instrument(info_span!("failover_complete"))
        .await
    }

    fn model_name(&self) -> &str {
        let idx = *self
            .last_success_idx
            .lock()
            .expect("last_success mutex poisoned");
        self.providers[idx].model_name()
    }

    fn max_output_tokens(&self) -> u32 {
        self.providers
            .iter()
            .map(|p| p.max_output_tokens())
            .min()
            .expect("failover has at least one provider")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock provider that returns a configurable result.
    struct MockProvider {
        name: String,
        max_tokens: u32,
        result: Mutex<Vec<Result<(Message, Option<ApiUsage>), CherubError>>>,
    }

    impl MockProvider {
        fn succeeding(name: &str, max_tokens: u32) -> Self {
            Self {
                name: name.to_owned(),
                max_tokens,
                result: Mutex::new(Vec::new()),
            }
        }

        fn with_results(
            name: &str,
            max_tokens: u32,
            results: Vec<Result<(Message, Option<ApiUsage>), CherubError>>,
        ) -> Self {
            // Results are stored in reverse so we can pop from the end.
            let mut reversed = results;
            reversed.reverse();
            Self {
                name: name.to_owned(),
                max_tokens,
                result: Mutex::new(reversed),
            }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn complete(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<(Message, Option<ApiUsage>), CherubError> {
            let mut results = self.result.lock().unwrap();
            if let Some(result) = results.pop() {
                result
            } else {
                // Default: return a simple success.
                Ok((
                    Message::Assistant {
                        content: vec![super::super::ContentBlock::Text {
                            text: format!("ok from {}", self.name),
                        }],
                        stop_reason: super::super::StopReason::EndTurn,
                    },
                    Some(ApiUsage::new(10, 5)),
                ))
            }
        }

        fn model_name(&self) -> &str {
            &self.name
        }

        fn max_output_tokens(&self) -> u32 {
            self.max_tokens
        }
    }

    fn ok_result(name: &str) -> Result<(Message, Option<ApiUsage>), CherubError> {
        Ok((
            Message::Assistant {
                content: vec![super::super::ContentBlock::Text {
                    text: format!("ok from {name}"),
                }],
                stop_reason: super::super::StopReason::EndTurn,
            },
            Some(ApiUsage::new(10, 5)),
        ))
    }

    fn provider_err(msg: &str) -> Result<(Message, Option<ApiUsage>), CherubError> {
        Err(CherubError::Provider(msg.to_owned()))
    }

    #[tokio::test]
    async fn single_provider_success() {
        let p = MockProvider::succeeding("model-a", 4096);
        let failover = FailoverProvider::new(vec![Box::new(p)], vec!["primary".to_owned()]);

        let messages = vec![Message::user_text("hello")];
        let result = failover.complete("system", &messages, &[]).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn failover_to_second_provider() {
        let p1 = MockProvider::with_results("model-a", 4096, vec![provider_err("down")]);
        let p2 = MockProvider::with_results("model-b", 4096, vec![ok_result("model-b")]);
        let failover = FailoverProvider::new(
            vec![Box::new(p1), Box::new(p2)],
            vec!["primary".to_owned(), "secondary".to_owned()],
        );

        let messages = vec![Message::user_text("hello")];
        let result = failover.complete("system", &messages, &[]).await;
        assert!(result.is_ok());

        // Verify last_success_idx points to the second provider.
        assert_eq!(failover.model_name(), "model-b");
    }

    #[tokio::test]
    async fn all_providers_fail() {
        let p1 = MockProvider::with_results("model-a", 4096, vec![provider_err("down-a")]);
        let p2 = MockProvider::with_results("model-b", 4096, vec![provider_err("down-b")]);
        let failover = FailoverProvider::new(
            vec![Box::new(p1), Box::new(p2)],
            vec!["primary".to_owned(), "secondary".to_owned()],
        );

        let messages = vec![Message::user_text("hello")];
        let result = failover.complete("system", &messages, &[]).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("down-b"), "should contain last error: {err}");
    }

    #[tokio::test]
    async fn non_provider_error_propagates_immediately() {
        let p1 = MockProvider::with_results(
            "model-a",
            4096,
            vec![Err(CherubError::InvalidInvocation("bad call".to_owned()))],
        );
        let p2 = MockProvider::with_results("model-b", 4096, vec![ok_result("model-b")]);
        let failover = FailoverProvider::new(
            vec![Box::new(p1), Box::new(p2)],
            vec!["primary".to_owned(), "secondary".to_owned()],
        );

        let messages = vec![Message::user_text("hello")];
        let result = failover.complete("system", &messages, &[]).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // Should be InvalidInvocation, not Provider.
        assert!(
            err.contains("bad call"),
            "should propagate non-Provider error: {err}"
        );
    }

    #[tokio::test]
    async fn circuit_breaker_trips_after_threshold() {
        // Threshold = 2, so after 2 consecutive failures the circuit opens.
        let p1 = MockProvider::with_results(
            "model-a",
            4096,
            vec![
                provider_err("fail-1"),
                provider_err("fail-2"),
                // This third call should never happen — circuit is open.
                ok_result("model-a"),
            ],
        );
        let p2 = MockProvider::succeeding("model-b", 4096);
        let failover = FailoverProvider::new(
            vec![Box::new(p1), Box::new(p2)],
            vec!["primary".to_owned(), "secondary".to_owned()],
        )
        .with_failure_threshold(2)
        .with_cooldown(Duration::from_secs(300));

        let messages = vec![Message::user_text("hello")];

        // First call: p1 fails, p2 succeeds. p1 has 1 failure.
        let r1 = failover.complete("system", &messages, &[]).await;
        assert!(r1.is_ok());

        // Second call: p1 fails again, circuit trips (2 failures >= threshold 2). p2 succeeds.
        let r2 = failover.complete("system", &messages, &[]).await;
        assert!(r2.is_ok());

        // Third call: p1 should be skipped (circuit open). p2 succeeds directly.
        let r3 = failover.complete("system", &messages, &[]).await;
        assert!(r3.is_ok());
        assert_eq!(failover.model_name(), "model-b");
    }

    #[tokio::test]
    async fn circuit_breaker_recovers_after_cooldown() {
        let p1 = MockProvider::with_results(
            "model-a",
            4096,
            vec![
                provider_err("fail-1"),
                provider_err("fail-2"),
                // After cooldown, this should be tried (half-open probe).
                ok_result("model-a"),
            ],
        );
        let p2 = MockProvider::succeeding("model-b", 4096);
        let failover = FailoverProvider::new(
            vec![Box::new(p1), Box::new(p2)],
            vec!["primary".to_owned(), "secondary".to_owned()],
        )
        .with_failure_threshold(2)
        .with_cooldown(Duration::from_millis(50)); // Very short for testing.

        let messages = vec![Message::user_text("hello")];

        // First two calls trip the circuit on p1.
        let _ = failover.complete("system", &messages, &[]).await;
        let _ = failover.complete("system", &messages, &[]).await;

        // Wait for cooldown to expire.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now p1 should be tried again (half-open). It returns ok_result.
        let r = failover.complete("system", &messages, &[]).await;
        assert!(r.is_ok());
        assert_eq!(failover.model_name(), "model-a");
    }

    #[tokio::test]
    async fn max_output_tokens_returns_minimum() {
        let p1 = MockProvider::succeeding("model-a", 4096);
        let p2 = MockProvider::succeeding("model-b", 2048);
        let p3 = MockProvider::succeeding("model-c", 8192);
        let failover = FailoverProvider::new(
            vec![Box::new(p1), Box::new(p2), Box::new(p3)],
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        );

        assert_eq!(failover.max_output_tokens(), 2048);
    }

    #[tokio::test]
    async fn model_name_tracks_last_success() {
        let p1 = MockProvider::with_results(
            "model-a",
            4096,
            vec![provider_err("down"), ok_result("model-a")],
        );
        let p2 = MockProvider::with_results("model-b", 4096, vec![ok_result("model-b")]);
        let failover = FailoverProvider::new(
            vec![Box::new(p1), Box::new(p2)],
            vec!["primary".to_owned(), "secondary".to_owned()],
        );

        let messages = vec![Message::user_text("hello")];

        // Initially model_name returns first provider's name (idx 0).
        assert_eq!(failover.model_name(), "model-a");

        // First call: p1 fails, p2 succeeds → last_success_idx = 1.
        let _ = failover.complete("system", &messages, &[]).await;
        assert_eq!(failover.model_name(), "model-b");

        // Second call: p1 succeeds → last_success_idx = 0.
        let _ = failover.complete("system", &messages, &[]).await;
        assert_eq!(failover.model_name(), "model-a");
    }

    #[tokio::test]
    async fn success_resets_failure_count() {
        let p1 = MockProvider::with_results(
            "model-a",
            4096,
            vec![
                provider_err("fail-1"),
                // After one success from p2, p1's failure count should reset... wait, no.
                // p1's failure count is only reset when p1 itself succeeds.
                // After p1 fails once, p2 succeeds. p1 still has 1 failure.
                // Next call: p1 succeeds. Should reset its counter.
                ok_result("model-a"),
                // Now fail again — should need full threshold to trip.
                provider_err("fail-2"),
            ],
        );
        let p2 = MockProvider::succeeding("model-b", 4096);
        let failover = FailoverProvider::new(
            vec![Box::new(p1), Box::new(p2)],
            vec!["primary".to_owned(), "secondary".to_owned()],
        )
        .with_failure_threshold(2);

        let messages = vec![Message::user_text("hello")];

        // Call 1: p1 fails (count=1), p2 succeeds.
        let _ = failover.complete("system", &messages, &[]).await;

        // Call 2: p1 succeeds (count resets to 0).
        let r = failover.complete("system", &messages, &[]).await;
        assert!(r.is_ok());
        assert_eq!(failover.model_name(), "model-a");

        // Verify circuit is not tripped: p1 had 1 failure then 1 success,
        // so count is 0. Now p1 fails again (count=1), still below threshold (2).
        let _ = failover.complete("system", &messages, &[]).await;
        // p1 failed, p2 succeeded. But circuit should NOT be open (only 1 failure).
        {
            let circuits = failover.circuits.lock().unwrap();
            assert!(
                circuits[0].opened_at.is_none(),
                "circuit should not be open after just 1 failure post-reset"
            );
        }
    }
}
