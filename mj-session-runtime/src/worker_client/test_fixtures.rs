//! Fixtures shared by the worker client tests here and in mj-daemon, which reaches
//! them through the `test-support` feature.

#[cfg(any(test, unix))]
use super::*;

pub const SESSION_ID: &str = "018f9dd2-a3b4-7c8d-9000-123456789abc";

#[cfg(unix)]
pub fn skills_sync_target(profile_home: &std::path::Path) -> CredentialSyncTarget {
    CredentialSyncTarget {
        session_id: SESSION_ID.into(),
        profile_id: "work".into(),
        harness: mj_core::config::HarnessKind::Claude,
        profile_home: profile_home.to_path_buf(),
        aws_credentials: None,
        skips_login_file_sync: false,
        sync_github_token: false,
        github: Default::default(),
        skills_scope: mj_core::skills::SkillsScope::Localhost,
        spec: CommandSpec::new("sh", ["-c", "exit 1"]),
    }
}

/// Plays a worker that keeps, in the directory named by its third argument,
/// the AWS credentials, GitHub token and skills it is sent, and starts with a
/// stale skills tree. Any request it does not expect fails the exchange.
#[cfg(unix)]
pub const AWS_SYNC_RELAY: &str = r#"
import base64, gzip, hashlib, json, os, sys, time
protocol, session, root = int(sys.argv[1]), sys.argv[2], sys.argv[3]
# A slow worker: its first AWS answer waits for this file, or fails the exchange.
wait_for = sys.argv[4] if len(sys.argv) > 4 else ""
# A slow login step: the first skills request announces itself and waits for this file.
login_hold = sys.argv[5] if len(sys.argv) > 5 else ""
def path(name): return os.path.join(root, name)
def hold():
    # The first AWS request of a slow worker announces itself and then waits.
    if not wait_for: return
    open(path("waiting"), "w").close()
    tries = 0
    while not os.path.exists(wait_for):
        tries += 1
        if tries > 200: sys.exit(1)
        time.sleep(0.05)
def state(name):
    # Provider files missing or different: the worker reports the credentials
    # absent although the secret file may still be there.
    if name == "aws" and os.path.exists(path("provider-files-broken")):
        return {"present": False, "fingerprint": "", "freshness_epoch_ms": None}
    if not os.path.exists(path(name)):
        return {"present": False, "fingerprint": "", "freshness_epoch_ms": None}
    data = open(path(name), "rb").read()
    return {"present": True, "fingerprint": hashlib.sha256(data).hexdigest(), "freshness_epoch_ms": None}
for line in sys.stdin:
    req = json.loads(line)
    method = req["request"]["method"]
    params = req["request"].get("params", {})
    if method == "hello":
        payload = {"type": "hello", "data": {"negotiated": protocol, "relay_version": "aws-sync-fixture", "session_id": session}}
    elif method == "aws_credentials_state":
        hold()
        payload = {"type": "credential_state", "data": state("aws")}
    elif method == "install_aws_credentials":
        hold()
        if os.path.exists(path("install-fails")): sys.exit(1)
        secret = base64.b64decode(params["data"])
        open(path("aws"), "wb").write(secret)
        # The scope record sits beside the secret, bound to it by fingerprint.
        if params.get("scope"):
            json.dump({"scope": params["scope"], "secret_fingerprint": hashlib.sha256(secret).hexdigest()}, open(path("aws-scope.json"), "w"))
        elif os.path.exists(path("aws-scope.json")):
            os.remove(path("aws-scope.json"))
        # The staged AWS configuration names the profile; withdrawing the
        # credentials leaves it.
        open(path("aws-config"), "w").write("[profile " + json.loads(secret)["Profile"] + "]\ncredential_process = cat aws\n")
        payload = {"type": "credential_state", "data": state("aws")}
    elif method == "aws_credentials_scope":
        hold()
        scope = None
        if os.path.exists(path("aws-scope.json")) and os.path.exists(path("aws")):
            stored = json.load(open(path("aws-scope.json")))
            if stored["secret_fingerprint"] == hashlib.sha256(open(path("aws"), "rb").read()).hexdigest():
                scope = stored["scope"]
        profile = None
        if os.path.exists(path("aws-config")):
            for line in open(path("aws-config")).read().splitlines():
                if line.startswith("[profile ") and line.endswith("]"):
                    profile = line[len("[profile "):-1]
                    break
        payload = {"type": "aws_credentials_scope", "data": {"scope": scope, "profile": profile}}
    elif method == "remove_aws_credentials":
        if os.path.exists(path("remove-fails")): sys.exit(1)
        for name in ("aws", "aws-scope.json"):
            if os.path.exists(path(name)): os.remove(path(name))
        payload = {"type": "credential_state", "data": state("aws")}
    elif method == "github_token_state":
        s = state("github")
        payload = {"type": "github_token_state", "data": {"present": s["present"], "fingerprint": s["fingerprint"]}}
    elif method == "install_github_token":
        open(path("github"), "wb").write(base64.b64decode(params["data"]))
        s = state("github")
        payload = {"type": "github_token_state", "data": {"present": True, "fingerprint": s["fingerprint"]}}
    elif method == "skills_state":
        if login_hold:
            open(path("login-holding"), "w").close()
            tries = 0
            while not os.path.exists(login_hold):
                tries += 1
                if tries > 200: sys.exit(1)
                time.sleep(0.05)
        payload = {"type": "skills_state", "data": {"present": True, "fingerprint": open(path("skills")).read() if os.path.exists(path("skills")) else "stale"}}
    elif method == "install_skills":
        archive = base64.b64decode(params["data"])
        body = archive[8:]
        if archive[:8] == b"HELSKIL2":
            body = gzip.decompress(body)
        fingerprint = hashlib.sha256(b"HELSKIL1" + body).hexdigest()
        open(path("skills"), "w").write(fingerprint)
        payload = {"type": "skills_state", "data": {"present": True, "fingerprint": fingerprint}}
    else:
        raise AssertionError(method)
    print(json.dumps({"request_id": req["request_id"], "protocol_version": protocol, "result": "ok", "payload": payload}), flush=True)
