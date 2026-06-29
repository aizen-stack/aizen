//! Personas — a character the agent role-plays, plus the engine that lets it *evolve*.
//!
//! Layered identity model (each a distinct prompt block, ordered for prefix-cache stability):
//! - **persona** (`<persona>`) = *who the agent IS* (name, role, voice, values) — this file.
//! - **self** (`<self>`) = *who the agent has BECOME* — persona-scoped, importance-ranked
//!   self-memories (episodes + reflected insights) — `self_mem` + `reflect`.
//! - **user_memory** (frozen core) = *who the USER is*.
//! - **skills** = *how to do things*.
//!
//! A persona is a human-editable markdown "character card" under `~/.nextgen/personas/<name>.md`:
//! ```text
//! ---
//! name: Aria
//! role: a sharp senior-engineer mentor
//! voice: concise, warm, a little sardonic
//! ---
//! Backstory, values, how you speak, boundaries…
//! ```
//! The ACTIVE persona (config `persona = "<name>"`) is rendered into a `<persona>` block, and its
//! accumulated `<self>` experience deepens across sessions (Generative-Agents pattern: a memory
//! stream of episodes → periodic reflection into higher-level insights). Self-memory lives in a
//! sibling `~/.nextgen/personas/<slug>.self/` directory, so each character grows independently.

pub mod reflect;
pub mod self_mem;
pub mod soul;

use crate::core::config::nextgen_home;
use crate::memory::frontmatter;
use crate::skills::sanitize_name; // shared slug rule
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Token budget for the injected `<self>` block (kept small — it sits alongside persona +
/// user_memory + skills, and must not crowd the prefix).
pub const SELF_BLOCK_MAX_TOKENS: usize = 700;

#[derive(Debug, Clone, PartialEq)]
pub struct Persona {
    pub name: String,
    pub role: String,
    pub voice: String,
    pub body: String,
}

/// `~/.nextgen/personas/` — the personal (HOME) persona dir; `ng persona new` writes here.
pub fn personas_dir() -> PathBuf {
    nextgen_home().join("personas")
}

/// `<repo-root>/.nextgen/personas/` — personas a cloned repo ships, merged OVER HOME (project wins).
pub fn project_personas_dir() -> PathBuf {
    crate::core::config::project_nextgen_dir().join("personas")
}

fn path_for(name: &str) -> PathBuf {
    personas_dir().join(format!("{}.md", sanitize_name(name)))
}

fn parse(content: &str, fallback_name: &str) -> Persona {
    let fm = frontmatter::parse(content);
    Persona {
        name: fm.get("name").unwrap_or(fallback_name).to_string(),
        role: fm.get("role").unwrap_or("").to_string(),
        voice: fm.get("voice").unwrap_or("").to_string(),
        body: fm.body,
    }
}

fn read_dir_personas(dir: &std::path::Path) -> Vec<Persona> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("persona").to_string();
        if let Ok(content) = std::fs::read_to_string(&p) {
            out.push(parse(&content, &stem));
        }
    }
    out
}

/// All personas, sorted by name. HOME merged with the repo's project-local personas — a project
/// persona of the same name WINS (missing dirs / unreadable files skipped).
pub fn list() -> Vec<Persona> {
    let mut by_name: BTreeMap<String, Persona> = BTreeMap::new();
    for p in read_dir_personas(&personas_dir()) {
        by_name.insert(sanitize_name(&p.name), p);
    }
    for p in read_dir_personas(&project_personas_dir()) {
        by_name.insert(sanitize_name(&p.name), p);
    }
    let mut out: Vec<Persona> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Load one persona by name (normalized). Project-local wins over HOME.
pub fn load(name: &str) -> Option<Persona> {
    let file = format!("{}.md", sanitize_name(name));
    for dir in [project_personas_dir(), personas_dir()] {
        let p = dir.join(&file);
        if let Ok(content) = std::fs::read_to_string(&p) {
            let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or(name).to_string();
            return Some(parse(&content, &stem));
        }
    }
    None
}

/// Create or overwrite a persona; returns the file path.
pub fn save(name: &str, role: &str, voice: &str, body: &str) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        bail!("a persona name is required");
    }
    if body.trim().is_empty() {
        bail!("a persona description (the body) is required");
    }
    let dir = personas_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut fields = BTreeMap::new();
    fields.insert("name".to_string(), name.to_string());
    if !role.trim().is_empty() {
        fields.insert("role".to_string(), role.trim().to_string());
    }
    if !voice.trim().is_empty() {
        fields.insert("voice".to_string(), voice.trim().to_string());
    }
    let text = frontmatter::serialize(&fields, body, &["name", "role", "voice"]);
    let path = path_for(name);
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Delete a persona by name (and its self-memory directory). `Ok(true)` if a file was removed.
pub fn delete(name: &str) -> Result<bool> {
    let p = path_for(name);
    let removed_card = if p.exists() {
        std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
        true
    } else {
        false
    };
    // The character is gone → its accumulated experience goes with it.
    let _ = std::fs::remove_dir_all(self_mem::self_dir(&sanitize_name(name)));
    Ok(removed_card)
}

