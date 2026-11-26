//! Component-aware relocation of connection files during host migration.

use std::path::{Path, PathBuf};

pub fn relocate_ec2_path(path: &mut PathBuf, old_config: &Path, new_config: &Path) {
    if let Ok(relative) = path.strip_prefix(old_config.join("ec2")) {
        *path = new_config.join("ec2").join(relative);
    }
}

pub fn relocate_ssh_identity_path(
    path: &mut PathBuf,
    old_config: &Path,
    new_config: &Path,
    old_home: &Path,
    new_home: &Path,
) {
    if path.starts_with(old_config.join("ec2")) {
        relocate_ec2_path(path, old_config, new_config);
    } else if !path.starts_with(old_config)
        && let Ok(relative) = path.strip_prefix(old_home)
    {
        *path = new_home.join(relative);
    }
}

fn for_each_ssh_path(args: &mut [String], mut visit: impl FnMut(&mut PathBuf, bool)) {
    let mut identity_next = false;
    for arg in args {
        let (prefix, identity) = if identity_next {
            ("", true)
        } else if arg == "-i" {
            identity_next = true;
            continue;
        } else if arg.starts_with("-i") {
            ("-i", true)
        } else if arg.starts_with("UserKnownHostsFile=") {
            ("UserKnownHostsFile=", false)
        } else if arg.starts_with("-oUserKnownHostsFile=") {
            ("-oUserKnownHostsFile=", false)
        } else {
            continue;
        };
        identity_next = false;
        let mut path = PathBuf::from(&arg[prefix.len()..]);
        let previous = path.clone();
        visit(&mut path, identity);
        if path != previous {
            *arg = format!("{prefix}{}", path.to_string_lossy());
        }
    }
}

pub fn extract_ssh_identity_arg_paths(args: &[String]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for_each_ssh_path(&mut args.to_vec(), |path, identity| {
        if identity {
            paths.push(path.clone());
        }
    });
    paths
}

pub fn relocate_ec2_ssh_args(args: &mut [String], old_config: &Path, new_config: &Path) {
    for_each_ssh_path(args, |path, _| {
        relocate_ec2_path(path, old_config, new_config)
    });
}

pub fn relocate_ssh_args(
    args: &mut [String],
    old_config: &Path,
    new_config: &Path,
    old_home: &Path,
    new_home: &Path,
) {
    for_each_ssh_path(args, |path, identity| {
        if identity {
            relocate_ssh_identity_path(path, old_config, new_config, old_home, new_home);
        } else {
            relocate_ec2_path(path, old_config, new_config);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_paths_relocate_identities_without_moving_unrelated_known_hosts() {
        let mut args = vec![
            "-i".into(),
            "/old/.ssh/key".into(),
            "-i/old/.ssh/second".into(),
            "-i~/.ssh/tilde".into(),
            "UserKnownHostsFile=/old/.ssh/known_hosts".into(),
            "-oUserKnownHostsFile=/old/config/ec2/host/known_hosts".into(),
        ];
        assert_eq!(
            extract_ssh_identity_arg_paths(&args),
            [
                PathBuf::from("/old/.ssh/key"),
                PathBuf::from("/old/.ssh/second"),
                PathBuf::from("~/.ssh/tilde")
            ]
        );
        relocate_ssh_args(
            &mut args,
            Path::new("/old/config"),
            Path::new("/new/config"),
            Path::new("/old"),
            Path::new("/new"),
        );
        assert_eq!(
            args,
            [
                "-i",
                "/new/.ssh/key",
                "-i/new/.ssh/second",
                "-i~/.ssh/tilde",
                "UserKnownHostsFile=/old/.ssh/known_hosts",
                "-oUserKnownHostsFile=/new/config/ec2/host/known_hosts"
            ]
        );
        let mut config_key = PathBuf::from("/old/config/private/key");
        relocate_ssh_identity_path(
            &mut config_key,
            Path::new("/old/config"),
            Path::new("/new/config"),
            Path::new("/old"),
            Path::new("/new"),
        );
        assert_eq!(config_key, Path::new("/old/config/private/key"));
    }
}
