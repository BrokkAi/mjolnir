#![cfg(unix)]
//! `mj login` must target only the selected profile's home for every harness
//! and never clobber the user's default harness home, including when ACP login
//! falls back to terminal sign-in in a headless session.

use std::path::Path;
use std::process::Command;

use mj_core::config::HarnessKind;
use mj_core::harness_runtime::ANTIGRAVITY_VERSION;

fn isolated_login_command(
    root: &Path,
    config_dir: &Path,
    data_dir: &Path,
    tools: &Path,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    command.env("MJ_DAEMON_OWNER_PID", std::process::id().to_string());
    command
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("XDG_DATA_HOME", root.join("xdg-data"))
        .env("MJ_CONFIG_DIR", config_dir)
        .env("MJ_DATA_DIR", data_dir)
        .env("MJ_DAEMON_CONTAINER", "login-isolation-test")
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
        .env_remove("MJ_INSTANCE")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("KIMI_CODE_HOME")
        .env_remove("GROK_HOME")
        .env_remove("DSH_ACP_DATA_DIR")
        .env_remove("MJ_ANTIGRAVITY_HOME");
    command
}

fn default_home_for(root: &Path, kind: HarnessKind) -> std::path::PathBuf {
    if kind.nested_home() {
        root.join("xdg-config").join(kind.id())
    } else {
        root.join(kind.default_home_leaf())
    }
}

fn profile_home_for(root: &Path, kind: HarnessKind) -> std::path::PathBuf {
    let base = root.join("profiles").join(kind.id());
    if kind.nested_home() {
        base.join(kind.id())
    } else {
        base
    }
}

