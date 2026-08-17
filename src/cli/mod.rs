//! One module per top-level `aizen <subcommand>`: the argument shapes live in `cli_args`, the
//! behaviour lives here. Split out of `main.rs`, which now only dispatches.

pub mod agents_cmd;
pub mod apps;
pub mod coop_cmd;
pub mod memory_cmd;
pub mod persona_cmd;
pub mod run_cmds;
pub mod sandbox_cmd;
pub mod sessions;
pub mod skill_cmd;
pub mod time;
pub mod where_report;

/// Read all of stdin, for the `-`/omitted body argument every "save a document" subcommand accepts.
pub(crate) fn read_stdin(ctx: &'static str) -> anyhow::Result<String> {
    use anyhow::Context;
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).context(ctx)?;
    // Strip a leading UTF-8 BOM (PowerShell's `|` prepends one) before trimming.
    Ok(buf
        .strip_prefix('\u{FEFF}')
        .unwrap_or(&buf)
        .trim()
        .to_string())
}
