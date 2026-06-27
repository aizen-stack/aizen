//! Skills — reusable, named procedures the agent can load on demand.
//!
//! A **skill** is a saved step-by-step playbook (deploy the VPS, triage logs, cut a release);
//! **memory** is facts/preferences about the user/project. They are distinct: a skill is *how to
//! do a recurring task*, memory is *what is true*. Skills live as human-editable markdown under
//! `~/.nextgen/skills/<name>.md`:
//!
//! ```text
//! ---
//! name: deploy-vps
//! description: Deploy + restart the service over SSH
//! when: asked to deploy / ship / restart the production service
//! requires: shell_run            # optional — hidden unless every listed tool is in the live surface
//! platforms: linux, macos        # optional — hidden unless the current OS is listed (unix/posix ok)
//! ---
//! 1. ssh deploy@host
//! 2. cd /srv/app && git pull
//! 3. systemctl restart app && systemctl status app
//! ```
//!
//! A compact index of `name: when` is injected into the system prompt (`<skills>`); the model
//! pulls the full body on demand via the `skill_load` tool, and may persist a new one via
//! `skill_save`. Same anti-bloat posture as memory: the prompt carries only the index, not bodies.

use crate::config::nextgen_home;
use crate::memory::frontmatter;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One saved procedure.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// When the skill applies (the trigger hint shown in the prompt index). May be empty.
    pub when: String,
    /// Optional `requires:` frontmatter — tool names this skill needs. When any is absent from the
    /// live tool surface, the skill is hidden from the index (a skill that drives `browser_*` is
    /// noise when browser tools aren't compiled in). Empty = no requirement.
    pub requires: Vec<String>,
    /// Optional `platforms:` frontmatter — OS names (`windows`/`macos`/`linux`, or `unix`/`posix`).
    /// When non-empty and the current OS isn't listed, the skill is hidden. Empty = all platforms.
    pub platforms: Vec<String>,
    pub body: String,
}

/// Split a frontmatter list value (`a, b c; d`) into trimmed, non-empty tokens.
fn parse_list(s: &str) -> Vec<String> {
    s.split([',', ' ', '\t', ';', '\n'])
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect()
}

/// `~/.nextgen/skills/` — the personal (HOME) skill dir; `skill_save`/`ng skill add` write here.
pub fn skills_dir() -> PathBuf {
    nextgen_home().join("skills")
}

/// `<repo-root>/.nextgen/skills/` — skills a cloned repo ships, merged OVER the HOME ones (project
/// wins on a same-name collision). Repo-root-aware (R4).
pub fn project_skills_dir() -> PathBuf {
    crate::config::project_nextgen_dir().join("skills")
}

/// File-safe slug for a skill name (lowercase alnum + `-`/`_`; collapses the rest to `-`).
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let s = s.trim_matches(|c| c == '-' || c == '_').to_string();
    if s.is_empty() { "skill".to_string() } else { s }
}

fn path_for(name: &str) -> PathBuf {
    skills_dir().join(format!("{}.md", sanitize_name(name)))
}

/// Parse a skill's markdown (frontmatter + body). `fallback_name` is used if there's no `name:`
/// field (e.g. the file stem, or a URL filename when fetching). Public so the fetch path can reuse it.
pub fn parse_markdown(content: &str, fallback_name: &str) -> Skill {
    let fm = frontmatter::parse(content);
    Skill {
        name: fm.get("name").unwrap_or(fallback_name).to_string(),
        description: fm.get("description").unwrap_or("").to_string(),
        when: fm.get("when").unwrap_or("").to_string(),
        requires: fm.get("requires").map(parse_list).unwrap_or_default(),
        platforms: fm.get("platforms").map(parse_list).unwrap_or_default(),
        body: fm.body,
    }
}

/// Is this skill applicable on the current OS? Empty `platforms` = yes. `unix`/`posix` match any
/// non-Windows OS.
fn os_matches(platforms: &[String]) -> bool {
    if platforms.is_empty() {
        return true;
    }
    let os = std::env::consts::OS; // "windows" | "macos" | "linux" | …
    platforms.iter().any(|p| {
        let p = p.to_ascii_lowercase();
        p == os || ((p == "unix" || p == "posix") && os != "windows")
    })
}

/// Are all the skill's `requires:` tools present in the live tool surface? Empty = yes. When the
/// surface hasn't been published (tests / offline `ng skill`), don't filter (`None` → show).
fn requires_satisfied(requires: &[String]) -> bool {
    if requires.is_empty() {
        return true;
    }
    match crate::agent::builtin::active_tool_names() {
        Some(active) => requires.iter().all(|t| active.contains(t)),
        None => true,
    }
}

/// Whether a skill is applicable RIGHT NOW (passes both the platform and required-tool gates).
fn applicable(sk: &Skill) -> bool {
    os_matches(&sk.platforms) && requires_satisfied(&sk.requires)
}

