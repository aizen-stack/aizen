//! Where a new fact belongs: the ONE decision point that turns a *proposal*
//! (what the model / write path asked for) into a *choice* (what actually gets written).
//!
//! Replaces `learning::scope_for`, which decided placement from `MemoryType` alone — so
//! "the user prefers Vietnamese" and "this repo pins windows-sys 0.59" were separated by a
//! type tag rather than by where they are true.
//!
//! Both entry points are PURE: filesystem existence arrives as an injected `exists` closure,
//! the current directory / home / device arrive as a [`Lineage`]. That is what makes the
//! clamp rules testable without touching a real disk.
//!
//! ## The rules (plan §Phase 1)
//!
//! | proposal | result |
//! |---|---|
//! | `user` | `User`, no anchor |
//! | `device` | `Device` tagged with this machine's id, no anchor |
//! | `place` + anchor is an ancestor of cwd | accepted (possibly walked up to an existing dir) |
//! | `place` + anchor is NOT an ancestor | clamped to the narrowest project/cwd, confidence ×0.8 |
//! | `place` + anchor at or above home | `Device` if the text is about the machine, else `User` |
//! | cwd itself IS home | same home fallback — **never** anchor the whole home dir |
//! | nothing / nonsense | `place` at the narrowest project/cwd, confidence ×0.7 |
//!
//! A guess that is too NARROW costs a missed recall; a guess that is too BROAD pollutes the
//! always-on prefix for every future directory. Missing is the cheaper failure, so every
//! ambiguous case clamps down, never up.

use crate::memory::path_scope::{is_ancestor, Lineage, Tier};

/// Confidence multiplier for an anchor that had to be clamped (the write path named a place
/// that does not contain the cwd — plausible, but we trust it less).
const CLAMP_PENALTY: f64 = 0.8;
/// Confidence multiplier when nothing usable was proposed and we fell back to the cwd.
const GUESS_PENALTY: f64 = 0.7;

/// What a write path *asked* for. Every field is optional/lenient — this is untrusted input
/// (a model's tool call, a legacy caller), not a decision.
#[derive(Debug, Clone, Default)]
pub struct TierProposal {
    /// The tier the caller named, if it named a valid one.
    pub tier: Option<Tier>,
    /// The anchor the caller named, already normalized (`config::anchor_of`).
    pub anchor: Option<String>,
    /// Does the fact text talk about the machine (toolchain, paths, hardware)? Only consulted
    /// when a `place` proposal collapses to the home dir and we must pick `Device` vs `User`.
    pub mentions_machine: bool,
}

/// The resolved placement. `confidence_mult` is applied by the caller to its own confidence.
#[derive(Debug, Clone, PartialEq)]
pub struct TierChoice {
    pub tier: Tier,
    pub anchor: Option<String>,
    pub device: Option<String>,
    pub confidence_mult: f64,
}

impl TierChoice {
    /// The unambiguous "about the person, applies everywhere" placement. Public because a few
    /// write paths know their fact is a user fact by construction (a denied STYLE rule, a
    /// `#remember` the user tagged themselves) and must not have it re-derived from the cwd.
    pub fn user_tier() -> Self {
        Self::user()
    }

    fn user() -> Self {
        TierChoice {
            tier: Tier::User,
            anchor: None,
            device: None,
            confidence_mult: 1.0,
        }
    }
    fn device(id: &str) -> Self {
        TierChoice {
            tier: Tier::Device,
            anchor: None,
            device: Some(id.to_string()),
            confidence_mult: 1.0,
        }
    }
    fn place(anchor: String, mult: f64) -> Self {
        TierChoice {
            tier: Tier::Place,
            anchor: Some(anchor),
            device: None,
            confidence_mult: mult,
        }
    }
}

