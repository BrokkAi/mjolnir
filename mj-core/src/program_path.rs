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

/// Whether `path` is a file this process could run: on Unix, one with an
/// execute bit set.
pub fn is_executable_file(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// The program a spawn of `program` with `path` as its PATH would run: like
/// [`find_program_on_path`], but skipping files that are not executable, as
/// the system's own search does.
pub fn find_executable_on_path(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    let names = candidate_names(program);
    std::env::split_paths(path).find_map(|directory| {
        names
            .iter()
            .map(|name| directory.join(name))
            .find(|candidate| is_executable_file(candidate))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: 3bf8fb93: Windows could not find docker.exe or podman.exe because PATH lookup ignored PATHEXT.
    #[test]
    fn pathext_extensions_are_appended_in_order() {
        assert_eq!(
            names_with_extensions("docker", ".COM;.EXE; .cmd ;;"),
            ["docker.COM", "docker.EXE", "docker.cmd"]
        );
        let defaults = names_with_extensions("podman", DEFAULT_PATHEXT);
        assert!(defaults.contains(&"podman.EXE".to_owned()));
        assert!(defaults.contains(&"podman.CMD".to_owned()));
    }

    // Hard-won: 3bf8fb93: Windows PATHEXT lookup reported installed Docker and Podman executables missing.
    #[cfg(windows)]
    #[test]
    fn finds_an_exe_by_its_bare_name() {
        let directory = tempfile::tempdir().unwrap();
        let exe = directory.path().join("docker.exe");
        std::fs::write(&exe, b"").unwrap();
        let path = std::env::join_paths([directory.path()]).unwrap();
        // NTFS names are case-insensitive; PATHEXT supplies `.EXE`.
        let found = find_program_on_path("docker", Some(&path)).unwrap();
        assert!(
            found.as_os_str().eq_ignore_ascii_case(exe.as_os_str()),
            "{found:?}"
        );
        assert_eq!(find_program_on_path("podman", Some(&path)), None);
    }

    #[cfg(unix)]
    #[test]
    fn executable_search_skips_files_without_an_execute_bit() {
        use std::os::unix::fs::PermissionsExt;
        let plain = tempfile::tempdir().unwrap();
        let runnable = tempfile::tempdir().unwrap();
        std::fs::write(plain.path().join("tool"), b"").unwrap();
        let program = runnable.path().join("tool");
        std::fs::write(&program, b"").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let only_plain = std::env::join_paths([plain.path()]).unwrap();
        assert!(find_program_on_path("tool", Some(&only_plain)).is_some());
        assert_eq!(find_executable_on_path("tool", Some(&only_plain)), None);

        let both = std::env::join_paths([plain.path(), runnable.path()]).unwrap();
        assert_eq!(find_executable_on_path("tool", Some(&both)), Some(program));
    }
}
