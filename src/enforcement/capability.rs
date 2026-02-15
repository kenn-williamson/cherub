use super::tier::Tier;

/// Unforgeable capability token. Proof that the enforcement layer has evaluated
/// and approved an action at a specific tier.
///
/// Construction is double-locked:
/// 1. `Seal` is a private type — prevents struct literal construction from outside this file.
/// 2. `new()` is `pub(super)` — only `enforcement/` submodules can call it.
///
/// No `Clone`, `Copy`, `Default`, or `From` — token is consumed on use (move semantics).
pub struct CapabilityToken {
    pub(crate) tier: Tier,
    _seal: Seal,
}

struct Seal;

impl CapabilityToken {
    pub(super) fn new(tier: Tier) -> Self {
        Self { tier, _seal: Seal }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_token_carries_tier() {
        let token = CapabilityToken::new(Tier::Observe);
        assert_eq!(token.tier, Tier::Observe);

        let token = CapabilityToken::new(Tier::Act);
        assert_eq!(token.tier, Tier::Act);

        let token = CapabilityToken::new(Tier::Commit);
        assert_eq!(token.tier, Tier::Commit);
    }

    #[test]
    fn capability_token_is_consumed() {
        let token = CapabilityToken::new(Tier::Observe);
        // Move token into a function — if CapabilityToken were Copy/Clone,
        // this test would still compile after using `token` again below.
        let _moved = consume(token);
        // Uncommenting the next line would fail to compile — token has been moved.
        // let _ = token.tier;
    }

    fn consume(token: CapabilityToken) -> Tier {
        token.tier
    }
}