/// Read every `*.md` skill in one dir (missing/unreadable → empty, never errors).
fn read_dir_skills(dir: &std::path::Path) -> Vec<Skill> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("md") {
            continue;
        }
        let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or("skill").to_string();
        if let Ok(content) = std::fs::read_to_string(&p) {
            out.push(parse_markdown(&content, &stem));
        }
    }
    out
}

/// All skills, sorted by name. HOME skills merged with the repo's project-local ones — a project
/// skill of the same (slug) name WINS, so a cloned repo can override a personal skill. Never errors.
pub fn list() -> Vec<Skill> {
    let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
    // HOME first, then project → project overwrites on a same-slug collision.
    for sk in read_dir_skills(&skills_dir()) {
        by_name.insert(sanitize_name(&sk.name), sk);
    }
    for sk in read_dir_skills(&project_skills_dir()) {
        by_name.insert(sanitize_name(&sk.name), sk);
    }
    let mut out: Vec<Skill> = by_name.into_values().collect();
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Load one skill by name (exact, after slug normalization). Project-local wins over HOME. `None`
/// if absent in both.
pub fn load(name: &str) -> Option<Skill> {
    let file = format!("{}.md", sanitize_name(name));
    for dir in [project_skills_dir(), skills_dir()] {
        let p = dir.join(&file);
        if let Ok(content) = std::fs::read_to_string(&p) {
            let stem = p.file_stem().and_then(|x| x.to_str()).unwrap_or(name).to_string();
            return Some(parse_markdown(&content, &stem));
        }
    }
    None
}

/// Whether any skill exists in HOME or the project (gates `skill_load` + the `<skills>` block).
pub fn has_any() -> bool {
    let any_md = |dir: PathBuf| {
        std::fs::read_dir(dir)
            .map(|rd| rd.flatten().any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md")))
            .unwrap_or(false)
    };
    any_md(skills_dir()) || any_md(project_skills_dir())
}

/// Create or overwrite a skill. Returns the file path written.
pub fn save(name: &str, description: &str, when: &str, body: &str) -> Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        bail!("a skill name is required");
    }
    if body.trim().is_empty() {
        bail!("a skill body (the steps) is required");
    }
    let dir = skills_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut fields = BTreeMap::new();
    fields.insert("name".to_string(), name.to_string());
    if !description.trim().is_empty() {
        fields.insert("description".to_string(), description.trim().to_string());
    }
    if !when.trim().is_empty() {
        fields.insert("when".to_string(), when.trim().to_string());
    }
    let text = frontmatter::serialize(&fields, body, &["name", "description", "when"]);
    let path = path_for(name);
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Delete a skill by name. `Ok(true)` if a file was removed, `Ok(false)` if none existed.
pub fn delete(name: &str) -> Result<bool> {
    let p = path_for(name);
    if !p.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
    Ok(true)
}

/// The compact `<skills>` block for the system prompt: one `name: when/description` line per
/// skill, telling the model to `skill_load` the matching one. `None` when there are no skills
/// (so the block — and the byte-stable prefix — is simply absent).
pub fn prompt_index() -> Option<String> {
    // Hide skills that don't apply right now (wrong OS, or a required tool isn't in the live
    // surface) — a skill referencing absent tools is pure index noise the model can't act on.
    let skills: Vec<Skill> = list().into_iter().filter(applicable).collect();
    if skills.is_empty() {
        return None;
    }
    let mut s = String::from(
        "Saved procedures. When a task matches one, call skill_load(\"<name>\") to get its steps, then follow them:\n",
    );
    for sk in &skills {
        let hint = if !sk.when.is_empty() {
            sk.when.as_str()
        } else if !sk.description.is_empty() {
            sk.description.as_str()
        } else {
            "(no description)"
        };
        // keep each line short — the index is always-on, bodies are loaded on demand.
        let hint: String = hint.chars().take(120).collect();
        s.push_str(&format!("- {}: {}\n", sk.name, hint.replace('\n', " ")));
    }
    Some(s.trim_end().to_string())
}

/// Render a loaded skill for the `skill_load` tool result (header + steps).
pub fn render_loaded(sk: &Skill) -> String {
    let mut s = format!("# skill: {}", sk.name);
    if !sk.description.is_empty() {
        s.push_str(&format!(" — {}", sk.description));
    }
    if !sk.when.is_empty() {
        s.push_str(&format!("\n(when: {})", sk.when));
    }
    s.push_str("\n\n");
    s.push_str(sk.body.trim());
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = crate::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-skill-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);
        // Pin the project root into the same isolated temp dir so project-local skill discovery
        // doesn't pick up the real repo's `.nextgen/skills/` and skew these HOME-only assertions.
        std::env::set_var("NG_PROJECT_ROOT", &dir);
        let out = f();
        std::env::remove_var("NEXTGEN_HOME");
        std::env::remove_var("NG_PROJECT_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn sanitize_makes_file_safe_slugs() {
        assert_eq!(sanitize_name("Deploy VPS!"), "deploy-vps");
        assert_eq!(sanitize_name("  release/cut  "), "release-cut");
        assert_eq!(sanitize_name("___"), "skill");
        assert_eq!(sanitize_name("keep_under-score"), "keep_under-score");
    }

    #[test]
    fn save_load_round_trip() {
        with_home("rt", || {
            let p = save("Deploy VPS", "ship the service", "asked to deploy", "1. ssh\n2. restart").unwrap();
            assert!(p.exists());
            let sk = load("deploy vps").expect("loads by normalized name");
            assert_eq!(sk.name, "Deploy VPS");
            assert_eq!(sk.description, "ship the service");
            assert_eq!(sk.when, "asked to deploy");
            assert_eq!(sk.body, "1. ssh\n2. restart");
        });
    }

    #[test]
    fn list_sorts_and_has_any_tracks() {
        with_home("list", || {
            assert!(!has_any());
            assert!(list().is_empty());
            save("zebra", "z", "", "do z").unwrap();
            save("alpha", "a", "", "do a").unwrap();
            assert!(has_any());
            let names: Vec<String> = list().into_iter().map(|s| s.name).collect();
            assert_eq!(names, vec!["alpha", "zebra"]);
        });
    }

    #[test]
    fn project_skills_merge_over_home_and_win() {
        with_home("proj", || {
            save("deploy", "home version", "", "home steps").unwrap(); // HOME skill
            let pdir = project_skills_dir();
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(pdir.join("deploy.md"), "---\nname: deploy\ndescription: project version\n---\nproject steps").unwrap();
            std::fs::write(pdir.join("lint.md"), "---\nname: lint\n---\nrun clippy").unwrap();
            let names: Vec<String> = list().into_iter().map(|s| s.name).collect();
            assert!(names.contains(&"lint".to_string()), "project-only skill shows in the merged list");
            assert_eq!(names.iter().filter(|n| *n == "deploy").count(), 1, "no duplicate on collision");
            assert_eq!(load("deploy").unwrap().description, "project version", "project skill wins over HOME");
            assert!(has_any());
        });
    }

    #[test]
    fn delete_removes() {
        with_home("del", || {
            save("temp", "t", "", "x").unwrap();
            assert!(delete("temp").unwrap());
            assert!(!delete("temp").unwrap()); // already gone
            assert!(load("temp").is_none());
        });
    }

    #[test]
    fn prompt_index_lists_when_present_else_none() {
        with_home("idx", || {
            assert!(prompt_index().is_none());
            save("deploy", "ship it", "asked to deploy", "steps").unwrap();
            let idx = prompt_index().unwrap();
            assert!(idx.contains("deploy: asked to deploy"));
            assert!(idx.contains("skill_load"));
        });
    }

    #[test]
    fn save_rejects_empty_body() {
        with_home("empty", || {
            assert!(save("x", "d", "w", "   ").is_err());
        });
    }

    #[test]
    fn parse_list_splits_on_commas_space_semicolon() {
        assert_eq!(parse_list("a, b  c;d"), vec!["a", "b", "c", "d"]);
        assert!(parse_list("   \n  ").is_empty());
    }

    #[test]
    fn os_matches_empty_current_and_unix_alias() {
        assert!(os_matches(&[]), "no constraint matches everything");
        assert!(os_matches(&[std::env::consts::OS.to_string()]), "the current OS matches");
        let foreign = if std::env::consts::OS == "windows" { "linux" } else { "windows" };
        assert!(!os_matches(&[foreign.to_string()]), "a foreign-only-OS skill is hidden");
        assert_eq!(
            os_matches(&["unix".to_string()]),
            std::env::consts::OS != "windows",
            "unix alias matches any non-Windows OS"
        );
    }

    #[test]
    fn requires_satisfied_short_circuits_on_empty() {
        // Empty requires never filters, regardless of whether a tool surface was published.
        assert!(requires_satisfied(&[]));
    }

    #[test]
    fn prompt_index_hides_foreign_platform_skill() {
        with_home("plat", || {
            let foreign = if std::env::consts::OS == "windows" { "linux" } else { "windows" };
            let dir = skills_dir();
            std::fs::create_dir_all(&dir).unwrap();
            // a skill pinned to a foreign OS (no `requires:`, so the platform gate is what's tested)
            std::fs::write(
                dir.join("foreign.md"),
                format!("---\nname: foreign\nwhen: never here\nplatforms: {foreign}\n---\nsteps"),
            )
            .unwrap();
            assert!(prompt_index().is_none(), "only skill is foreign-OS → no index");
            // a current-OS skill shows; the foreign one stays hidden
            save("local", "", "always", "do it").unwrap();
            let idx = prompt_index().unwrap();
            assert!(idx.contains("local"));
            assert!(!idx.contains("foreign"), "foreign-OS skill stays hidden");
        });
    }
}
