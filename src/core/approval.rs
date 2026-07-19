//! Unified approval policy shared by the interactive REPL, one-shot runners, sub-agents, and bots.
//!
//! The three levels form one ordered capability ladder:
//! - `ask`: prompt before destructive tools;
//! - `smart`: auto-run shell commands the hard guard classifies as read-only, prompt for the rest;
//! - `yolo`: pre-authorize destructive tools after the non-overridable hard safety floor.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    #[default]
    Ask,
    Smart,
    Yolo,
}

impl ApprovalMode {
    /// Whether a shell command classified as read-only may skip the approval prompt.
    pub fn approves_readonly_shell(self) -> bool {
        matches!(self, Self::Smart | Self::Yolo)
    }

    /// Whether every destructive tool may skip the approval prompt (the hard floor still runs first).
    pub fn approves_all(self) -> bool {
        self == Self::Yolo
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Smart => "smart",
            Self::Yolo => "yolo",
        }
    }
}

impl fmt::Display for ApprovalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ApprovalMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" | "manual" | "prompt" => Ok(Self::Ask),
            "smart" => Ok(Self::Smart),
            "yolo" | "auto" | "yes" => Ok(Self::Yolo),
            other => Err(format!(
                "unknown approval mode '{other}' — use ask, smart, or yolo"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases_and_displays_canonical_names() {
        assert_eq!("ask".parse(), Ok(ApprovalMode::Ask));
        assert_eq!("manual".parse(), Ok(ApprovalMode::Ask));
        assert_eq!("smart".parse(), Ok(ApprovalMode::Smart));
        assert_eq!("yes".parse(), Ok(ApprovalMode::Yolo));
        assert_eq!(ApprovalMode::Yolo.to_string(), "yolo");
        assert!("unsafe".parse::<ApprovalMode>().is_err());
    }

    #[test]
    fn capability_ladder_is_monotonic() {
        assert!(!ApprovalMode::Ask.approves_readonly_shell());
        assert!(!ApprovalMode::Ask.approves_all());
        assert!(ApprovalMode::Smart.approves_readonly_shell());
        assert!(!ApprovalMode::Smart.approves_all());
        assert!(ApprovalMode::Yolo.approves_readonly_shell());
        assert!(ApprovalMode::Yolo.approves_all());
    }

    #[test]
    fn serde_uses_lowercase_strings() {
        assert_eq!(serde_json::to_string(&ApprovalMode::Smart).unwrap(), "\"smart\"");
        assert_eq!(
            serde_json::from_str::<ApprovalMode>("\"yolo\"").unwrap(),
            ApprovalMode::Yolo
        );
    }
}
