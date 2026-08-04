use anyhow::{Context, Result, bail};
use similar::TextDiff;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

const AGENTS_FILE: &str = "AGENTS.md";
const START_MARKER: &str = "<!-- mjolnir:bifrost-agent-guidance:start -->";
const END_MARKER: &str = "<!-- mjolnir:bifrost-agent-guidance:end -->";
const GUIDANCE_HEADING: &str = "# Bifrost Code Intelligence";
const GUIDANCE_SOURCE: &str = "https://brokkai.github.io/bifrost/agents/";

// Canonical text from BrokkAi/bifrost's bifrost://agent-guidance/agents.md resource.
const GUIDANCE: &str = r#"# Bifrost Code Intelligence

When planning broad refactors, risky behavior changes, or edits to large classes
or modules, use Bifrost's structured code-intelligence tools before proposing a
plan or modifying code.

- Start with `get_summaries` for the target files, directories, classes, or
  modules so the plan is grounded in the actual API shape and neighboring code.
- Use `search_symbols` to find relevant classes, functions, methods, fields, and
  modules by name before opening files manually.
- Use `get_symbol_sources` when you need the exact body of a known symbol.
- Use `scan_usages` before changing existing behavior so callers, references,
  and related tests are considered.
- Prefer analyzer-backed summaries, symbols, definitions, and usages over raw
  grep or repeated file reads for code navigation decisions.
- Trust Bifrost for alias-aware and import-aware resolution. Text search may
  miss references that use aliases, re-exports, imports, or language-specific
  indirection.

Keep project-specific instructions in the existing `AGENTS.md`. Append this
section only to steer agents toward Bifrost context gathering before they make
implementation plans.
"#;

pub fn install(root: &Path, yes: bool) -> Result<()> {
    let plan = InstallPlan::load(root)?;
    if plan.previous == plan.proposed {
        println!(
            "{} already contains the current Bifrost guidance.",
            plan.path.display()
        );
        return Ok(());
    }

    let old_name = if plan.previous.is_empty() {
        "/dev/null".to_string()
    } else {
        plan.path.display().to_string()
    };
    let new_name = plan.path.display().to_string();
    print!(
        "{}",
        TextDiff::from_lines(&plan.previous, &plan.proposed)
            .unified_diff()
            .header(&old_name, &new_name)
    );
    println!("Canonical source: {GUIDANCE_SOURCE}");

    if !yes && !confirm_install()? {
        println!("No changes written.");
        return Ok(());
    }

    plan.write()?;
    println!(
        "Installed Bifrost agent guidance in {}.",
        plan.path.display()
    );
    Ok(())
}

fn confirm_install() -> Result<bool> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    if !stdin.is_terminal() || !stdout.is_terminal() {
        bail!(
            "confirmation requires terminal input and output; rerun with --yes to apply the displayed diff"
        );
    }

    write!(stdout, "Apply this change? [y/N] ").context("write confirmation prompt")?;
    stdout.flush().context("flush confirmation prompt")?;

    let mut answer = String::new();
    stdin
        .read_line(&mut answer)
        .context("read confirmation response")?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

struct InstallPlan {
    path: PathBuf,
    previous: String,
    proposed: String,
}

impl InstallPlan {
    fn load(root: &Path) -> Result<Self> {
        let path = root.join(AGENTS_FILE);
        let previous = read_agents_file(&path)?;
        let proposed = merge_guidance(&previous)?;
        Ok(Self {
            path,
            previous,
            proposed,
        })
    }

    fn write(&self) -> Result<()> {
        if read_agents_file(&self.path)? != self.previous {
            bail!(
                "{} changed after the preview; no changes were written",
                self.path.display()
            );
        }
        fs::write(&self.path, &self.proposed)
            .with_context(|| format!("write {}", self.path.display()))
    }
}

