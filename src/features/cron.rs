//! Daemon-free scheduled agent tasks (`aizen cron`). Instead of an always-on daemon + embedded
//! scheduler (sled / tokio-cron-scheduler — which would bloat the binary and not survive reboots),
//! `cron add` registers an OS-scheduler entry (Windows Task Scheduler / Unix crontab) that runs
//! `aizen cron run <name>` in a FRESH process at the schedule. Job specs live as JSON under
//! `~/.aizen/cron/`, with the model PINNED at create time (fails-closed: a renamed/removed model
//! errors rather than silently running on the wrong one). The hard `cmd_guard` floor still protects
//! the unattended run, so a scheduled agent can't be tricked into a catastrophic shell command.

use crate::core::config::aizen_home;
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum CronCmd {
    /// Schedule an agent task. SCHEDULE is one of: `daily@HH:MM`, `hourly`, `<N>m` (every N min),
    /// `<N>h` (every N hours). The current model + endpoint are pinned into the job.
    Add {
        /// A short job name (used for the OS task name + spec file).
        name: String,
        /// When to run: daily@HH:MM | hourly | <N>m | <N>h.
        #[arg(short, long)]
        schedule: String,
        /// The task prompt the agent runs (unattended).
        #[arg(short, long)]
        task: String,
    },
    /// List scheduled jobs.
    List,
    /// Remove a scheduled job (unregisters it from the OS scheduler + deletes its spec).
    Remove {
        /// The job name.
        name: String,
    },
    /// Internal: run a job NOW (this is what the OS scheduler invokes). Loads the spec, runs the
    /// agent unattended, appends output to the job log.
    Run {
        /// The job name.
        name: String,
    },
}

#[derive(Serialize, Deserialize, Debug)]
struct CronJob {
    name: String,
    schedule: String,
    task: String,
    /// Model pinned at create time (fails-closed if it's gone).
    model: Option<String>,
    base_url: Option<String>,
    created: String,
}

fn cron_dir() -> PathBuf {
    aizen_home().join("cron")
}
fn slug(name: &str) -> String {
    crate::skills::sanitize_name(name)
}
fn spec_path(name: &str) -> PathBuf {
    cron_dir().join(format!("{}.json", slug(name)))
}
fn log_path(name: &str) -> PathBuf {
    cron_dir().join(format!("{}.log", slug(name)))
}
/// The OS task name (namespaced so we can find/remove only our own).
fn task_name(name: &str) -> String {
    format!("ng_{}", slug(name))
}

pub async fn handle(cmd: CronCmd) -> Result<()> {
    match cmd {
        CronCmd::Add {
            name,
            schedule,
            task,
        } => add(&name, &schedule, &task),
        CronCmd::List => list(),
        CronCmd::Remove { name } => remove(&name),
        CronCmd::Run { name } => run(&name).await,
    }
}

fn add(name: &str, schedule: &str, task: &str) -> Result<()> {
    if slug(name).is_empty() {
        bail!("invalid job name");
    }
    // Validate the schedule up front (so we don't register a half-baked OS entry).
    validate_schedule(schedule)?;

    let cfg = crate::core::cli_config::load();
    let exe = std::env::current_exe().context("resolving the aizen executable path")?;
    let exe = exe.display().to_string();

    std::fs::create_dir_all(cron_dir()).context("creating ~/.aizen/cron")?;
    register_os(&task_name(name), schedule, &exe, name)?;

    let job = CronJob {
        name: name.to_string(),
        schedule: schedule.to_string(),
        task: task.to_string(),
        model: cfg.model.clone(),
        base_url: cfg.base_url.clone(),
        created: crate::memory::learning::default_session_id(),
    };
    std::fs::write(spec_path(name), serde_json::to_string_pretty(&job)?)
        .with_context(|| format!("writing spec for '{name}'"))?;

    println!(
        "scheduled '{name}' ({schedule}) → runs: {} cron run {name}\n  model: {}\n  spec:  {}",
        exe,
        job.model
            .as_deref()
            .unwrap_or("(config default at run time)"),
        spec_path(name).display()
    );
    Ok(())
}

