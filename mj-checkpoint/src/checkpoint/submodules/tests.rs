use super::*;
use crate::test_support::{git, git_line};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Answers each Git command from a script keyed by the directory it runs in
/// and its first argument, and records where each command ran.
struct ScriptedGit {
    answers: BTreeMap<(PathBuf, String), GitOutput>,
    ran: Mutex<Vec<(PathBuf, String)>>,
}

impl ScriptedGit {
    fn new(answers: impl IntoIterator<Item = (PathBuf, &'static str, GitOutput)>) -> Self {
        Self {
            answers: answers
                .into_iter()
                .map(|(directory, command, output)| ((directory, command.to_owned()), output))
                .collect(),
            ran: Mutex::new(Vec::new()),
        }
    }

    fn directories(&self) -> BTreeSet<PathBuf> {
        let ran = self.ran.lock().unwrap();
        ran.iter().map(|(directory, _)| directory.clone()).collect()
    }
}

impl GitCommandRunner for ScriptedGit {
    fn run(&self, repository: &Path, command: &GitCommand) -> Result<GitOutput> {
        let first = command.arguments[0].to_string_lossy().into_owned();
        self.ran
            .lock()
            .unwrap()
            .push((repository.to_path_buf(), first.clone()));
        self.answers
            .get(&(repository.to_path_buf(), first))
            .cloned()
            .with_context(|| {
                format!(
                    "unexpected Git command in {}: {:?}",
                    repository.display(),
                    command.arguments
                )
            })
    }
}

fn answer(stdout: &str) -> GitOutput {
    GitOutput {
        status: 0,
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

fn gitlink_entries(paths: &[&str]) -> String {
    paths
        .iter()
        .map(|path| format!("160000 {} 0\t{path}\0", "b".repeat(40)))
        .collect()
}

fn registrations(paths: &[&str]) -> String {
    paths
        .iter()
        .map(|path| format!("submodule.{path}.path\n{path}\0"))
        .collect()
}

#[test]
fn only_checked_out_registered_submodules_are_inspected_and_unregistered_gitlinks_are_named() {
    let temp = tempfile::tempdir().unwrap();
    let top = temp.path().to_path_buf();
    let project = top.join("project");
    let clean = top.join("vendor/clean");
    let inner = clean.join("inner");
    let dirty = top.join("vendor/dirty");
    for checkout in [&clean, &inner, &dirty, &top.join("projectsrc/tailscale")] {
        fs::create_dir_all(checkout.join(".git")).unwrap();
    }
    fs::create_dir_all(top.join("vendor/absent")).unwrap();
    fs::write(top.join(".gitmodules"), b"").unwrap();
    fs::write(clean.join(".gitmodules"), b"").unwrap();
    let files = format!("100644 {} 0\tproject/README.md\0", "a".repeat(40));
    let git = ScriptedGit::new([
        (
            project.clone(),
            "rev-parse",
            answer(&format!("{}\n", top.display())),
        ),
        (
            top.clone(),
            "ls-files",
            answer(&format!(
                "{files}{}",
                gitlink_entries(&[
                    "vendor/clean",
                    "vendor/dirty",
                    "vendor/absent",
                    "projectsrc/tailscale",
                ])
            )),
        ),
        (
            top.clone(),
            "config",
            answer(&registrations(&[
                "vendor/clean",
                "vendor/dirty",
                "vendor/absent",
            ])),
        ),
        (clean.clone(), "status", answer("")),
        (
            clean.clone(),
            "ls-files",
            answer(&gitlink_entries(&["inner"])),
        ),
        (clean.clone(), "config", answer(&registrations(&["inner"]))),
        (inner.clone(), "status", answer("?? notes.txt\n")),
        (inner.clone(), "ls-files", answer("")),
        (dirty.clone(), "status", answer(" M lib.rs\n")),
        (dirty.clone(), "ls-files", answer("")),
    ]);

    let inspection = inspect_submodules(&git, &project).unwrap();

    assert_eq!(
        inspection,
        SubmoduleInspection {
            dirty: vec!["vendor/clean/inner".into(), "vendor/dirty".into()],
            unregistered: vec!["projectsrc/tailscale".into()],
        }
    );
    assert_eq!(
        git.directories(),
        BTreeSet::from([project.clone(), top, clean, inner, dirty]),
        "Git never runs in an unregistered gitlink or a submodule that is not checked out"
    );
    let error = reject_dirty_submodules(&git, &project).unwrap_err();
    assert_eq!(
        error.to_string(),
        "submodules vendor/clean/inner, vendor/dirty have uncommitted changes; commit or stash them, then try again"
    );
}

#[test]
fn a_git_failure_while_inspecting_submodules_keeps_its_stderr() {
    let temp = tempfile::tempdir().unwrap();
    let top = temp.path().to_path_buf();
    let git = ScriptedGit::new([
        (
            top.clone(),
            "rev-parse",
            answer(&format!("{}\n", top.display())),
        ),
        (
            top.clone(),
            "ls-files",
            GitOutput {
                status: 128,
                stdout: Vec::new(),
                stderr: b"fatal: index file corrupt\n".to_vec(),
            },
        ),
    ]);

    let error = reject_dirty_submodules(&git, &top).unwrap_err();

    assert_eq!(
        error.to_string(),
        "failed to inspect submodules: fatal: index file corrupt"
    );
}

fn init_repository(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q"]);
    git(path, &["config", "user.email", "hel@example.test"]);
    git(path, &["config", "user.name", "Hel Test"]);
}

/// R18: a checkout that records a nested repository as a bare gitlink, with
/// no `.gitmodules` entry, could not be checkpointed at all, because
/// `git submodule foreach` fails on a gitlink it has no URL for. The session's
/// project directory was a subdirectory of that checkout.
#[test]
fn a_real_checkout_refuses_a_dirty_submodule_but_not_an_unregistered_gitlink() {
    let temp = tempfile::tempdir().unwrap();
    let top = temp.path().join("checkout");
    let project = top.join("project");
    init_repository(&top);
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("README.md"), b"project").unwrap();
    git(&top, &["add", "."]);
    git(&top, &["commit", "-q", "-m", "base"]);

    let nested = top.join("projectsrc/tailscale");
    init_repository(&nested);
    git(&nested, &["commit", "-q", "--allow-empty", "-m", "nested"]);
    let nested_head = git_line(&nested, &["rev-parse", "HEAD"]);
    git(
        &top,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{nested_head},projectsrc/tailscale"),
        ],
    );
    git(
        &top,
        &["commit", "-q", "-m", "record the nested repository"],
    );
    // Work inside an unregistered gitlink never blocks the checkpoint.
    fs::write(nested.join("scratch.txt"), b"nested work").unwrap();

    let library = temp.path().join("library");
    init_repository(&library);
    fs::write(library.join("lib.txt"), b"base").unwrap();
    git(&library, &["add", "."]);
    git(&library, &["commit", "-q", "-m", "library"]);
    git(
        &top,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &library.to_string_lossy(),
            "vendor/library",
        ],
    );
    git(&top, &["commit", "-q", "-m", "add the library"]);
    fs::write(top.join("vendor/library/lib.txt"), b"changed").unwrap();

    assert_eq!(
        inspect_submodules(&SystemGit, &project).unwrap(),
        SubmoduleInspection {
            dirty: vec!["vendor/library".into()],
            unregistered: vec!["projectsrc/tailscale".into()],
        }
    );
    let error = reject_dirty_submodules(&SystemGit, &project).unwrap_err();
    assert_eq!(
        error.to_string(),
        "submodule vendor/library has uncommitted changes; commit or stash them, then try again"
    );

    git(&top.join("vendor/library"), &["checkout", "--", "lib.txt"]);
    reject_dirty_submodules(&SystemGit, &project)
        .expect("an unregistered gitlink never blocks the checkpoint");
}
