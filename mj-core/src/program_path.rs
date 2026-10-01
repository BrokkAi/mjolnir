//! Finding a program on a PATH value the way the operating system would run it.

use std::ffi::OsStr;
use std::path::PathBuf;

/// Extensions Windows tries when `PATHEXT` is unset.
#[cfg(any(windows, test))]
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// File names to try in each PATH directory, in order, for `program` on a
/// system whose executable extensions are listed in `pathext` (a
/// `;`-separated list such as `.COM;.EXE`). A name that already carries an
/// extension is tried as written first, as `CreateProcess` does.
#[cfg(any(windows, test))]
fn names_with_extensions(program: &str, pathext: &str) -> Vec<String> {
    let mut names = Vec::new();
    if std::path::Path::new(program).extension().is_some() {
        names.push(program.to_owned());
    }
    for extension in pathext.split(';').map(str::trim).filter(|e| !e.is_empty()) {
        let extension = extension.trim_start_matches('.');
        names.push(format!("{program}.{extension}"));
    }
    names
}

#[cfg(windows)]
fn candidate_names(program: &str) -> Vec<String> {
    let pathext = std::env::var("PATHEXT").unwrap_or_default();
    let pathext = if pathext.trim().is_empty() {
        DEFAULT_PATHEXT
    } else {
        &pathext
    };
    names_with_extensions(program, pathext)
}

#[cfg(not(windows))]
fn candidate_names(program: &str) -> Vec<String> {
    vec![program.to_owned()]
}

/// The first file named `program` (with the platform's executable
/// extensions) in a directory of `path`, a PATH value. A missing PATH finds
/// nothing. On Unix this does not check the execute bit.
pub fn find_program_on_path(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    let names = candidate_names(program);
    std::env::split_paths(path).find_map(|directory| {
        names
            .iter()
            .map(|name| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathext_extensions_are_appended_in_order() {
        assert_eq!(
            names_with_extensions("docker", ".COM;.EXE; .cmd ;;"),
            ["docker.COM", "docker.EXE", "docker.cmd"]
        );
    }

    #[test]
    fn a_name_with_an_extension_is_tried_as_written_first() {
        assert_eq!(
            names_with_extensions("tool.exe", ".EXE;.BAT"),
            ["tool.exe", "tool.exe.EXE", "tool.exe.BAT"]
        );
    }

    #[test]
    fn default_pathext_covers_exe_and_cmd() {
        let names = names_with_extensions("podman", DEFAULT_PATHEXT);
        assert!(names.contains(&"podman.EXE".to_owned()));
        assert!(names.contains(&"podman.CMD".to_owned()));
    }

    #[test]
    fn a_missing_path_finds_nothing() {
        assert_eq!(find_program_on_path("docker", None), None);
    }

    #[cfg(windows)]
    #[test]
    fn finds_an_exe_by_its_bare_name() {
        let directory = tempfile::tempdir().unwrap();
        let exe = directory.path().join("docker.exe");
        std::fs::write(&exe, b"").unwrap();
        let path = std::env::join_paths([directory.path()]).unwrap();
        assert_eq!(find_program_on_path("docker", Some(&path)), Some(exe));
        assert_eq!(find_program_on_path("podman", Some(&path)), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn finds_a_file_by_its_exact_name() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("docker");
        std::fs::write(&file, b"").unwrap();
        let path = std::env::join_paths([directory.path()]).unwrap();
        assert_eq!(find_program_on_path("docker", Some(&path)), Some(file));
    }
}