#[test]
fn cli_login_targets_only_the_selected_profile_home_for_every_harness() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("mj-config");
    let data_dir = root.path().join("mj-data");
    let tools = root.path().join("tools");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&tools).unwrap();

    // Seed every harness's default home with a sentinel credential so any
    // write to the default home is detected immediately.
    let mut config_toml = String::from("version = 23\n\n");
    for kind in HarnessKind::ALL {
        let default_home = default_home_for(root.path(), kind);
        let default_marker = default_home.join(kind.credential_file_name());
        std::fs::create_dir_all(default_marker.parent().unwrap()).unwrap();
        std::fs::write(&default_marker, b"default-account").unwrap();

        let profile_home = profile_home_for(root.path(), kind);
        std::fs::create_dir_all(&profile_home).unwrap();
        let kind_toml = serde_json::to_string(&kind).unwrap();
        config_toml.push_str(&format!(
            "[profiles.p-{}]\nkind = {}\nhome = \"{}\"\n\n",
            kind.id(),
            kind_toml,
            profile_home.display()
        ));
    }
    std::fs::write(config_dir.join("config.toml"), config_toml).unwrap();

    // Install fake harness executables that write a credential to whichever
    // home directory the harness's own mechanism selects.
    mj_core::test_hooks::install_fake_command(
        &tools,
        "codex",
        "#!/bin/sh\ntarget=\"${CODEX_HOME:-$HOME/.codex}\"\nmkdir -p \"$target\"\nprintf profile-account > \"$target/auth.json\"\n",
    );
    mj_core::test_hooks::install_fake_command(
        &tools,
        "claude",
        "#!/bin/sh\ntarget=\"${CLAUDE_CONFIG_DIR:-$HOME/.claude}\"\nmkdir -p \"$target\"\nprintf profile-account > \"$target/.credentials.json\"\n",
    );
    mj_core::test_hooks::install_fake_command(
        &tools,
        "kimi",
        "#!/bin/sh\ntarget=\"${KIMI_CODE_HOME:-$HOME/.kimi-code}\"\nmkdir -p \"$target/credentials\"\nprintf profile-account > \"$target/credentials/kimi-code.json\"\n",
    );
    mj_core::test_hooks::install_fake_command(
        &tools,
        "grok",
        "#!/bin/sh\ntarget=\"${GROK_HOME:-$HOME/.grok}\"\nmkdir -p \"$target\"\nprintf profile-account > \"$target/auth.json\"\n",
    );
    mj_core::test_hooks::install_fake_command(
        &tools,
        "muse",
        "#!/bin/sh\ntarget=\"${XDG_CONFIG_HOME:-$HOME/.config}/muse\"\nmkdir -p \"$target\"\nprintf profile-account > \"$target/auth.json\"\n",
    );
    mj_core::test_hooks::install_fake_command(
        &tools,
        "opencode",
        "#!/bin/sh\nmkdir -p \"${XDG_CONFIG_HOME:-$HOME/.config}/opencode\"\nprintf '%s\\n' \"$XDG_CONFIG_HOME|$XDG_DATA_HOME\" > \"${XDG_CONFIG_HOME:-$HOME/.config}/opencode/recorded-env\"\n",
    );
    // `locate_in_image` canonicalizes `agy`, which would follow a symlink to
    // `fake-command.sh`; write a regular executable file before spawning `mj`.
    {
        use std::os::unix::fs::PermissionsExt;
        let agy = tools.join("agy");
        std::fs::write(
            &agy,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{ANTIGRAVITY_VERSION}\"; exit 0; fi\ntarget=\"$HOME/.gemini\"\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in --gemini-dir) shift; target=\"$1\" ;; esac\n  shift\ndone\nmkdir -p \"$target/antigravity-cli\"\nprintf profile-account > \"$target/antigravity-cli/antigravity-oauth-token\"\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    mj_core::test_hooks::install_fake_command(
        &tools,
        "dsh",
        "#!/bin/sh\necho \"dsh should never be invoked by login\" >&2\nexit 99\n",
    );

    for kind in HarnessKind::ALL {
        // A harness that keeps its login outside its home has no login per
        // home to isolate: on a Mac every Claude profile signs in to the one
        // Keychain item, with its home variable unset (#1491).
        if !kind.keeps_login_in_home(mj_core::config::HarnessHost::current()) {
            continue;
        }
        let profile_id = format!("p-{}", kind.id());
        let output = isolated_login_command(root.path(), &config_dir, &data_dir, &tools)
            .args([
                "--instance",
                "login-isolation",
                "login",
                "--profile",
                &profile_id,
                "--cli",
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();

        if kind == HarnessKind::Dsh {
            assert!(
                !output.status.success(),
                "dsh login should refuse without invoking dsh"
            );
            continue;
        }

        assert!(
            output.status.success(),
            "login --cli failed for {}: stdout={} stderr={}",
            kind.id(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let profile_home = profile_home_for(root.path(), kind);
        if kind == HarnessKind::OpenCode {
            let recorded = std::fs::read_to_string(profile_home.join("recorded-env")).unwrap();
            let expected = format!(
                "{}|{}\n",
                profile_home.parent().unwrap().display(),
                profile_home.join(".data").display()
            );
            assert_eq!(recorded, expected);
        } else {
            let marker = profile_home.join(kind.credential_file_name());
            assert_eq!(
                std::fs::read(&marker).unwrap(),
                b"profile-account",
                "profile login did not write to {} for {}",
                marker.display(),
                kind.id()
            );
        }

        let default_marker = default_home_for(root.path(), kind).join(kind.credential_file_name());
        assert_eq!(
            std::fs::read(&default_marker).unwrap(),
            b"default-account",
            "default home was clobbered for {}",
            kind.id()
        );
    }
}

#[test]
fn headless_antigravity_acp_login_falls_back_to_profile_scoped_terminal_login() {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("mj-config");
    let data_dir = root.path().join("mj-data");
    let tools = root.path().join("tools");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&tools).unwrap();

    let default_home = default_home_for(root.path(), HarnessKind::Antigravity);
    let default_marker = default_home.join(HarnessKind::Antigravity.credential_file_name());
    std::fs::create_dir_all(default_marker.parent().unwrap()).unwrap();
    std::fs::write(&default_marker, b"default-account").unwrap();

    let profile_home = profile_home_for(root.path(), HarnessKind::Antigravity);
    std::fs::create_dir_all(&profile_home).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "version = 23\n\n[profiles.gemini2]\nkind = \"antigravity\"\nhome = \"{}\"\n",
            profile_home.display()
        ),
    )
    .unwrap();

    // Fake `agy` (regular executable so `find_command` canonicalization keeps its path) that:
    // - prints the pinned version on `--version`
    // - in `acp --login`: writes the token to `--gemini-dir` (or `$HOME/.gemini`)
    // - in `acp` (no `--login`): rejects `authenticate` with agy's SSH no-browser error
    {
        use std::os::unix::fs::PermissionsExt;
        let agy = tools.join("agy");
        std::fs::write(
            &agy,
            format!(
                r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    echo "{ANTIGRAVITY_VERSION}"
    exit 0
fi
login_mode=0
target="$HOME/.gemini"
while [ "$#" -gt 0 ]; do
    case "$1" in
        --login) login_mode=1 ;;
        --gemini-dir) shift; target="$1" ;;
    esac
    shift
done
if [ "$login_mode" -eq 1 ]; then
    mkdir -p "$target/antigravity-cli"
    printf profile-account > "$target/antigravity-cli/antigravity-oauth-token"
    exit 0
fi
while IFS= read -r line; do
    case "$line" in
        *'"method":"initialize"'*)
            printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":1,"authMethods":[{{"id":"google-oauth","name":"Sign in with Google"}}]}}}}'
            ;;
        *'"method":"authenticate"'*)
            printf '%s\n' '{{"jsonrpc":"2.0","id":2,"error":{{"code":-32603,"message":"No browser is available here (this is a remote (SSH) session). Run `agy acp --login` in a terminal on this machine, then retry; or use a Gemini API key."}}}}'
            ;;
    esac
done
"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let output = isolated_login_command(root.path(), &config_dir, &data_dir, &tools)
        .args([
            "--instance",
            "headless-agy",
            "login",
            "--profile",
            "gemini2",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "headless antigravity login failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(profile_home.join(HarnessKind::Antigravity.credential_file_name()))
            .unwrap_or_else(|e| panic!(
                "missing profile token ({e}): stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )),
        b"profile-account"
    );
    assert_eq!(std::fs::read(&default_marker).unwrap(), b"default-account");
}