fn read_agents_file(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn merge_guidance(existing: &str) -> Result<String> {
    let start_count = existing.matches(START_MARKER).count();
    let end_count = existing.matches(END_MARKER).count();
    match (start_count, end_count) {
        (0, 0) => append_guidance(existing),
        (1, 1) => replace_guidance(existing),
        _ => {
            bail!("{AGENTS_FILE} contains malformed Bifrost guidance markers; leaving it unchanged")
        }
    }
}

fn append_guidance(existing: &str) -> Result<String> {
    if existing.lines().any(|line| line.trim() == GUIDANCE_HEADING) {
        if normalize_newlines(existing).contains(GUIDANCE.trim_end()) {
            return Ok(existing.to_string());
        }
        bail!(
            "{AGENTS_FILE} already contains an unmanaged '{GUIDANCE_HEADING}' section; leaving it unchanged"
        );
    }

    let newline = newline_for(existing);
    let mut proposed = existing.to_string();
    if !proposed.is_empty() {
        if !proposed.ends_with(newline) {
            proposed.push_str(newline);
        }
        if !proposed.ends_with(&format!("{newline}{newline}")) {
            proposed.push_str(newline);
        }
    }
    proposed.push_str(&managed_block(newline));
    Ok(proposed)
}

fn replace_guidance(existing: &str) -> Result<String> {
    let start = existing.find(START_MARKER).expect("validated start marker");
    let end = existing.find(END_MARKER).expect("validated end marker");
    if end < start {
        bail!("{AGENTS_FILE} has reversed Bifrost guidance markers; leaving it unchanged");
    }

    let mut range_end = end + END_MARKER.len();
    if existing[range_end..].starts_with("\r\n") {
        range_end += 2;
    } else if existing[range_end..].starts_with('\n') {
        range_end += 1;
    }

    let newline = newline_for(existing);
    let mut proposed = existing.to_string();
    proposed.replace_range(start..range_end, &managed_block(newline));
    Ok(proposed)
}

fn managed_block(newline: &str) -> String {
    format!("{START_MARKER}\n{GUIDANCE}{END_MARKER}\n").replace('\n', newline)
}

fn newline_for(contents: &str) -> &'static str {
    if contents.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn normalize_newlines(contents: &str) -> String {
    contents.replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_plan_creates_agents_file_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = InstallPlan::load(dir.path()).expect("plan");

        assert!(plan.previous.is_empty());
        assert!(plan.proposed.starts_with(START_MARKER));
        assert!(plan.proposed.contains("`get_summaries`"));
        assert!(plan.proposed.contains("`get_symbol_sources`"));

        plan.write().expect("write plan");
        assert_eq!(
            fs::read_to_string(dir.path().join(AGENTS_FILE)).expect("read installed file"),
            plan.proposed
        );
    }

    #[test]
    fn install_plan_preserves_existing_agents_instructions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(AGENTS_FILE);
        fs::write(&path, "# Project Instructions\n\nKeep this text.\n").expect("seed file");

        let plan = InstallPlan::load(dir.path()).expect("plan");
        assert!(
            plan.proposed
                .starts_with("# Project Instructions\n\nKeep this text.\n\n")
        );
        assert!(plan.proposed.contains(START_MARKER));

        plan.write().expect("write plan");
        let installed = fs::read_to_string(path).expect("read installed file");
        assert!(installed.starts_with("# Project Instructions\n\nKeep this text.\n\n"));
        assert!(installed.ends_with(&format!("{END_MARKER}\n")));
    }

    #[test]
    fn install_plan_updates_only_managed_guidance() {
        let existing = format!("before\n\n{START_MARKER}\nold guidance\n{END_MARKER}\n\nafter\n");
        let updated = merge_guidance(&existing).expect("merge");

        assert!(updated.starts_with("before\n\n"));
        assert!(updated.ends_with("\nafter\n"));
        assert!(!updated.contains("old guidance"));
        assert!(updated.contains(GUIDANCE.trim_end()));
    }

    #[test]
    fn install_plan_is_idempotent() {
        let installed = merge_guidance("").expect("first merge");
        assert_eq!(merge_guidance(&installed).expect("second merge"), installed);
    }

    #[test]
    fn install_plan_refuses_to_overwrite_changes_made_after_preview() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(AGENTS_FILE);
        fs::write(&path, "original\n").expect("seed file");
        let plan = InstallPlan::load(dir.path()).expect("plan");

        fs::write(&path, "edited while confirming\n").expect("concurrent edit");
        let error = plan.write().expect_err("refuse stale plan");

        assert!(error.to_string().contains("changed after the preview"));
        assert_eq!(
            fs::read_to_string(path).expect("read preserved edit"),
            "edited while confirming\n"
        );
    }

    #[test]
    fn unmanaged_bifrost_section_is_not_duplicated_or_replaced() {
        let existing = "# Bifrost Code Intelligence\n\nCustom guidance.\n";
        let error = merge_guidance(existing).expect_err("refuse unmanaged section");

        assert!(error.to_string().contains("unmanaged"));
    }
}