/// Resolve a proposal into the placement that will actually be written.
///
/// `exists` answers "is this normalized path a real directory right now?" — injected so the
/// clamp logic is testable, and so a junction/`subst` path that has not been created yet gets
/// walked up to a directory that HAS been, instead of writing an anchor that can never match.
pub fn decide(p: &TierProposal, lin: &Lineage, exists: &dyn Fn(&str) -> bool) -> TierChoice {
    match p.tier {
        Some(Tier::User) => TierChoice::user(),
        Some(Tier::Device) => TierChoice::device(&lin.device),
        Some(Tier::Place) => decide_place(p, lin, exists, CLAMP_PENALTY),
        // Nothing was said (or it did not parse): guess narrow and mark it as a guess.
        None => decide_place(p, lin, exists, GUESS_PENALTY),
    }
}

/// The `place` branch. `miss_penalty` is the multiplier applied when the named anchor is
/// unusable and we fall back to the cwd — different for "you named a bad place" (0.8) vs
/// "you named no place at all" (0.7).
fn decide_place(
    p: &TierProposal,
    lin: &Lineage,
    exists: &dyn Fn(&str) -> bool,
    miss_penalty: f64,
) -> TierChoice {
    // Standing IN the home dir means there is no project to anchor to: anchoring home would
    // make the fact fire in every directory the user ever visits, i.e. a `user` fact wearing a
    // `place` label. Say so honestly instead.
    if at_or_above_home(&lin.cwd, lin.home.as_deref()) {
        return home_fallback(p, lin);
    }

    let named = p.anchor.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let Some(named) = named else {
        // No anchor proposed → narrowest plausible place, flagged as a guess.
        return TierChoice::place(lin.narrowest_project_or_cwd(), miss_penalty);
    };

    let named = named.to_ascii_lowercase();

    // An anchor at/above home is the same pollution as cwd-at-home, just spelled explicitly.
    if at_or_above_home(&named, lin.home.as_deref()) {
        return home_fallback(p, lin);
    }

    if !is_ancestor(&named, &lin.cwd) {
        // The named place does not contain where we are. It may still be true, but we cannot
        // verify it from here, so clamp to what we CAN verify.
        return TierChoice::place(lin.narrowest_project_or_cwd(), miss_penalty);
    }

    // Accepted. Walk up to a directory that actually exists so the anchor can ever match.
    match nearest_existing(&named, lin, exists) {
        Some(a) => TierChoice::place(a, 1.0),
        None => TierChoice::place(lin.narrowest_project_or_cwd(), miss_penalty),
    }
}

/// `place` collapsed onto the home dir: re-file as `device` when the fact is about the machine,
/// else as `user`. Never returns an anchor.
fn home_fallback(p: &TierProposal, lin: &Lineage) -> TierChoice {
    if p.mentions_machine {
        TierChoice::device(&lin.device)
    } else {
        TierChoice::user()
    }
}

/// Is `path` the home dir itself, or an ancestor of it (drive root, `/Users`, …)?
/// No home resolvable → false, so a headless/CI environment still anchors normally.
fn at_or_above_home(path: &str, home: Option<&str>) -> bool {
    match home {
        Some(h) => is_ancestor(path, h), // path == home, or home lives under path
        None => false,
    }
}

/// The nearest ancestor of `anchor` (starting with `anchor` itself) that exists, stopping
/// before home. `None` when nothing in the chain exists.
fn nearest_existing(anchor: &str, lin: &Lineage, exists: &dyn Fn(&str) -> bool) -> Option<String> {
    let mut cur = anchor.trim_end_matches('/').to_string();
    loop {
        if exists(&cur) {
            return Some(cur);
        }
        if at_or_above_home(&cur, lin.home.as_deref()) {
            return None;
        }
        match cur.rfind('/') {
            Some(0) | None => return None,
            Some(i) => cur.truncate(i),
        }
    }
}

/// Real-filesystem `exists` for production callers.
pub fn fs_exists(path: &str) -> bool {
    std::path::Path::new(path).is_dir()
}