fn list() -> Result<()> {
    let dir = cron_dir();
    let mut found = false;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for ent in rd.flatten() {
            if ent.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(s) = std::fs::read_to_string(ent.path()) {
                if let Ok(job) = serde_json::from_str::<CronJob>(&s) {
                    found = true;
                    println!(
                        "{:<20} {:<14} {}",
                        job.name,
                        job.schedule,
                        truncate(&job.task, 60)
                    );
                }
            }
        }
    }
    if !found {
        println!("(no scheduled jobs — add one with `aizen cron add <name> --schedule daily@09:00 --task \"…\"`)");
    }
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    let sp = spec_path(name);
    if !sp.exists() {
        bail!("no job named '{name}'");
    }
    unregister_os(&task_name(name))?;
    let _ = std::fs::remove_file(&sp);
    let _ = std::fs::remove_file(log_path(name));
    println!("removed '{name}'");
    Ok(())
}

async fn run(name: &str) -> Result<()> {
    // A scheduled run has no human at the keyboard: every spawn in this process falls under the
    // sandbox's unattended fail-closed rule (no kernel backend + no opt-in ⇒ refuse, don't degrade).
    crate::sandbox::set_process_unattended();
    let spec = std::fs::read_to_string(spec_path(name))
        .with_context(|| format!("no spec for job '{name}' (was it removed?)"))?;
    let job: CronJob = serde_json::from_str(&spec).context("parsing job spec")?;

    // Pinned model/endpoint win; fall back to current config only when the spec didn't capture one.
    let (base_url, api_key, model) =
        crate::resolve_endpoint(job.base_url.clone(), None, job.model.clone())
            .context("no endpoint configured for the scheduled run")?;
    let http = crate::http_client()?;

    // Unattended → yolo approval (no human to confirm). The hard cmd_guard floor still refuses
    // catastrophic shell commands, so this is bounded, not a blank cheque.
    let result = crate::run_agent_capture(
        &http,
        &base_url,
        &api_key,
        &model,
        &job.task,
        crate::core::approval::ApprovalMode::Yolo,
    )
    .await;

    let stamp = crate::memory::learning::default_session_id();
    let entry = match &result {
        Ok(out) => format!("\n===== {stamp} =====\n{out}\n"),
        Err(e) => format!("\n===== {stamp} (ERROR) =====\n{e}\n"),
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(name))
    {
        use std::io::Write;
        let _ = f.write_all(entry.as_bytes());
    }
    // Echo to stdout too (the scheduler may capture it); surface errors as a non-zero exit.
    print!("{entry}");
    result.map(|_| ())
}

// ── schedule translation ─────────────────────────────────────────────────────

/// Accept `daily@HH:MM`, `hourly`, `<N>m`, `<N>h`. Returns an error describing the grammar on miss.
fn validate_schedule(s: &str) -> Result<()> {
    parse_schedule(s).map(|_| ())
}

enum Sched {
    DailyAt(u8, u8),
    Hourly,
    EveryMinutes(u32),
    EveryHours(u32),
}

fn parse_schedule(s: &str) -> Result<Sched> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("hourly") {
        return Ok(Sched::Hourly);
    }
    if let Some(rest) = s.strip_prefix("daily@") {
        let (h, m) = rest
            .split_once(':')
            .context("daily schedule must be daily@HH:MM")?;
        let h: u8 = h.parse().context("bad hour")?;
        let m: u8 = m.parse().context("bad minute")?;
        if h > 23 || m > 59 {
            bail!("daily@HH:MM out of range");
        }
        return Ok(Sched::DailyAt(h, m));
    }
    if let Some(n) = s.strip_suffix('m') {
        let n: u32 = n.parse().context("bad minute interval")?;
        if n == 0 {
            bail!("interval must be ≥ 1");
        }
        return Ok(Sched::EveryMinutes(n));
    }
    if let Some(n) = s.strip_suffix('h') {
        let n: u32 = n.parse().context("bad hour interval")?;
        if n == 0 {
            bail!("interval must be ≥ 1");
        }
        return Ok(Sched::EveryHours(n));
    }
    bail!("unknown schedule '{s}' — use daily@HH:MM | hourly | <N>m | <N>h")
}

// ── OS scheduler glue ────────────────────────────────────────────────────────

fn register_os(task: &str, schedule: &str, exe: &str, name: &str) -> Result<()> {
    let sched = parse_schedule(schedule)?;
    if cfg!(windows) {
        register_windows(task, &sched, exe, name)
    } else {
        register_unix(task, &sched, exe, name)
    }
}