"#;

/// A Claude session on [`AWS_SYNC_RELAY`] whose profile names a role, whose
/// GitHub token comes from `token_file` and whose skills come from `home`.
#[cfg(unix)]
pub fn aws_sync_session(
    home: &std::path::Path,
    root: &std::path::Path,
    token_file: &std::path::Path,
    aws_cli: &std::path::Path,
) -> CredentialSyncTarget {
    aws_sync_session_named(home, root, token_file, aws_cli, SESSION_ID, None)
}

/// [`aws_sync_session`] for a named session, whose worker may wait for a
/// file before answering about its AWS credentials.
#[cfg(unix)]
pub fn aws_sync_session_named(
    home: &std::path::Path,
    root: &std::path::Path,
    token_file: &std::path::Path,
    aws_cli: &std::path::Path,
    session_id: &str,
    worker_waits_for: Option<&std::path::Path>,
) -> CredentialSyncTarget {
    std::fs::create_dir_all(home.join("skills/review")).unwrap();
    std::fs::write(home.join("skills/review/SKILL.md"), "review").unwrap();
    // Staged, as launch does, under the profile the host environment names.
    std::fs::write(
        root.join("aws-config"),
        "[profile bedrock]\ncredential_process = cat aws\n",
    )
    .unwrap();
    let mut target = skills_sync_target(home);
    target.session_id = session_id.into();
    target.skips_login_file_sync = true;
    target.sync_github_token = true;
    target.github = mj_core::config::GithubConfig {
        session_token_path: Some(token_file.to_path_buf()),
        ..Default::default()
    };
    target.skills_scope = mj_core::skills::SkillsScope::Isolated;
    target.spec = CommandSpec::new(
        "python3",
        [
            "-u".to_owned(),
            "-c".to_owned(),
            AWS_SYNC_RELAY.to_owned(),
            RELAY_PROTOCOL_VERSION.to_string(),
            session_id.to_owned(),
            root.to_string_lossy().into_owned(),
        ]
        .into_iter()
        .chain(worker_waits_for.map(|path| path.to_string_lossy().into_owned()))
        .collect::<Vec<_>>(),
    )
    .purpose("AWS sync test relay");
    target.aws_credentials = Some(mj_core::aws_credentials::AwsCredentialSource {
        environment: BTreeMap::from([
            ("PATH".into(), aws_cli.to_string_lossy().into_owned()),
            ("AWS_PROFILE".into(), "bedrock".into()),
        ]),
        role: Some(role_config()),
        mints: true,
        selection: None,
    });
    target
}

/// The role the AWS sync tests configure.
pub fn role_config() -> mj_core::config::AwsRoleConfig {
    mj_core::config::AwsRoleConfig {
        role_arn: "arn:aws:iam::123456789012:role/mj-bedrock".into(),
        session_policy: None,
        session_policy_arns: Vec::new(),
        duration_seconds: None,
        source_identity: false,
    }
}

/// An `aws` that answers every `sts assume-role` with the recorded output,
/// expiring far in the future.
#[cfg(unix)]
pub fn granting_aws_cli(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("granting-bin");
    std::fs::create_dir(&bin).unwrap();
    let mut recorded: serde_json::Value =
        serde_json::from_str(crate::aws_assume_role::test_fixtures::ASSUME_ROLE).unwrap();
    recorded["Credentials"]["Expiration"] = "2099-01-01T00:00:00+00:00".into();
    let output = dir.join("granted.json");
    std::fs::write(&output, serde_json::to_vec(&recorded).unwrap()).unwrap();
    let aws = bin.join("aws");
    std::fs::write(
        &aws,
        format!("#!/bin/sh\nexec /bin/cat {}\n", output.display()),
    )
    .unwrap();
    std::fs::set_permissions(&aws, std::fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

#[cfg(test)]
pub(crate) fn duplex_relay_client(
    controller: tokio::io::DuplexStream,
    session_id: &str,
) -> RelayClient {
    let (output, input) = tokio::io::split(controller);
    RelayClient {
        child: None,
        input: Some(Box::new(input)),
        output: BufReader::new(Box::new(output)),
        request_timeout: RELAY_RPC_TIMEOUT,
        abandoned: None,
        next_request: 1,
        connection_nonce: 1,
        protocol_version: RELAY_PROTOCOL_VERSION,
        session_id: session_id.into(),
        relay_version: "duplex-test-peer".into(),
        worker_build: None,
        latest_ordinal: 0,
        latest_digest: RELAY_EVENT_GENESIS_DIGEST.into(),
        subagent_acknowledged: None,
        subagent_queue: BTreeMap::new(),
        subagent_publication: None,
        known_state: None,
        handshake_complete: true,
        ssh_session: None,
        adoption_wait_progress: None,
    }
}