/// Cheap, allocation-free hint for the `device` vs `user` fork: does this text talk about the
/// machine rather than the person? Deliberately conservative — a false negative files the fact
/// as `user` (still always-on, just not machine-scoped), a false positive would hide it on
/// every other machine.
pub fn mentions_machine(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "this machine",
        "this device",
        "this computer",
        "this laptop",
        "this pc",
        "máy này",
        "máy tính này",
        "installed at",
        "is installed",
        "not installed",
        "on my machine",
        "toolchain",
        "gpu",
        "cpu",
        "ram",
        "windows",
        "linux",
        "macos",
        "wsl",
        "path environment",
        "%localappdata%",
        "program files",
        "/usr/bin",
        "/usr/local",
    ];
    let low = text.to_lowercase();
    MARKERS.iter().any(|m| low.contains(m))
}

/// Build the proposal a plain `MemoryType`-only caller implies, so legacy write paths keep
/// working while they migrate to naming a tier explicitly.
///
/// `user`/`feedback` are about the person → `User`. `project`/`reference` are about the work in
/// front of us → `Place`, anchored where we stand.
pub fn proposal_from_mtype(
    mtype: crate::memory::store::MemoryType,
    body: &str,
    lin: &Lineage,
) -> TierProposal {
    use crate::memory::store::MemoryType;
    match mtype {
        MemoryType::User | MemoryType::Feedback => TierProposal {
            tier: Some(Tier::User),
            anchor: None,
            mentions_machine: false,
        },
        MemoryType::Project | MemoryType::Reference => TierProposal {
            tier: Some(Tier::Place),
            anchor: Some(lin.narrowest_project_or_cwd()),
            mentions_machine: mentions_machine(body),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lineage rooted under a fake home, with no real filesystem behind it.
    fn lin(cwd: &str) -> Lineage {
        Lineage {
            cwd: cwd.to_string(),
            places: vec![cwd.to_string()],
            device: "dev-deadbeef".to_string(),
            home: Some("c:/users/admin".to_string()),
        }
    }

    fn all_exist(_: &str) -> bool {
        true
    }
    fn none_exist(_: &str) -> bool {
        false
    }

    #[test]
    fn user_proposal_never_gets_an_anchor() {
        let c = decide(
            &TierProposal {
                tier: Some(Tier::User),
                ..Default::default()
            },
            &lin("c:/users/admin/proj"),
            &all_exist,
        );
        assert_eq!(c.tier, Tier::User);
        assert_eq!(c.anchor, None);
        assert_eq!(c.device, None);
    }

    #[test]
    fn device_proposal_is_tagged_with_this_machine() {
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Device),
                ..Default::default()
            },
            &lin("c:/users/admin/proj"),
            &all_exist,
        );
        assert_eq!(c.tier, Tier::Device);
        assert_eq!(c.device.as_deref(), Some("dev-deadbeef"));
        assert_eq!(c.anchor, None);
    }

    #[test]
    fn ancestor_anchor_is_accepted_verbatim() {
        let l = lin("c:/users/admin/proj/src/agent");
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("c:/users/admin/proj".into()),
                mentions_machine: false,
            },
            &l,
            &all_exist,
        );
        assert_eq!(c.anchor.as_deref(), Some("c:/users/admin/proj"));
        assert_eq!(c.confidence_mult, 1.0);
    }

    #[test]
    fn clamp_rejects_non_ancestor() {
        let l = lin("c:/users/admin/proj");
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("c:/users/admin/other".into()),
                mentions_machine: false,
            },
            &l,
            &all_exist,
        );
        assert_eq!(c.tier, Tier::Place);
        assert_ne!(c.anchor.as_deref(), Some("c:/users/admin/other"));
        assert!(
            c.confidence_mult < 1.0,
            "a clamped anchor must cost confidence"
        );
    }

    #[test]
    fn clamp_never_goes_above_home() {
        let l = lin("c:/users/admin/proj");
        for above in ["c:/users/admin", "c:/users", "c:/"] {
            let c = decide(
                &TierProposal {
                    tier: Some(Tier::Place),
                    anchor: Some(above.into()),
                    mentions_machine: false,
                },
                &l,
                &all_exist,
            );
            assert_ne!(
                c.tier,
                Tier::Place,
                "{above} must not become a place anchor"
            );
            assert_eq!(c.anchor, None, "{above} must not produce an anchor");
        }
    }

    #[test]
    fn cwd_at_home_yields_user_or_device_never_anchor() {
        let l = lin("c:/users/admin");
        let as_user = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("c:/users/admin".into()),
                mentions_machine: false,
            },
            &l,
            &all_exist,
        );
        assert_eq!(as_user.tier, Tier::User);
        assert_eq!(as_user.anchor, None);

        let as_device = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: None,
                mentions_machine: true,
            },
            &l,
            &all_exist,
        );
        assert_eq!(as_device.tier, Tier::Device);
        assert_eq!(as_device.anchor, None);
    }

    #[test]
    fn nonexistent_path_falls_back_to_existing_ancestor() {
        let l = lin("c:/users/admin/proj/src/agent/lsp");
        // Only `.../proj` exists on this fake disk.
        let exists = |p: &str| p == "c:/users/admin/proj";
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("c:/users/admin/proj/src/agent/lsp".into()),
                mentions_machine: false,
            },
            &l,
            &exists,
        );
        assert_eq!(c.anchor.as_deref(), Some("c:/users/admin/proj"));
    }

    #[test]
    fn nothing_existing_clamps_instead_of_writing_a_dead_anchor() {
        let l = lin("c:/users/admin/proj");
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("c:/users/admin/proj/ghost".into()),
                mentions_machine: false,
            },
            &l,
            &none_exist,
        );
        assert!(c.confidence_mult < 1.0);
        assert_ne!(c.anchor.as_deref(), Some("c:/users/admin/proj/ghost"));
    }

    #[test]
    fn no_proposal_guesses_narrow_and_pays_for_it() {
        let l = lin("c:/users/admin/proj");
        let c = decide(&TierProposal::default(), &l, &all_exist);
        assert_eq!(c.tier, Tier::Place);
        assert!(c.anchor.is_some());
        assert_eq!(c.confidence_mult, GUESS_PENALTY);
    }

    #[test]
    fn anchor_case_is_folded_so_prefix_matching_holds() {
        let l = lin("c:/users/admin/proj/src");
        let c = decide(
            &TierProposal {
                tier: Some(Tier::Place),
                anchor: Some("C:/Users/Admin/Proj".into()),
                mentions_machine: false,
            },
            &l,
            &all_exist,
        );
        assert_eq!(c.anchor.as_deref(), Some("c:/users/admin/proj"));
    }

    #[test]
    fn mentions_machine_catches_both_languages() {
        assert!(mentions_machine("this machine has no gcc"));
        assert!(mentions_machine("máy này không có gcc"));
        assert!(mentions_machine("git is installed at C:/Program Files/Git"));
        assert!(!mentions_machine("prefers concise answers"));
    }

    #[test]
    fn mtype_proposal_routes_person_vs_work() {
        use crate::memory::store::MemoryType;
        let l = lin("c:/users/admin/proj");
        assert_eq!(
            proposal_from_mtype(MemoryType::User, "likes tabs", &l).tier,
            Some(Tier::User)
        );
        assert_eq!(
            proposal_from_mtype(MemoryType::Feedback, "be terser", &l).tier,
            Some(Tier::User)
        );
        assert_eq!(
            proposal_from_mtype(MemoryType::Project, "pins windows-sys", &l).tier,
            Some(Tier::Place)
        );
        assert!(proposal_from_mtype(MemoryType::Reference, "see docs", &l)
            .anchor
            .is_some());
    }
}