/// The currently-active persona (config `persona = "<name>"`), if set + loadable.
pub fn active() -> Option<Persona> {
    let name = crate::core::cli_config::load().persona?;
    load(&name)
}

/// Set the active persona in config (must already exist). Stores its canonical display name so the
/// HUD/menu show it cleanly. Errors if no such persona — never points the config at a ghost.
pub fn set_active(name: &str) -> Result<()> {
    let p = load(name).with_context(|| format!("no persona named '{name}'"))?;
    let mut cfg = crate::core::cli_config::load();
    cfg.persona = Some(p.name);
    crate::core::cli_config::save(&cfg)
}

/// The filename slug of the active persona (the key for its `<slug>.self/` store). `None` when no
/// persona is active.
pub fn active_slug() -> Option<String> {
    active().map(|p| sanitize_name(&p.name))
}

/// The `<persona>` block for the system prompt (rendered from the active persona). `None` when no
/// persona is active → the block is absent and the agent behaves as the default assistant.
pub fn prompt_block() -> Option<String> {
    let p = active()?;
    let mut s = String::new();
    if p.role.trim().is_empty() {
        s.push_str(&format!("You are {}.\n", p.name));
    } else {
        s.push_str(&format!("You are {}, {}.\n", p.name, p.role));
    }
    if !p.voice.trim().is_empty() {
        s.push_str(&format!("Voice: {}.\n", p.voice));
    }
    if !p.body.trim().is_empty() {
        s.push('\n');
        s.push_str(p.body.trim());
    }
    s.push_str(&format!(
        "\n\nStay in character as {} — keep this voice and perspective — while remaining genuinely \
         helpful, accurate, and honest. Never fabricate facts to stay in character.",
        p.name
    ));
    Some(s)
}

/// The `<self>` block (inner) for the system prompt: the active persona's accumulated experience
/// (reflected insights + recent episodes, importance-ranked). `None` when no persona is active or
/// the character has no self-memory yet.
pub fn self_block() -> Option<String> {
    let slug = active_slug()?;
    self_mem::self_block(&slug, SELF_BLOCK_MAX_TOKENS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-persona-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::env::set_var("NG_PROJECT_ROOT", &dir); // isolate project-local discovery from the real repo
        let out = f();
        std::env::remove_var("NEXTGEN_HOME");
        std::env::remove_var("NG_PROJECT_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn save_load_round_trip() {
        with_home("rt", || {
            save("Aria", "a sharp mentor", "concise, warm", "You value clarity.").unwrap();
            let p = load("aria").expect("loads by normalized name");
            assert_eq!(p.name, "Aria");
            assert_eq!(p.role, "a sharp mentor");
            assert_eq!(p.voice, "concise, warm");
            assert_eq!(p.body, "You value clarity.");
        });
    }

    #[test]
    fn prompt_block_none_until_active_then_renders() {
        with_home("active", || {
            assert!(prompt_block().is_none(), "no persona active");
            save("Aria", "a mentor", "warm", "Backstory here.").unwrap();
            let mut cfg = crate::core::cli_config::load();
            cfg.persona = Some("Aria".to_string());
            crate::core::cli_config::save(&cfg).unwrap();
            let block = prompt_block().expect("active persona renders");
            assert!(block.contains("You are Aria, a mentor."));
            assert!(block.contains("Backstory here."));
            assert!(block.contains("Stay in character"));
        });
    }

    #[test]
    fn save_rejects_empty_body() {
        with_home("empty", || {
            assert!(save("X", "r", "v", "   ").is_err());
        });
    }

    #[test]
    fn delete_also_removes_self_memory() {
        with_home("del-self", || {
            save("Aria", "m", "v", "body").unwrap();
            let slug = sanitize_name("Aria");
            self_mem::record_episode(&slug, "did a thing", 5).unwrap();
            assert!(!self_mem::list(&slug).is_empty());
            delete("Aria").unwrap();
            assert!(self_mem::list(&slug).is_empty(), "self-memory removed with the persona");
        });
    }

    #[test]
    fn self_block_none_until_active_persona_has_memory() {
        with_home("selfblk", || {
            assert!(self_block().is_none(), "no persona, no self block");
            save("Aria", "m", "v", "body").unwrap();
            let mut cfg = crate::core::cli_config::load();
            cfg.persona = Some("Aria".to_string());
            crate::core::cli_config::save(&cfg).unwrap();
            assert!(self_block().is_none(), "active but no experience yet");
            self_mem::record_episode(&sanitize_name("Aria"), "shipped the redesign", 6).unwrap();
            let block = self_block().expect("self block renders once there is experience");
            assert!(block.contains("shipped the redesign"));
        });
    }
}
