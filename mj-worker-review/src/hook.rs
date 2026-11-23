//! Repository-local pre-push review gate.

use super::capture::{checked_git, run};
use anyhow::{Context, Result};
use mj_checkpoint::archive::GitCommandRunner;
use mj_core::local_git::{canonical_git_output_path, resolve_git_path};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MARKER: &str = "# mj-review-gate";
const SCRIPT: &str = r#"#!/bin/sh
# mj-review-gate
hooks_dir=$(dirname "$0")
tmp=$(mktemp "${TMPDIR:-/tmp}/mj-review-gate.XXXXXXXXXX") || exit 1
trap 'rm -f "$tmp" "$tmp.bases" "$tmp.diff"' 0
trap 'exit 1' HUP INT TERM
cat > "$tmp" || exit 1
# Let Git match paths, including names containing whitespace or newlines.
# Disable renames so moving code to a documentation name still exposes its deletion.
documentation_only() {
  # Agent instructions and configuration remain configuration even in Markdown.
  git diff --quiet --no-ext-diff --no-textconv --no-renames --ignore-submodules=none \
    "$1" "$2" -- \
    ':(top,glob)**/AGENTS.md' ':(top,glob)**/AGENTS.override.md' \
    ':(top,glob)**/CLAUDE.md' ':(top,glob)**/CLAUDE.local.md' \
    ':(top,glob)**/GEMINI.md' ':(top,glob)**/SKILL.md' \
    ':(top,glob)**/*.instructions.md' ':(top,glob)**/*.prompt.md' \
    ':(top,glob)**/.agents/*.md' ':(top,glob)**/.agents/skills/**' \
    ':(top,glob)**/.github/**' ':(top,glob)**/.claude/**' \
    ':(top,glob)**/.codex/**' ':(top,glob)**/.gemini/**' \
    ':(top,glob)**/.cursor/**' ':(top,glob)**/.opencode/**' || return 1
  # A character class prevents Git treating an exact name as a directory prefix.
  git diff --quiet --no-ext-diff --no-textconv --no-renames --ignore-submodules=none \
    "$1" "$2" -- ':(top)**' \
    ':(top,exclude,glob)**/*.md' ':(top,exclude,glob)**/*.markdown' \
    ':(top,exclude,glob)**/*.rst' ':(top,exclude,glob)**/*.adoc' \
    ':(top,exclude,glob)**/*.asciidoc' \
    ':(top,exclude,glob)READM[E]' ':(top,exclude,glob)LICENS[E]' \
    ':(top,exclude,glob)COPYIN[G]' ':(top,exclude,glob)AUTHOR[S]' \
    ':(top,exclude,glob)CONTRIBUTOR[S]' ':(top,exclude,glob)CHANGELO[G]' \
    ':(top,exclude,glob)NEW[S]' || return 1
  # A documentation extension cannot exempt executables, links, or binary data.
  git diff --raw --numstat --no-abbrev --no-ext-diff --no-textconv --no-renames --ignore-submodules=none \
    "$1" "$2" -- > "$tmp.diff" || return 1
  awk '
    /^:/ && ($1 !~ /^:(000000|100644)$/ || $2 !~ /^(000000|100644)$/) { exit 1 }
    $1 == "-" || $2 == "-" { exit 1 }
  ' "$tmp.diff"
}
documentation_only_push() {
  case "$2" in
    *[!0]*)
      documentation_only "$2" "$1" && return 0
      # A reviewed comparison point must cover the destination's existing history.
      git for-each-ref --merged="$1" --contains="$2" --format='%(objectname)' \
        refs/mj/reviewed/ refs/mj/overridden/ > "$tmp.bases" || return 1
      ;;
    *)
      # A new branch can build on published history or a reviewed ancestor.
      git for-each-ref --merged="$1" --format='%(objectname)' \
        refs/mj/reviewed/ refs/mj/overridden/ > "$tmp.bases" || return 1
      # Fetch history describes the push destination only when their URLs agree.
      if fetch_url=$(git remote get-url "$3" 2>/dev/null) && [ "$fetch_url" = "$4" ]; then
        git for-each-ref --merged="$1" --format='%(objectname)' \
          "refs/remotes/$3/" >> "$tmp.bases" || return 1
      fi
      ;;
  esac
  # Compare whole trees, never just the tip commit, to include earlier code edits.
  while read -r base; do
    documentation_only "$base" "$1" && return 0
  done < "$tmp.bases"
  # A repository containing only documentation needs no initial code review.
  case "$2" in
    *[!0]*) return 1 ;;
  esac
  [ ! -s "$tmp.bases" ] || return 1
  empty_tree=$(git hash-object -w -t tree --stdin < /dev/null) || return 1
  documentation_only "$empty_tree" "$1"
}
if [ "${MJ_REVIEW_GATE:-}" = "1" ]; then
  while read -r local_ref local_sha remote_ref remote_sha; do
    case "$local_sha" in
      *[!0]*|'') ;;
      *) continue ;;
    esac
    tree=$(git rev-parse --verify "$local_sha^{tree}") || exit 1
    if ! git show-ref --verify --quiet "refs/mj/reviewed/$tree" && ! git show-ref --verify --quiet "refs/mj/overridden/$tree"; then
      if documentation_only_push "$local_sha" "$remote_sha" "$1" "$2"; then
        echo "mj: $local_ref changes only plain documentation; review is not required." >&2
        continue
      fi
      echo "mj: $local_ref (tree $tree) has not been reviewed." >&2
      echo "mj: call the mj-review request_review tool for the worktree \"$(git rev-parse --show-toplevel)\", wait for the verdict message, then push again." >&2
      exit 1
    fi
  done < "$tmp"
