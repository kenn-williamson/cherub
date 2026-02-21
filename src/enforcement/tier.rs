/// Capability tiers ordered by privilege level.
/// Variant order defines the `Ord` derivation: Observe < Act < Commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    Observe,
    Act,
    Commit,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Observe => "observe",
            Tier::Act => "act",
            Tier::Commit => "commit",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_ordering() {
        assert!(Tier::Observe < Tier::Act);
        assert!(Tier::Act < Tier::Commit);
        assert!(Tier::Observe < Tier::Commit);
    }
}
