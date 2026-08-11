//! Small terminal-native Mermaid subset used by retained Markdown rendering.
//!
//! Unsupported syntax deliberately returns `None`; callers keep the original fenced source instead
//! of pretending a partial diagram is authoritative.
//!
//! Not wired into the Markdown renderer yet: nothing detects a ```mermaid fence and routes it here,
//! so every item below is unreachable from the binary. The renderer side is the missing half, not
//! this one — the parser is complete and covered by its own tests, so it is kept whole rather than
//! deleted and rewritten later. Remove the allow the moment a fence handler calls [`render`].
#![allow(dead_code)]

#[derive(Debug, Clone, PartialEq, Eq)]
struct Edge {
    from: String,
    to: String,
    label: Option<String>,
}

/// Render common Mermaid flow/state/sequence syntax as compact Unicode text.
pub fn render(source: &str, width: usize) -> Option<String> {
    let first = source.lines().find(|line| !line.trim().is_empty())?.trim();
    if first.starts_with("flowchart") || first.starts_with("graph ") {
        return render_flow(
            source
                .lines()
                .skip_while(|line| line.trim().is_empty())
                .skip(1),
            width,
        );
    }
    if first.starts_with("stateDiagram") {
        return render_flow(
            source
                .lines()
                .skip_while(|line| line.trim().is_empty())
                .skip(1),
            width,
        );
    }
    if first.starts_with("sequenceDiagram") {
        return render_sequence(
            source
                .lines()
                .skip_while(|line| line.trim().is_empty())
                .skip(1),
            width,
        );
    }
    None
}

fn render_flow<'a>(lines: impl Iterator<Item = &'a str>, width: usize) -> Option<String> {
    let mut edges = Vec::new();
    let mut node_labels = std::collections::HashMap::<String, String>::new();
    let mut node_aliases = std::collections::HashMap::<String, String>::new();
    for raw in lines {
        let line = raw.trim().trim_end_matches(';');
        if line.is_empty()
            || line.starts_with("%%")
            || line.starts_with("classDef")
            || line.starts_with("class ")
        {
            continue;
        }
        let (left, rest, arrow) = split_arrow(line)?;
        let (label, right) = if arrow == "-->|" {
            let close = rest.find('|')?;
            (
                Some(rest[..close].trim().to_string()),
                rest[close + 1..].trim(),
            )
        } else if let Some(tagged) = rest.strip_prefix('|') {
            // Common Mermaid label form: `A -->|yes| B`. Since split_arrow matched `-->`
            // (the bar comes AFTER the arrow), parse the leading `|label|` here.
            let close = tagged.find('|')?;
            (
                Some(tagged[..close].trim().to_string()),
                tagged[close + 1..].trim(),
            )
        } else {
            (None, rest.trim())
        };
        let (from_id, from_label) = node_parts(left);
        let (to_id, to_label) = node_parts(right);
        if from_id.is_empty() || to_id.is_empty() {
            return None;
        }
        if !from_label.is_empty() {
            node_aliases.insert(from_label.clone(), from_id.clone());
            node_labels.insert(from_id.clone(), from_label);
        }
        if !to_label.is_empty() {
            node_aliases.insert(to_label.clone(), to_id.clone());
            node_labels.insert(to_id.clone(), to_label);
        }
        edges.push(Edge {
            from: from_id,
            to: to_id,
            label,
        });
    }
    if edges.is_empty() {
        return None;
    }
    // Labels may be declared only on a later edge (`A --> B{Check}` then `B --> C`). Resolve after
    // parsing the whole graph so every occurrence of node id B renders as "Check".
    for edge in &mut edges {
        if let Some(id) = node_aliases.get(&edge.from) {
            edge.from = id.clone();
        }
        if let Some(id) = node_aliases.get(&edge.to) {
            edge.to = id.clone();
        }
        if let Some(label) = node_labels.get(&edge.from) {
            edge.from = label.clone();
        }
        if let Some(label) = node_labels.get(&edge.to) {
            edge.to = label.clone();
        }
    }
    let mut out = String::new();
    for (i, edge) in edges.iter().enumerate() {
        let arrow = edge
            .label
            .as_deref()
            .map(|l| format!(" ──{l}──▶ "))
            .unwrap_or_else(|| " ───▶ ".to_string());
        let row = format!("{}{}{}", edge.from, arrow, edge.to);
        out.push_str(&truncate(&row, width));
        if i + 1 < edges.len() {
            out.push('\n');
        }
    }
    Some(out)
}