fi
if [ -x "$hooks_dir/pre-push.before-mj" ]; then
  "$hooks_dir/pre-push.before-mj" "$@" < "$tmp"
  exit $?
fi
exit 0
"#;

#[derive(Debug, PartialEq, Eq)]
pub enum HookInstall {
    Installed,
    Rewritten,
    RefusedExistingBackup(PathBuf),
    RefusedWorkTreeHooksDirectory(PathBuf),
}

/// Installs the gate where Git runs hooks, preserving an existing user hook.
pub fn install_pre_push_hook(runner: &dyn GitCommandRunner, root: &Path) -> Result<HookInstall> {
    let configured = run(
        runner,
        root,
        &["config", "--path", "--get", "core.hooksPath"],
    )?;
    let directory = match configured.status {
        0 => configured.stdout,
        1 => checked_git(
            runner,
            root,
            &["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
        )?,
        _ => anyhow::bail!(
            "read core.hooksPath: {}",
            String::from_utf8_lossy(&configured.stderr)
        ),
    };
    let work_tree = canonical_git_output_path(Path::new(
        String::from_utf8(checked_git(
            runner,
            root,
            &["rev-parse", "--show-toplevel"],
        )?)?
        .trim_end_matches('\n'),
    ))?;
    let directory = resolve_git_path(&work_tree, &String::from_utf8(directory)?)?;
    let directory = canonical_hooks_directory(&directory)?;
    let common = String::from_utf8(checked_git(
        runner,
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)?;
    let common = canonical_git_output_path(Path::new(common.trim_end_matches('\n')))?;
    let hook = directory.join("pre-push");
    if directory.starts_with(&work_tree) && !directory.starts_with(&common) {
        let ignored = run(
            runner,
            root,
            &[
                "check-ignore",
                "-q",
                "--",
                hook.to_str().context("decode the hook path")?,
            ],
        )?;
        match ignored.status {
            0 => {}
            1 => return Ok(HookInstall::RefusedWorkTreeHooksDirectory(directory)),
            _ => anyhow::bail!(
                "check whether the hook is ignored: {}",
                String::from_utf8_lossy(&ignored.stderr)
            ),
        }
    }
    fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
    let backup = directory.join("pre-push.before-mj");
    let outcome = match fs::read(&hook) {
        Ok(contents)
            if contents
                .split(|byte| *byte == b'\n')
                .any(|line| line == MARKER.as_bytes()) =>
        {
            HookInstall::Rewritten
        }
        Ok(_) => {
            // Hard-linking reserves the backup name without overwriting a
            // concurrently created file. Removing the old name completes the move.
            match fs::hard_link(&hook, &backup) {
                Ok(()) => fs::remove_file(&hook).context("move the previous pre-push hook")?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let original = fs::symlink_metadata(&hook)?;
                    let saved = fs::symlink_metadata(&backup)?;
                    if original.dev() != saved.dev() || original.ino() != saved.ino() {
                        return Ok(HookInstall::RefusedExistingBackup(backup));
                    }
                    fs::remove_file(&hook).context("complete the previous hook backup")?;
                }
                Err(error) => return Err(error).context("preserve the previous pre-push hook"),
            }
            HookInstall::Installed
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => HookInstall::Installed,
        Err(error) => return Err(error).context("read the previous pre-push hook"),
    };
    // Set the executable mode before the atomic rename makes the script visible.
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    use std::io::Write;
    temporary.write_all(SCRIPT.as_bytes())?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o755))?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&hook)
        .map_err(|error| error.error)
        .context("install the pre-push review gate")?;
    Ok(outcome)
}