fn unregister_os(task: &str) -> Result<()> {
    if cfg!(windows) {
        let out = std::process::Command::new("schtasks")
            .args(["/Delete", "/TN", task, "/F"])
            .output()
            .context("running schtasks /Delete")?;
        // Missing task is not a hard error (idempotent remove).
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.to_lowercase().contains("cannot find") {
                bail!("schtasks /Delete failed: {}", err.trim());
            }
        }
        Ok(())
    } else {
        rewrite_crontab(|lines| {
            lines
                .into_iter()
                .filter(|l| !l.contains(&marker(task)))
                .collect()
        })
    }
}

fn register_windows(task: &str, sched: &Sched, exe: &str, name: &str) -> Result<()> {
    // The command schtasks runs. Inner quotes around the exe path are escaped for the /TR arg.
    let tr = format!("\\\"{exe}\\\" cron run {name}");
    let mut args: Vec<String> = vec![
        "/Create".into(),
        "/TN".into(),
        task.into(),
        "/TR".into(),
        tr,
        "/F".into(),
    ];
    match sched {
        Sched::DailyAt(h, m) => {
            args.extend([
                "/SC".into(),
                "DAILY".into(),
                "/ST".into(),
                format!("{h:02}:{m:02}"),
            ]);
        }
        Sched::Hourly => args.extend(["/SC".into(), "HOURLY".into()]),
        Sched::EveryMinutes(n) => {
            args.extend(["/SC".into(), "MINUTE".into(), "/MO".into(), n.to_string()])
        }
        Sched::EveryHours(n) => {
            args.extend(["/SC".into(), "HOURLY".into(), "/MO".into(), n.to_string()])
        }
    }
    let out = std::process::Command::new("schtasks")
        .args(&args)
        .output()
        .context("running schtasks /Create")?;
    if !out.status.success() {
        bail!(
            "schtasks /Create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn register_unix(task: &str, sched: &Sched, exe: &str, name: &str) -> Result<()> {
    let cron_expr = match sched {
        Sched::DailyAt(h, m) => format!("{m} {h} * * *"),
        Sched::Hourly => "0 * * * *".to_string(),
        Sched::EveryMinutes(n) => format!("*/{n} * * * *"),
        Sched::EveryHours(n) => format!("0 */{n} * * *"),
    };
    // `marker(task)` is a trailing comment so we can find + remove exactly our own line.
    let line = format!("{cron_expr} \"{exe}\" cron run {name} {}", marker(task));
    rewrite_crontab(|mut lines| {
        lines.retain(|l| !l.contains(&marker(task))); // replace any existing entry for this task
        lines.push(line.clone());
        lines
    })
}

fn marker(task: &str) -> String {
    format!("# {task}")
}

/// Read the current crontab, transform the lines, write it back. Empty crontab is fine.
fn rewrite_crontab(f: impl FnOnce(Vec<String>) -> Vec<String>) -> Result<()> {
    let existing = std::process::Command::new("crontab").arg("-l").output();
    let current = match existing {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(), // no crontab yet
    };
    let lines: Vec<String> = current.lines().map(|s| s.to_string()).collect();
    let next = f(lines);
    let body = next.join("\n") + "\n";

    use std::io::Write;
    let mut child = std::process::Command::new("crontab")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("running crontab -")?;
    child
        .stdin
        .as_mut()
        .context("crontab stdin")?
        .write_all(body.as_bytes())?;
    let status = child.wait().context("waiting for crontab")?;
    if !status.success() {
        bail!("crontab update failed");
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() > max {
        one_line.chars().take(max).collect::<String>() + "…"
    } else {
        one_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_schedules() {
        assert!(matches!(parse_schedule("hourly").unwrap(), Sched::Hourly));
        assert!(matches!(
            parse_schedule("daily@09:30").unwrap(),
            Sched::DailyAt(9, 30)
        ));
        assert!(matches!(
            parse_schedule("15m").unwrap(),
            Sched::EveryMinutes(15)
        ));
        assert!(matches!(
            parse_schedule("6h").unwrap(),
            Sched::EveryHours(6)
        ));
    }

    #[test]
    fn rejects_bad_schedules() {
        assert!(parse_schedule("daily@99:99").is_err());
        assert!(parse_schedule("0m").is_err());
        assert!(parse_schedule("weekly").is_err());
        assert!(parse_schedule("daily@9").is_err());
    }

    #[test]
    fn task_name_is_namespaced() {
        assert_eq!(task_name("Nightly Build"), "ng_nightly-build");
    }
}