fn render_sequence<'a>(lines: impl Iterator<Item = &'a str>, width: usize) -> Option<String> {
    let mut rows = Vec::new();
    for raw in lines {
        let line = raw.trim();
        if line.is_empty()
            || line.starts_with("%%")
            || line.starts_with("participant ")
            || line.starts_with("actor ")
        {
            continue;
        }
        let (left, rest, arrow) = split_sequence_arrow(line)?;
        let (right, label) = rest.split_once(':').unwrap_or((rest, ""));
        let glyph = if arrow.contains("--") {
            " - -▶ "
        } else {
            " ───▶ "
        };
        let mut row = format!("{}{}{}", left.trim(), glyph, right.trim());
        if !label.trim().is_empty() {
            row.push_str(&format!("  {}", label.trim()));
        }
        rows.push(truncate(&row, width));
    }
    (!rows.is_empty()).then(|| rows.join("\n"))
}

fn split_arrow(line: &str) -> Option<(&str, &str, &'static str)> {
    for (pat, tag) in [("-->|", "-->|"), ("-->", "-->"), ("--", "--"), ("->", "->")] {
        if let Some(i) = line.find(pat) {
            return Some((&line[..i], &line[i + pat.len()..], tag));
        }
    }
    None
}

fn split_sequence_arrow(line: &str) -> Option<(&str, &str, &'static str)> {
    for (pat, tag) in [
        ("-->>", "-->>"),
        ("->>", "->>"),
        ("-->", "-->"),
        ("->", "->"),
    ] {
        if let Some(i) = line.find(pat) {
            return Some((&line[..i], &line[i + pat.len()..], tag));
        }
    }
    None
}

fn node_parts(raw: &str) -> (String, String) {
    let t = raw.trim();
    let id_end = t
        .find(['[', '(', '{'])
        .unwrap_or_else(|| t.find(char::is_whitespace).unwrap_or(t.len()));
    let id = t[..id_end].trim();
    let label = if t.contains(['[', '(', '{']) {
        node_label(t)
    } else {
        String::new()
    };
    let id = if id == "[*]" { "●" } else { id };
    (id.to_string(), label)
}

fn node_label(raw: &str) -> String {
    let t = raw.trim();
    for (open, close) in [("[", "]"), ("(", ")"), ("{", "}")] {
        if let Some(i) = t.find(open) {
            if let Some(j) = t.rfind(close) {
                if j > i {
                    return t[i + 1..j].trim_matches(['"', '\'']).trim().to_string();
                }
            }
        }
    }
    let id = t.split_whitespace().next().unwrap_or("");
    if id == "[*]" {
        "●".to_string()
    } else {
        id.to_string()
    }
}

fn truncate(s: &str, width: usize) -> String {
    let width = width.max(8);
    if console::measure_text_width(s) <= width {
        s.to_string()
    } else {
        console::truncate_str(s, width, "…").into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flowchart_renders_labels() {
        let got = render(
            "flowchart LR\n A[Input] --> B{Check}\n B -->|yes| C[Done]",
            80,
        )
        .unwrap();
        assert!(got.contains("Input ───▶ Check"), "{got}");
        assert!(got.contains("Check ──yes──▶ Done"), "{got}");
    }

    #[test]
    fn sequence_renders_messages() {
        let got = render("sequenceDiagram\n A->>B: ping\n B-->>A: pong", 80).unwrap();
        assert!(got.contains("A ───▶ B  ping"), "{got}");
        assert!(got.contains("B - -▶ A  pong"), "{got}");
    }

    #[test]
    fn unsupported_fails_open() {
        assert!(render("pie\n title X", 80).is_none());
    }
}
