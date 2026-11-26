//! Component-aware relocation of EC2 connection files during host migration.

use std::path::{Path, PathBuf};

pub fn relocate_ec2_path(path: &mut PathBuf, old_config: &Path, new_config: &Path) {
    if let Ok(relative) = path.strip_prefix(old_config.join("ec2")) {
        *path = new_config.join("ec2").join(relative);
    }
}

pub fn relocate_ec2_ssh_args(args: &mut [String], old_config: &Path, new_config: &Path) {
    let mut identity_next = false;
    for arg in args {
        let prefix = if identity_next {
            ""
        } else if arg == "-i" {
            identity_next = true;
            continue;
        } else if arg.starts_with("-i") {
            "-i"
        } else if arg.starts_with("UserKnownHostsFile=") {
            "UserKnownHostsFile="
        } else if arg.starts_with("-oUserKnownHostsFile=") {
            "-oUserKnownHostsFile="
        } else {
            continue;
        };
        identity_next = false;
        let mut path = PathBuf::from(&arg[prefix.len()..]);
        let previous = path.clone();
        relocate_ec2_path(&mut path, old_config, new_config);
        if path != previous {
            *arg = format!("{prefix}{}", path.to_string_lossy());
        }
    }
}