// Resolve existing symlink ancestors without creating an untracked directory.
fn canonical_hooks_directory(directory: &Path) -> Result<PathBuf> {
    let mut ancestor = directory;
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                resolved.extend(missing.into_iter().rev());
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name().context("locate the hooks directory")?);
                ancestor = ancestor
                    .parent()
                    .context("locate the hooks directory parent")?;
            }
            Err(error) => return Err(error).context("resolve the hooks directory"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::push_tests::{commit, repository};
    use crate::tests::git;
    use mj_checkpoint::archive::{
        GitCommand, GitOutput, SystemGit, record_overridden_tree, record_reviewed_tree,
    };

    fn push(root: &Path, gate: bool, refs: &[&str]) -> GitOutput {
        let mut arguments = vec!["push".into(), "origin".into()];
        arguments.extend(refs.iter().map(Into::into));
        SystemGit
            .run(
                root,
                &GitCommand {
                    arguments,
                    stdin: Vec::new(),
                    env: vec![("MJ_REVIEW_GATE".into(), if gate { "1" } else { "0" }.into())],
                },
            )
            .unwrap()
    }

    fn reviewed(root: &Path) {
        let tree = git(root, &["rev-parse", "HEAD^{tree}"]);
        let head = git(root, &["rev-parse", "HEAD"]);
        record_reviewed_tree(&SystemGit, root, &tree, &head).unwrap();
    }

    fn commit_file(root: &Path, path: &str, contents: &[u8]) {
        fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
        fs::write(root.join(path), contents).unwrap();
        git(root, &["add", "--", path]);
        git(root, &["commit", "-qm", "change a file"]);
    }

    #[test]
    fn hook_exempts_documentation_after_published_or_reviewed_work() {
        let temp = repository();
        let root = temp.path().join("checkout");
        install_pre_push_hook(&SystemGit, &root).unwrap();
        fs::create_dir(root.join("docs")).unwrap();
        for path in [
            "README.md",
            "docs/notes with\na newline.rst",
            "LICENSE",
            ".agents/docs/context.md",
            ".agents/plans/feature.md",
        ] {
            commit_file(&root, path, b"Plain documentation\n");
        }
        assert_eq!(push(&root, true, &["HEAD:refs/heads/main"]).status, 0);
        commit_file(&root, "docs/guide.adoc", b"More documentation\n");
        git(&root, &["mv", "docs/guide.adoc", "docs/renamed.markdown"]);
        git(&root, &["rm", "docs/notes with\na newline.rst"]);
        git(&root, &["commit", "-qm", "rename and delete documentation"]);
        assert_eq!(push(&root, true, &["HEAD:refs/heads/docs"]).status, 0);
        // The code has been reviewed but has never been published.
        commit(&root, "reviewed code");
        reviewed(&root);
        commit_file(&root, "README.md", b"Updated documentation\n");
        assert_eq!(
            push(&root, true, &["HEAD:refs/heads/reviewed-docs"]).status,
            0
        );
        assert_eq!(push(&root, true, &["HEAD:refs/heads/main"]).status, 0);
        // Documentation exemptions must not manufacture review approval refs.
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        assert!(!git(&root, &["for-each-ref", "refs/mj/reviewed/"]).contains(&tree));
    }

    #[test]
    fn hook_exempts_documentation_roots_but_not_replacing_code_with_them() {
        let temp = repository();
        let root = temp.path().join("checkout");
        install_pre_push_hook(&SystemGit, &root).unwrap();
        git(&root, &["checkout", "--orphan", "documentation"]);
        git(&root, &["rm", "-rf", "."]);
        commit_file(
            &root,
            "README.md",
            b"An independent documentation history\n",
        );
        assert_eq!(push(&root, true, &["HEAD:refs/heads/docs"]).status, 0);
        // Another published branch is no license to delete the destination's code.
        assert_ne!(push(&root, true, &["+HEAD:refs/heads/main"]).status, 0);
    }

    #[test]
    fn hook_documentation_exemption_cannot_revert_published_code_to_a_reviewed_ancestor() {
        let temp = repository();
        let root = temp.path().join("checkout");
        install_pre_push_hook(&SystemGit, &root).unwrap();
        reviewed(&root);
        let reviewed = git(&root, &["rev-parse", "HEAD"]);
        commit(&root, "newer published code");
        assert_eq!(push(&root, false, &["HEAD:refs/heads/main"]).status, 0);
        git(&root, &["reset", "--hard", &reviewed]);
        commit_file(
            &root,
            "README.md",
            b"Documentation after the older reviewed code\n",
        );
        assert_ne!(push(&root, true, &["+HEAD:refs/heads/main"]).status, 0);
    }

    #[test]
    fn hook_documentation_exemption_includes_ignored_submodule_updates() {
        let temp = repository();
        let root = temp.path().join("checkout");
        install_pre_push_hook(&SystemGit, &root).unwrap();
        let first = git(&root, &["rev-parse", "HEAD"]);
        commit_file(
            &root,
            ".gitmodules",
            b"[submodule \"vendor\"]\n\tpath = vendor\n\turl = /unused\n",
        );
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                &first,
                "vendor",
            ],
        );
        git(&root, &["commit", "-qm", "add a submodule"]);
        let second = git(&root, &["rev-parse", "HEAD"]);
        assert_eq!(push(&root, false, &["HEAD:refs/heads/main"]).status, 0);
        git(
            &root,
            &["update-index", "--cacheinfo", "160000", &second, "vendor"],
        );
        git(&root, &["commit", "-qm", "update the dependency code"]);
        commit_file(
            &root,
            "README.md",
            b"Documentation beside a submodule update\n",
        );
        for setting in ["diff.ignoreSubmodules", "submodule.vendor.ignore"] {
            git(&root, &["config", setting, "all"]);
            assert_ne!(
                push(&root, true, &["HEAD:refs/heads/main"]).status,
                0,
                "{setting}"
            );
            assert_ne!(
                push(&root, true, &["HEAD:refs/heads/new"]).status,
                0,
                "{setting}"
            );
            git(&root, &["config", "--unset", setting]);
        }
    }

    #[test]
    fn hook_documentation_exemption_keeps_code_and_non_plain_files_gated() {
        let temp = repository();
        let root = temp.path().join("checkout");
        install_pre_push_hook(&SystemGit, &root).unwrap();
        let base = git(&root, &["rev-parse", "HEAD"]);
        for path in [
            "main.rs",
            "Cargo.toml",
            "Cargo.lock",
            "build.sh",
            "page.mdx",
            "requirements.txt",
            "code\nwith a newline.rs",
            "AGENTS.md",
            ".agents/PLANS.md",
            "nested/CLAUDE.md",
            "assets/skills/mj/SKILL.md",
            ".github/instructions/code.instructions.md",
            ".claude/rules/conventions.md",
            "README/build.sh",
            "LICENSE/main.rs",
            "nested.md/main.rs",
        ] {
            commit_file(&root, path, b"Needs review\n");
            commit_file(&root, "README.md", b"Documentation on top of code\n");
            assert_ne!(
                push(&root, true, &["HEAD:refs/heads/main"]).status,
                0,
                "{path}"
            );
            assert_ne!(
                push(&root, true, &["HEAD:refs/heads/new"]).status,
                0,
                "{path}"
            );
            git(&root, &["reset", "--hard", &base]);
        }
        commit_file(&root, "binary.md", b"binary\0data\n");
        assert_ne!(push(&root, true, &["HEAD:refs/heads/main"]).status, 0);
        git(&root, &["reset", "--hard", &base]);
        commit_file(&root, "executable.md", b"#!/bin/sh\nexit 0\n");
        git(&root, &["update-index", "--chmod=+x", "executable.md"]);
        git(&root, &["commit", "-qm", "make documentation executable"]);
        assert_ne!(push(&root, true, &["HEAD:refs/heads/main"]).status, 0);
        git(&root, &["reset", "--hard", &base]);
        std::os::unix::fs::symlink("tracked.txt", root.join("link.md")).unwrap();
        git(&root, &["add", "link.md"]);
        git(&root, &["commit", "-qm", "add a symbolic link"]);
        assert_ne!(push(&root, true, &["HEAD:refs/heads/main"]).status, 0);
        git(&root, &["reset", "--hard", &base]);
        git(&root, &["mv", "tracked.txt", "renamed.md"]);
        git(&root, &["commit", "-qm", "rename code as documentation"]);
        assert_ne!(push(&root, true, &["HEAD:refs/heads/main"]).status, 0);
    }

    #[test]
    fn hook_documentation_exemption_does_not_use_fetch_history_for_another_push_url() {
        let temp = repository();
        let root = temp.path().join("checkout");
        let destination = temp.path().join("different-origin.git");
        git(
            temp.path(),
            &["init", "--bare", "-q", destination.to_str().unwrap()],
        );
        git(
            &root,
            &[
                "remote",
                "set-url",
                "--push",
                "origin",
                destination.to_str().unwrap(),
            ],
        );
        install_pre_push_hook(&SystemGit, &root).unwrap();
        commit_file(
            &root,
            "README.md",
            b"Documentation after code from another repository\n",
        );
        assert_ne!(push(&root, true, &["HEAD:refs/heads/docs"]).status, 0);
    }

    #[test]
    fn hook_documentation_exemption_checks_all_refs_and_preserves_user_hooks() {
        let temp = repository();
        let root = temp.path().join("checkout");
        fs::write(
            root.join(".git/hooks/pre-push"),
            "#!/bin/sh\ncat > hook-input\nexit 23\n",
        )
        .unwrap();
        fs::set_permissions(
            root.join(".git/hooks/pre-push"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        install_pre_push_hook(&SystemGit, &root).unwrap();
        commit_file(&root, "README.md", b"Documentation\n");
        // An exempt push still runs the user's hook and obeys its refusal.
        assert_ne!(push(&root, true, &["HEAD:refs/heads/docs"]).status, 0);
        assert!(root.join("hook-input").exists());
        fs::remove_file(root.join("hook-input")).unwrap();
        let docs = git(&root, &["rev-parse", "HEAD"]);
        commit(&root, "unreviewed code");
        assert_ne!(
            push(
                &root,
                true,
                &[&format!("{docs}:refs/heads/docs"), "HEAD:refs/heads/code"]
            )
            .status,
            0
        );
        assert!(!root.join("hook-input").exists());
    }

    #[test]
    fn hook_gates_pushes_by_tree_allows_deletion_and_is_idempotent() {
        let temp = repository();
        let root = temp.path().join("checkout").canonicalize().unwrap();
        assert_eq!(
            install_pre_push_hook(&SystemGit, &root).unwrap(),
            HookInstall::Installed
        );
        assert_eq!(
            install_pre_push_hook(&SystemGit, &root).unwrap(),
            HookInstall::Rewritten
        );
        assert!(!root.join(".git/hooks/pre-push.before-mj").exists());
        commit(&root, "unreviewed");
        let rejected = push(&root, true, &["HEAD:refs/heads/feature"]);
        assert_ne!(rejected.status, 0);
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(stderr.contains("mj: HEAD (tree "), "{stderr}");
        assert!(stderr.contains("has not been reviewed."), "{stderr}");
        assert!(stderr.contains(&format!("call the mj-review request_review tool for the worktree \"{}\", wait for the verdict message, then push again.", root.display())), "{stderr}");
        reviewed(&root);
        assert_eq!(push(&root, true, &["HEAD:refs/heads/feature"]).status, 0);
        commit(&root, "gate off");
        assert_eq!(push(&root, false, &["HEAD:refs/heads/feature"]).status, 0);
        assert_eq!(push(&root, true, &[":refs/heads/feature"]).status, 0);
    }

    // An overridden tree may be pushed, and a hook written before overridden
    // refs existed is replaced when the hook is installed again.
    #[test]
    fn hook_accepts_an_overridden_tree_and_a_reinstall_replaces_the_old_script() {
        let temp = repository();
        let root = temp.path().join("checkout").canonicalize().unwrap();
        install_pre_push_hook(&SystemGit, &root).unwrap();
        let hook = root.join(".git/hooks/pre-push");
        let old = SCRIPT
            .replace(
                " && ! git show-ref --verify --quiet \"refs/mj/overridden/$tree\"",
                "",
            )
            .replace("refs/mj/reviewed/ refs/mj/overridden/", "refs/mj/reviewed/");
        assert_ne!(old, SCRIPT);
        fs::write(&hook, &old).unwrap();
        let head = commit(&root, "overridden");
        let tree = git(&root, &["rev-parse", "HEAD^{tree}"]);
        record_overridden_tree(&SystemGit, &root, &tree, &head).unwrap();
        assert_ne!(push(&root, true, &["HEAD:refs/heads/feature"]).status, 0);
        assert_eq!(
            install_pre_push_hook(&SystemGit, &root).unwrap(),
            HookInstall::Rewritten
        );
        assert_eq!(fs::read_to_string(&hook).unwrap(), SCRIPT);
        assert!(!root.join(".git/hooks/pre-push.before-mj").exists());
        assert_eq!(push(&root, true, &["HEAD:refs/heads/feature"]).status, 0);
    }

    #[test]
    fn preserved_hook_gets_arguments_stdin_and_controls_exit_status() {
        let temp = repository();
        let root = temp.path().join("checkout").canonicalize().unwrap();
        let hooks = root.join(".git/hooks");
        fs::write(
            hooks.join("pre-push"),
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > hook-arguments\ncat > hook-input\nexit 23\n",
        )
        .unwrap();
        fs::set_permissions(hooks.join("pre-push"), fs::Permissions::from_mode(0o755)).unwrap();
        install_pre_push_hook(&SystemGit, &root).unwrap();
        install_pre_push_hook(&SystemGit, &root).unwrap();
        assert!(hooks.join("pre-push.before-mj").exists());
        let head = commit(&root, "later");
        assert_ne!(push(&root, true, &["HEAD:refs/heads/feature"]).status, 0);
        assert!(!root.join("hook-input").exists());
        reviewed(&root);
        assert_ne!(push(&root, true, &["HEAD:refs/heads/feature"]).status, 0);
        assert!(
            fs::read_to_string(root.join("hook-input"))
                .unwrap()
                .starts_with(&format!("HEAD {head} refs/heads/feature "))
        );
        assert!(
            fs::read_to_string(root.join("hook-arguments"))
                .unwrap()
                .starts_with("origin\n")
        );
        fs::remove_file(root.join("hook-input")).unwrap();
        assert_ne!(push(&root, false, &["HEAD:refs/heads/feature"]).status, 0);
        assert!(root.join("hook-input").exists());
        // Git collapses hook failures to status 1. Call the script directly
        // as well to check that it preserves the chained hook's exact status.
        let input = "ref line\n".repeat(20_000);
        let temporary = root.join("hook-temporary");
        fs::create_dir(&temporary).unwrap();
        let output = mj_core::subprocess::run_with_input(
            std::process::Command::new(hooks.join("pre-push"))
                .current_dir(&root)
                .env("MJ_REVIEW_GATE", "0")
                .env("TMPDIR", &temporary),
            input.as_bytes(),
        );
        assert_eq!(output.unwrap().status.code(), Some(23));
        assert_eq!(fs::read_to_string(root.join("hook-input")).unwrap(), input);
        assert_eq!(fs::read_dir(temporary).unwrap().count(), 0);
    }

    #[test]
    fn configured_hooks_path_and_linked_worktree_use_git_hook_locations() {
        let temp = repository();
        let root = temp.path().join("checkout").canonicalize().unwrap();
        let external = temp.path().join("custom hooks");
        git(
            &root,
            &["config", "core.hooksPath", external.to_str().unwrap()],
        );
        install_pre_push_hook(&SystemGit, &root).unwrap();
        assert!(external.join("pre-push").exists());
        commit(&root, "later");
        assert_ne!(push(&root, true, &["HEAD:refs/heads/custom"]).status, 0);
        git(&root, &["config", "--unset", "core.hooksPath"]);
        let linked = temp.path().join("linked");
        git(
            &root,
            &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
        );
        assert!(linked.join(".git").is_file());
        install_pre_push_hook(&SystemGit, &linked).unwrap();
        assert!(root.join(".git/hooks/pre-push").exists());
        assert_ne!(push(&linked, true, &["HEAD:refs/heads/linked"]).status, 0);
        reviewed(&linked);
        assert_eq!(push(&linked, true, &["HEAD:refs/heads/linked"]).status, 0);
    }

    // Hard-won: before-push review: installing into tracked Husky hooks dirtied every review capture.
    #[test]
    fn tracked_worktree_hooks_are_refused_without_file_changes() {
        let temp = repository();
        let root = temp.path().join("checkout").canonicalize().unwrap();
        let hooks = root.join(".husky");
        fs::create_dir(&hooks).unwrap();
        fs::write(hooks.join("pre-push"), "#!/bin/sh\nexit 0\n").unwrap();
        git(&root, &["add", ".husky/pre-push"]);
        git(&root, &["commit", "-qm", "tracked Husky hook"]);
        git(&root, &["config", "core.hooksPath", ".husky"]);
        let before = fs::read(hooks.join("pre-push")).unwrap();
        assert_eq!(
            install_pre_push_hook(&SystemGit, &root).unwrap(),
            HookInstall::RefusedWorkTreeHooksDirectory(hooks.clone())
        );
        assert_eq!(fs::read(hooks.join("pre-push")).unwrap(), before);
        assert_eq!(fs::read_dir(&hooks).unwrap().count(), 1);
        assert!(git(&root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
        let head = git(&root, &["rev-parse", "HEAD"]);
        super::super::capture::capture_pushable_commits(&SystemGit, &root, &head).unwrap();
        let alias = temp.path().join("linked-hooks");
        std::os::unix::fs::symlink(&hooks, &alias).unwrap();
        git(
            &root,
            &["config", "core.hooksPath", alias.to_str().unwrap()],
        );
        assert_eq!(
            install_pre_push_hook(&SystemGit, &root).unwrap(),
            HookInstall::RefusedWorkTreeHooksDirectory(hooks)
        );
        // Refuse untracked pollution before even creating the directory.
        git(&root, &["config", "core.hooksPath", ".untracked-hooks"]);
        assert_eq!(
            install_pre_push_hook(&SystemGit, &root).unwrap(),
            HookInstall::RefusedWorkTreeHooksDirectory(root.join(".untracked-hooks"))
        );
        assert!(!root.join(".untracked-hooks").exists());
    }

    #[test]
    fn ignored_worktree_hooks_install_without_dirtying_the_checkout() {
        let temp = repository();
        let root = temp.path().join("checkout").canonicalize().unwrap();
        let hooks = root.join(".husky/_");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join(".gitignore"), "*\n").unwrap();
        git(&root, &["config", "core.hooksPath", ".husky/_"]);
        assert_eq!(
            install_pre_push_hook(&SystemGit, &root).unwrap(),
            HookInstall::Installed
        );
        assert!(hooks.join("pre-push").exists());
        assert!(git(&root, &["status", "--porcelain", "--untracked-files=all"]).is_empty());
        commit(&root, "unreviewed");
        assert_ne!(push(&root, true, &["HEAD:refs/heads/feature"]).status, 0);
        reviewed(&root);
        assert_eq!(push(&root, true, &["HEAD:refs/heads/feature"]).status, 0);
    }

    #[test]
    fn interrupted_backup_of_the_same_file_can_finish() {
        let temp = repository();
        let root = temp.path().join("checkout").canonicalize().unwrap();
        let hooks = root.join(".git/hooks");
        fs::write(hooks.join("pre-push"), "user hook").unwrap();
        fs::hard_link(hooks.join("pre-push"), hooks.join("pre-push.before-mj")).unwrap();
        assert_eq!(
            install_pre_push_hook(&SystemGit, &root).unwrap(),
            HookInstall::Installed
        );
        assert_eq!(
            fs::read_to_string(hooks.join("pre-push.before-mj")).unwrap(),
            "user hook"
        );
    }

    #[test]
    fn taken_backup_refuses_without_changing_either_hook() {
        let temp = repository();
        let root = temp.path().join("checkout").canonicalize().unwrap();
        let hooks = root.join(".git/hooks");
        fs::write(hooks.join("pre-push"), "user hook").unwrap();
        fs::write(hooks.join("pre-push.before-mj"), "earlier hook").unwrap();
        assert_eq!(
            install_pre_push_hook(&SystemGit, &root).unwrap(),
            HookInstall::RefusedExistingBackup(hooks.join("pre-push.before-mj"))
        );
        assert_eq!(
            fs::read_to_string(hooks.join("pre-push")).unwrap(),
            "user hook"
        );
        assert_eq!(
            fs::read_to_string(hooks.join("pre-push.before-mj")).unwrap(),
            "earlier hook"
        );
    }
}
