//! Where a memory came from — the trust axis the learning pipeline routes on.
//!
//! Auto-learned (`Inferred`) facts are the prompt-injection blast radius, so they
//! carry lower priority and NEVER silently enter the always-on frozen core
//! (core promotion always needs confirmation — see `learning::route`).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceKind {
    /// The user typed it explicitly (`/remember`, a direct correction). Highest trust.
    UserExplicit,
    /// Inferred from conversation by the free extractor. Lower trust.
    Inferred,
    /// Imported from an external store (`aizen memory import`). Medium trust.
    Imported,
    /// Hand-authored via `aizen memory add`. High trust (a human wrote the file).
    Manual,
}

impl ProvenanceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProvenanceKind::UserExplicit => "user-explicit",
            ProvenanceKind::Inferred => "inferred",
            ProvenanceKind::Imported => "imported",
            ProvenanceKind::Manual => "manual",
        }
    }

    /// Unknown / missing → `inferred` (the conservative, lowest-trust default).
    pub fn parse(s: &str) -> ProvenanceKind {
        match s.trim().to_lowercase().as_str() {
            "user-explicit" | "user_explicit" | "explicit" => ProvenanceKind::UserExplicit,
            "imported" | "import" => ProvenanceKind::Imported,
            "manual" => ProvenanceKind::Manual,
            _ => ProvenanceKind::Inferred,
        }
    }

    /// Trust priority (higher wins on conflict). Used by consolidation/supersession.
    pub fn priority(self) -> u8 {
        match self {
            ProvenanceKind::Manual => 4,
            ProvenanceKind::UserExplicit => 3,
            ProvenanceKind::Imported => 2,
            ProvenanceKind::Inferred => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_falls_back_to_inferred() {
        assert_eq!(
            ProvenanceKind::parse("user-explicit"),
            ProvenanceKind::UserExplicit
        );
        assert_eq!(ProvenanceKind::parse("nonsense"), ProvenanceKind::Inferred);
        assert_eq!(ProvenanceKind::parse(""), ProvenanceKind::Inferred);
    }

    #[test]
    fn manual_outranks_inferred() {
        assert!(ProvenanceKind::Manual.priority() > ProvenanceKind::Inferred.priority());
        assert!(ProvenanceKind::UserExplicit.priority() > ProvenanceKind::Inferred.priority());
    }
}
