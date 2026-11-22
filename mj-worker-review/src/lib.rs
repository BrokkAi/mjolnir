//! Target-owned reviewer services.

#![cfg(unix)]

#[cfg(test)]
mj_core::enable_test_fixtures_on_startup!();

pub mod capture;
pub mod hook;

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    /// Run Git in the test repository and return its trimmed standard output.
    pub(crate) fn git(repository: &Path, arguments: &[&str]) -> String {
        let mut command = Command::new("git");
        command.args(arguments).current_dir(repository);
        let output = mj_core::subprocess::run_with_input(&mut command, &[]).unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}
