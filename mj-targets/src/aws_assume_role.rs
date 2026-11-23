//! Cache host AWS exports and session-scoped STS credentials.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use hmac::Hmac;
use mj_core::aws_credentials::{AwsCredentialScope, AwsCredentialSource, AwsCredentials};
use mj_core::config::AwsRoleConfig;
use mj_core::targets::{CommandExecutor, CommandSpec};

type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// Single-flight cache of AWS credentials shared by launch and periodic sync.
pub struct AwsCredentialCache {
    now: Clock,
    entries: Mutex<BTreeMap<CacheKey, Arc<Mutex<Option<CacheEntry>>>>>,
}

/// What makes two requests the same credentials: the session (it is the role
/// session name), the role and policies, and the environment the AWS CLI runs
/// in, because that selects the host identity that assumes the role (the AWS
/// profile, but also `AWS_CONFIG_FILE`, `HOME` and the like). No `Debug`:
/// the environment is not for logs.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CacheKey {
    session_id: Option<String>,
    role: Option<AwsRoleConfig>,
    environment: BTreeMap<String, String>,
}

impl CacheKey {
    fn new(session_id: &str, source: &AwsCredentialSource) -> Self {
        Self {
            session_id: source.role.as_ref().map(|_| session_id.to_owned()),
            role: source.role.clone(),
            environment: source.environment.clone(),
        }
    }
}

struct CacheEntry {
    credentials: AwsCredentials,
    expires_at: Option<SystemTime>,
    refresh_at: SystemTime,
}

impl AwsCredentialCache {
    /// Drop expired credentials and static exports due for refresh. Run once
    /// per periodic pass to keep the scan linear in the number of entries.
    pub fn sweep_expired(&self) {
        let now = (self.now)();
        sweep(
            &mut self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            now,
        );
    }

    /// Read the entry cached for this session and source, when there is one
    /// and nobody is minting into it.
    fn peek<T>(
        &self,
        session_id: &str,
        source: &AwsCredentialSource,
        read: impl FnOnce(&CacheEntry, SystemTime) -> Option<T>,
    ) -> Option<T> {
        let now = (self.now)();
        let cell = Arc::clone(
            self.entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&CacheKey::new(session_id, source))?,
        );
        let entry = cell.try_lock().ok()?;
        read(entry.as_ref()?, now)
    }

    /// How long until the credentials cached for this session and source are
    /// no longer fresh and are due to be renewed; `None` when none are cached
    /// (or another caller is minting into the entry), or when they are due
    /// already: no wait is a reason to retry, not to renew again at once.
    pub fn renew_in(&self, session_id: &str, source: &AwsCredentialSource) -> Option<Duration> {
        self.peek(session_id, source, |entry, now| {
            entry
                .refresh_at
                .duration_since(now)
                .ok()
                .filter(|wait| !wait.is_zero())
        })
    }

    /// How long the cached credentials stay valid; `None` when none are cached
    /// or they have expired or have no expiration.
    pub fn remaining_validity(
        &self,
        session_id: &str,
        source: &AwsCredentialSource,
    ) -> Option<Duration> {
        self.peek(session_id, source, |entry, now| {
            entry
                .expires_at?
                .duration_since(now)
                .ok()
                .filter(|left| !left.is_zero())
        })
    }

    /// Drop the credentials cached for this session and source, so the next
    /// request mints new ones: for a session whose credentials may have been
    /// refused, or lost from the worker. Blocking, like `credentials_for`: it
    /// waits for a mint in progress for the key.
    pub fn forget(&self, session_id: &str, source: &AwsCredentialSource) {
        let cell = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&CacheKey::new(session_id, source))
            .map(Arc::clone);
        if let Some(cell) = cell {
            *cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    /// Whether the cache holds credentials for the session.
    #[cfg(unix)]
    #[doc(hidden)]
    pub fn holds(&self, session_id: &str) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .any(|key| key.session_id.as_deref() == Some(session_id))
    }

    pub fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<AwsCredentialCache>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| Arc::new(Self::with_clock(Arc::new(SystemTime::now)))))
    }

    fn with_clock(now: Clock) -> Self {
        Self {
            now,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Get fresh credentials, exporting once per unscoped environment or
    /// assuming the role once per session and environment. Blocking: callers
    /// for the same key wait for the first caller's refresh.
    ///
    /// A failed refresh leaves the credentials already installed on the worker
    /// until they expire or are withdrawn by sync.
    pub fn credentials_for(
        &self,
        session_id: &str,
        source: &AwsCredentialSource,
        executor: &impl CommandExecutor,
        static_refresh_interval: Duration,
    ) -> Result<AwsCredentials> {
        let cell = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(
                entries
                    .entry(CacheKey::new(session_id, source))
                    .or_default(),
            )
        };
        let mut cached = cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = cached.as_ref()
            && (self.now)() < entry.refresh_at
        {
            return Ok(entry.credentials.clone());
        }
        let minted = if let Some(role) = &source.role {
            mint_scoped(session_id, source, role, executor)?
        } else {
            crate::aws_credentials::export(source, executor)?
        };
        let minted_at = (self.now)();
        let expires_at = minted
            .expires_at_epoch_millis()
            .and_then(|millis| u64::try_from(millis).ok())
            .map(|millis| UNIX_EPOCH + Duration::from_millis(millis));
        ensure!(
            source.role.is_none() || expires_at.is_some(),
            "AWS returned credentials without an expiration"
        );
        let refresh_after = expires_at.map_or(static_refresh_interval, |expiration| {
            expiration.duration_since(minted_at).unwrap_or_default() / 2
        });
        *cached = Some(CacheEntry {
            credentials: minted.clone(),
            expires_at,
            refresh_at: minted_at + refresh_after,
        });
        Ok(minted)
    }
}

fn mint_scoped(
    session_id: &str,
    source: &AwsCredentialSource,
    role: &AwsRoleConfig,
    executor: &impl CommandExecutor,
) -> Result<AwsCredentials> {
    let host_profile = selected_profile(source);
    let name = session_name(session_id);
    let command = assume_role_command(role, host_profile, &name, &source.environment);
    // Only the call to AWS (and reading its answer) is the role's to
    // explain: its advice, including the unscoped fallback, does not
    // apply to a failure of the controller's own signing key.
    let issued = mint(&command, host_profile, executor)
        .and_then(|credentials| {
            // Recorded beside the secret on the worker, so what Mjolnir
            // issued for this session's role is recognisable later.
            credentials.issued_for(
                &role.role_arn,
                &name,
                &role.session_policy_fingerprint(),
                role.source_identity.then_some(name.as_str()),
            )
        })
        .with_context(|| {
            format!(
                "Could not assume AWS role {} for this session. The controller's AWS login (profile {host_profile}) must be allowed to assume it: give that login sts:AssumeRole on the role, and name it in the role's trust policy",
                role.role_arn
            )
        })?;
    // ...and authenticated with a key the session cannot compute.
    issued
        .sign_scope(sign_scope)
        .context("could not sign the session's AWS scope record with the controller's signing key")
}

/// Drop expired credentials, static credentials due for refresh, and empty cells.
/// A cell another caller holds (it is minting into it, or waiting to) is left
/// alone, expired or not: removing it would let that caller mint into a cell
/// the map no longer knows. The map lock is held, so nobody can newly clone a
/// cell while this counts them.
fn sweep(entries: &mut BTreeMap<CacheKey, Arc<Mutex<Option<CacheEntry>>>>, now: SystemTime) {
    entries.retain(|_, cell| {
        if Arc::strong_count(cell) > 1 {
            return true;
        }
        match cell.try_lock() {
            Ok(entry) => entry
                .as_ref()
                .is_some_and(|entry| entry.expires_at.unwrap_or(entry.refresh_at) > now),
            Err(_) => true,
        }
    });
}

fn selected_profile(source: &AwsCredentialSource) -> &str {
    source
        .environment
        .get("AWS_PROFILE")
        .map(String::as_str)
        .unwrap_or("default")
}

/// The key that authenticates scope records, derived from the controller's
/// private signing key (the phone cookie key, kept owner-only in the data
/// directory and created once) so no new secret file is needed. Rotating that
/// key, the documented sign-everyone-out gesture, also makes every record fail
/// verification, which withdraws what a failed mint would otherwise keep: the
/// safe direction.
///
/// The key is derived once per process and kept: reading it can create the
/// key file and block on its lock, so verification must not repeat that, and
/// the async path loads it on the blocking pool. A rotation therefore takes
/// effect when the daemon restarts, as it does for phone cookies.
static SCOPE_KEY: OnceLock<Vec<u8>> = OnceLock::new();
static FIXTURE_SCOPE_KEY: OnceLock<Vec<u8>> = OnceLock::new();

fn scope_key_cache() -> Result<(&'static OnceLock<Vec<u8>>, bool)> {
    let fixture = cfg!(test) || mj_core::test_hooks::test_fixtures_enabled();
    let cache = if fixture {
        // Check on the caller's thread. Blocking-pool threads do not see
        // a test's thread-local failure flag.
        anyhow::ensure!(
            !SIGNING_KEY_UNAVAILABLE.with(std::cell::Cell::get),
            "the signing key is unreadable (test)"
        );
        &FIXTURE_SCOPE_KEY
    } else {
        &SCOPE_KEY
    };
    Ok((cache, fixture))
}

/// Blocking: reads (or creates) the signing key on first use.
fn scope_key() -> Result<&'static [u8]> {
    let (cache, fixture) = scope_key_cache()?;
    load_scope_key(cache, fixture)
}

fn load_scope_key(cache: &'static OnceLock<Vec<u8>>, fixture: bool) -> Result<&'static [u8]> {
    if let Some(key) = cache.get() {
        return Ok(key);
    }
    let root = if fixture {
        b"test signing key, not a secret".to_vec()
    } else {
        crate::signing_key::load_or_create_cookie_key(&crate::signing_key::cookie_key_path())
            .context("read the controller's signing key")?
    };
    use hmac::{Hmac, KeyInit, Mac};
    let mut derive =
        Hmac::<sha2::Sha256>::new_from_slice(&root).expect("HMAC accepts any key length");
    derive.update(b"mjolnir aws credential scope key v1");
    Ok(cache.get_or_init(|| derive.finalize().into_bytes().to_vec()))
}

/// The key for code on the async runtime: the first use is loaded on the
/// blocking pool, every later one is a read of the kept key.
async fn scope_key_without_blocking() -> Result<&'static [u8]> {
    let (cache, fixture) = scope_key_cache()?;
    if let Some(key) = cache.get() {
        return Ok(key);
    }
    mj_core::runtime::spawn_blocking(move || load_scope_key(cache, fixture))
        .await
        .context("signing key load task stopped")?
}

// Lets a test make the signing key unreadable on the thread that sets this.
thread_local! {
    #[doc(hidden)]
    pub static SIGNING_KEY_UNAVAILABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn scope_mac(
    key: &[u8],
    scope: &AwsCredentialScope,
    secret_fingerprint: &str,
) -> Hmac<sha2::Sha256> {
    use hmac::{KeyInit, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&scope.signed_payload(secret_fingerprint));
    mac
}

/// The tag for a scope record describing the secret with this fingerprint.
pub fn sign_scope(scope: &AwsCredentialScope, secret_fingerprint: &str) -> Result<String> {
    use hmac::Mac;
    let mac = scope_mac(scope_key()?, scope, secret_fingerprint);
    Ok(mj_core::hex::lower_hex(mac.finalize().into_bytes()))
}

/// Whether a record the worker reports was made by this controller for the
/// secret with this fingerprint. A record the session could have written
/// itself does not verify. An error means the answer is unknown (the signing
/// key could not be read), which is not the same as "not authentic": callers
/// must not act on it as if the record were forged.
pub async fn scope_is_authentic(
    scope: &AwsCredentialScope,
    secret_fingerprint: &str,
) -> Result<bool> {
    use hmac::Mac;
    let key = scope_key_without_blocking().await?;
    let Some(tag) = (0..scope.tag.len() / 2)
        .map(|index| u8::from_str_radix(scope.tag.get(index * 2..index * 2 + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()
    else {
        return Ok(false);
    };
    Ok(scope_mac(key, scope, secret_fingerprint)
        .verify_slice(&tag)
        .is_ok())
}

/// What AWS documents as the default `DurationSeconds` of `AssumeRole` when the
/// call sets none: one hour.
const STS_DEFAULT_DURATION: Duration = Duration::from_secs(3600);

/// Half the lifetime a role's credentials are granted, which is when they are
/// renewed by the cache. A renewal that keeps failing is retried
/// no more slowly than this, because slower than this the credentials would
/// run out between attempts.
pub fn credential_half_life(role: &AwsRoleConfig) -> Duration {
    role.duration_seconds
        .map_or(STS_DEFAULT_DURATION, |seconds| {
            Duration::from_secs(u64::from(seconds))
        })
        / 2
}

/// The role session name: it appears in the assumed-role ARN of every
/// CloudTrail event the session causes. A session id is 32 hex characters, so
/// this fits STS's 64-character limit.
pub fn session_name(session_id: &str) -> String {
    format!("mj-{session_id}")
}

/// `aws sts assume-role` for one session, run with the host's own login.
pub fn assume_role_command(
    role: &AwsRoleConfig,
    host_profile: &str,
    session_name: &str,
    environment: &BTreeMap<String, String>,
) -> CommandSpec {
    let mut args = vec![
        "sts".to_owned(),
        "assume-role".to_owned(),
        "--role-arn".to_owned(),
        role.role_arn.clone(),
        "--role-session-name".to_owned(),
        session_name.to_owned(),
    ];
    if let Some(policy) = role.effective_session_policy() {
        args.extend(["--policy".to_owned(), policy.to_owned()]);
    }
    if !role.session_policy_arns.is_empty() {
        args.push("--policy-arns".to_owned());
        args.extend(
            role.session_policy_arns
                .iter()
                .map(|arn| format!("arn={arn}")),
        );
    }
    if let Some(seconds) = role.duration_seconds {
        args.extend(["--duration-seconds".to_owned(), seconds.to_string()]);
    }
    if role.source_identity {
        args.extend(["--source-identity".to_owned(), session_name.to_owned()]);
    }
    args.extend([
        "--output".to_owned(),
        "json".to_owned(),
        "--profile".to_owned(),
        host_profile.to_owned(),
    ]);
    let mut command =
        CommandSpec::new("aws", args).purpose("assume the profile's AWS role for a session");
    // The output holds the new secret keys, and a failing credential helper
    // can print secrets to either stream.
    command.sensitive_output = true;
    command.env = environment.clone();
    command
        .env
        .insert("AWS_CLI_AUTO_PROMPT".into(), "off".into());
    command
}

fn mint(
    command: &CommandSpec,
    host_profile: &str,
    executor: &impl CommandExecutor,
) -> Result<AwsCredentials> {
    let output = executor
        .execute(command)
        .context("the AWS CLI could not run; install AWS CLI v2 on the controller host")?;
    ensure!(
        output.status == 0,
        "{}",
        assume_role_failure(&output.stderr, output.status)
    );
    AwsCredentials::from_assume_role(host_profile, &output.stdout)
}

/// What to tell the person about a failed call. The command's output is
/// sensitive (a custom credential helper can print secrets to either stream),
/// so only identifier-shaped fields are taken from the AWS CLI's error line,
/// `An error occurred (<Code>) when calling the <Operation> operation: ...`:
/// the error code and the operation, each checked against `[A-Za-z0-9.]+`. The
/// free-text message is dropped. A client-side range check prints no
/// operation, only `An error occurred (ParamValidation): ...`.
fn assume_role_failure(stderr: &[u8], status: i32) -> String {
    let identifier = |text: &str| {
        !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
    };
    let text = String::from_utf8_lossy(stderr);
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("aws: [ERROR]: ").unwrap_or(line);
        let Some((code, rest)) = line
            .strip_prefix("An error occurred (")
            .and_then(|line| line.split_once(')'))
        else {
            continue;
        };
        if !identifier(code) {
            continue;
        }
        let operation = rest
            .trim_start()
            .strip_prefix("when calling the ")
            .and_then(|rest| rest.split_once(" operation"))
            .map(|(operation, _)| operation)
            .filter(|operation| identifier(operation));
        return match operation {
            Some(operation) if operation != "AssumeRole" => {
                format!("AWS refused AssumeRole: {code} (operation {operation})")
            }
            _ => format!("AWS refused AssumeRole: {code}"),
        };
    }
    format!("the AWS CLI exited with status {status} (its output is not shown)")
}

/// One real `AssumeRole`, for `mj doctor`: the same command a session's mint
/// runs, bypassing the cache. The role session name (and `SourceIdentity`,
/// when the profile sets it) has the shape of a real session's, `mj-` and 32
/// random lowercase hex digits, because a trust policy may allow only that
/// shape and a doctor probe must not be refused for looking different.
pub fn probe(
    source: &AwsCredentialSource,
    executor: &impl CommandExecutor,
) -> Result<AwsCredentials> {
    let role = source
        .role
        .as_ref()
        .context("no AWS role is configured for this profile")?;
    let host_profile = selected_profile(source);
    let name = session_name(&mj_core::state::new_session_id()?);
    let command = assume_role_command(role, host_profile, &name, &source.environment);
    mint(&command, host_profile, executor)
}

/// The unscoped export for a profile without a role, the scoped mint for one
/// with a role. Launch always resolves unscoped credentials immediately.
pub fn obtain(
    session_id: &str,
    source: &AwsCredentialSource,
    executor: &impl CommandExecutor,
) -> Result<AwsCredentials> {
    if source.role.is_some() {
        AwsCredentialCache::shared().credentials_for(session_id, source, executor, Duration::ZERO)
    } else {
        crate::aws_credentials::export(source, executor)
    }
}

/// The STS replies recorded for tests of role assumption, here and in the
/// crates that drive it.
#[doc(hidden)]
pub mod test_fixtures {
    pub const ASSUME_ROLE: &str = include_str!("../tests/fixtures/aws-sts-assume-role.json");
    pub const ACCESS_DENIED: &str =
        include_str!("../tests/fixtures/aws-sts-assume-role-access-denied.txt");
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::targets::CommandOutput;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const SESSION: &str = "0123456789abcdef0123456789abcdef";

    fn role() -> AwsRoleConfig {
        AwsRoleConfig {
            role_arn: "arn:aws:iam::123456789012:role/mj-bedrock".into(),
            session_policy: None,
            session_policy_arns: Vec::new(),
            duration_seconds: None,
            source_identity: false,
        }
    }

    fn source(role: AwsRoleConfig) -> AwsCredentialSource {
        AwsCredentialSource {
            environment: BTreeMap::from([("AWS_PROFILE".into(), "work".into())]),
            role: Some(role),
            mints: true,
        }
    }

    /// The recorded `aws sts assume-role` output with its expiration moved to
    /// `lifetime` after `issued`, because credentials expired in the past
    /// are refused. Everything else is exactly what the CLI printed.
    fn recorded_output(issued: SystemTime, lifetime: Duration) -> Vec<u8> {
        let mut output: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/aws-sts-assume-role.json"))
                .unwrap();
        output["Credentials"]["Expiration"] =
            chrono::DateTime::<chrono::Utc>::from(issued + lifetime)
                .format("%Y-%m-%dT%H:%M:%S+00:00")
                .to_string()
                .into();
        serde_json::to_vec(&output).unwrap()
    }

    /// A test clock the executor and the cache share.
    #[derive(Clone)]
    struct TestClock(Arc<Mutex<SystemTime>>);

    impl TestClock {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(SystemTime::now())))
        }

        fn now(&self) -> SystemTime {
            *self.0.lock().unwrap()
        }

        fn advance(&self, by: Duration) {
            *self.0.lock().unwrap() += by;
        }

        fn clock(&self) -> Clock {
            let this = self.clone();
            Arc::new(move || this.now())
        }
    }

    /// Plays the AWS CLI: grants `lifetime`, counts calls, can fail.
    struct FakeAws {
        clock: TestClock,
        lifetime: Duration,
        static_export: bool,
        calls: AtomicUsize,
        commands: Mutex<Vec<CommandSpec>>,
        stderr: Mutex<Option<String>>,
    }

    impl FakeAws {
        fn new(clock: &TestClock, lifetime: Duration) -> Self {
            Self {
                clock: clock.clone(),
                lifetime,
                static_export: false,
                calls: AtomicUsize::new(0),
                commands: Mutex::new(Vec::new()),
                stderr: Mutex::new(None),
            }
        }
    }

    impl CommandExecutor for FakeAws {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let export = command.args[..2] == ["configure", "export-credentials"];
            if !export {
                assert_eq!(&command.args[..2], ["sts", "assume-role"]);
            }
            assert!(command.sensitive_output);
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.commands.lock().unwrap().push(command.clone());
            if let Some(stderr) = self.stderr.lock().unwrap().clone() {
                return Ok(CommandOutput {
                    status: 254,
                    stdout: b"private-key-material".to_vec(),
                    stderr: stderr.into_bytes(),
                });
            }
            let mut stdout = recorded_output(self.clock.now(), self.lifetime);
            if export {
                let mut reply: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
                let mut credentials = reply["Credentials"].take();
                credentials["Version"] = 1.into();
                if self.static_export {
                    credentials.as_object_mut().unwrap().remove("Expiration");
                    credentials.as_object_mut().unwrap().remove("SessionToken");
                }
                stdout = serde_json::to_vec(&credentials).unwrap();
            }
            Ok(CommandOutput {
                status: 0,
                stdout,
                stderr: Vec::new(),
            })
        }
    }

    fn cache(clock: &TestClock) -> AwsCredentialCache {
        AwsCredentialCache::with_clock(clock.clock())
    }

    #[test]
    fn expiring_exports_share_the_cache_until_half_life() {
        let clock = TestClock::new();
        let aws = FakeAws::new(&clock, Duration::from_secs(900));
        let cache = cache(&clock);
        let mut source = source(role());
        source.role = None;
        let interval = Duration::from_secs(3600);
        let first = cache
            .credentials_for(SESSION, &source, &aws, interval)
            .unwrap();
        assert!(first.scope().is_none());
        let renew_in = cache.renew_in("peer", &source).unwrap();
        assert!(renew_in <= Duration::from_secs(450) && renew_in > Duration::from_secs(449));
        cache
            .credentials_for("peer", &source, &aws, interval)
            .unwrap();
        assert_eq!(aws.calls.load(Ordering::SeqCst), 1);
        clock.advance(renew_in);
        cache
            .credentials_for("peer", &source, &aws, interval)
            .unwrap();
        assert_eq!(aws.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn static_exports_are_shared_and_forget_invalidates_the_shared_entry() {
        let clock = TestClock::new();
        let mut aws = FakeAws::new(&clock, Duration::ZERO);
        aws.static_export = true;
        let cache = cache(&clock);
        let mut source = source(role());
        source.role = None;
        let interval = Duration::from_secs(60);
        cache
            .credentials_for(SESSION, &source, &aws, interval)
            .unwrap();
        cache
            .credentials_for("peer", &source, &aws, interval)
            .unwrap();
        assert_eq!(cache.renew_in("peer", &source), Some(interval));
        assert_eq!(cache.remaining_validity("peer", &source), None);
        assert_eq!(aws.calls.load(Ordering::SeqCst), 1);
        cache.forget("peer", &source);
        cache
            .credentials_for(SESSION, &source, &aws, interval)
            .unwrap();
        assert_eq!(aws.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn sweep_retains_static_exports_until_they_are_due_and_unheld() {
        let clock = TestClock::new();
        let mut aws = FakeAws::new(&clock, Duration::ZERO);
        aws.static_export = true;
        let cache = cache(&clock);
        let mut source = source(role());
        source.role = None;
        let interval = Duration::from_secs(60);
        cache
            .credentials_for(SESSION, &source, &aws, interval)
            .unwrap();
        cache.sweep_expired();
        let held = Arc::clone(cache.entries.lock().unwrap().values().next().unwrap());
        clock.advance(interval);
        cache.sweep_expired();
        assert_eq!(cache.entries.lock().unwrap().len(), 1);
        drop(held);
        cache.sweep_expired();
        assert!(cache.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn credentials_are_reused_until_half_their_lifetime_has_passed() {
        for lifetime in [Duration::from_secs(900), Duration::from_secs(3600)] {
            let clock = TestClock::new();
            let aws = FakeAws::new(&clock, lifetime);
            let cache = cache(&clock);
            let source = source(role());
            let mint = || {
                cache
                    .credentials_for(SESSION, &source, &aws, Duration::ZERO)
                    .unwrap()
            };
            let first = mint();
            // AWS reports whole seconds, so the granted lifetime can be a
            // fraction of a second short.
            let near = |wait: Option<Duration>, expected: Duration| {
                let wait = wait.unwrap();
                wait <= expected && expected - wait < Duration::from_secs(1)
            };
            assert!(near(cache.renew_in(SESSION, &source), lifetime / 2));
            clock.advance(lifetime / 2 - Duration::from_secs(1));
            assert_eq!(mint().encode().unwrap(), first.encode().unwrap());
            assert_eq!(aws.calls.load(Ordering::SeqCst), 1);
            // The renewal is scheduled for the moment reuse stops.
            assert!(near(
                cache.renew_in(SESSION, &source),
                Duration::from_secs(1)
            ));
            // Half the granted lifetime has passed: the next pass renews, and
            // no renewal is scheduled for "now", which would loop without
            // waiting.
            clock.advance(Duration::from_secs(1));
            assert_eq!(cache.renew_in(SESSION, &source), None);
            mint();
            assert_eq!(aws.calls.load(Ordering::SeqCst), 2);
        }
    }

    // The signing key is the controller's own: when it cannot be read, the
    // failure says so, and does not tell the person to give sessions their
    // unscoped AWS credentials instead.
    #[test]
    fn a_signing_key_failure_is_not_reported_as_a_failed_role_assumption() {
        let clock = TestClock::new();
        let aws = FakeAws::new(&clock, Duration::from_secs(3600));
        let cache = cache(&clock);
        SIGNING_KEY_UNAVAILABLE.with(|unavailable| unavailable.set(true));
        let error = cache
            .credentials_for(SESSION, &source(role()), &aws, Duration::ZERO)
            .map(|_| ())
            .unwrap_err();
        SIGNING_KEY_UNAVAILABLE.with(|unavailable| unavailable.set(false));
        let reported = format!("{error:#}");
        assert!(reported.contains("signing key"), "{reported}");
        assert!(!reported.contains("Could not assume"), "{reported}");
        assert!(!reported.contains("unscoped"), "{reported}");
    }

    // Verification uses the key derived once per process: after the first
    // use, the async path reads it from memory instead of loading it again.
    #[tokio::test]
    async fn verification_keeps_the_derived_key_for_later_use() {
        let clock = TestClock::new();
        let aws = FakeAws::new(&clock, Duration::from_secs(3600));
        let issued = cache(&clock)
            .credentials_for(SESSION, &source(role()), &aws, Duration::ZERO)
            .unwrap();
        let scope = issued.scope().unwrap().clone();
        let fingerprint = mj_core::aws_credentials::snapshot(&issued.encode().unwrap()).fingerprint;
        assert!(scope_is_authentic(&scope, &fingerprint).await.unwrap());
        assert!(FIXTURE_SCOPE_KEY.get().is_some());
        let mut forged = scope.clone();
        forged.role_arn.push('x');
        assert!(!scope_is_authentic(&forged, &fingerprint).await.unwrap());
    }

    #[test]
    fn each_session_has_its_own_credentials_and_session_name() {
        let clock = TestClock::new();
        let aws = FakeAws::new(&clock, Duration::from_secs(3600));
        let cache = cache(&clock);
        let source = source(role());
        let other = "fedcba9876543210fedcba9876543210";
        cache
            .credentials_for(SESSION, &source, &aws, Duration::ZERO)
            .unwrap();
        cache
            .credentials_for(other, &source, &aws, Duration::ZERO)
            .unwrap();
        cache
            .credentials_for(SESSION, &source, &aws, Duration::ZERO)
            .unwrap();
        let commands = aws.commands.lock().unwrap();
        let names = commands
            .iter()
            .map(|command| {
                let at = command
                    .args
                    .iter()
                    .position(|arg| arg == "--role-session-name")
                    .unwrap();
                command.args[at + 1].clone()
            })
            .collect::<Vec<_>>();
        assert_eq!(names, [format!("mj-{SESSION}"), format!("mj-{other}")]);
    }

    #[test]
    fn a_failed_renewal_is_an_error_and_keeps_the_entry_until_it_expires() {
        let clock = TestClock::new();
        let aws = FakeAws::new(&clock, Duration::from_secs(900));
        let cache = cache(&clock);
        let source = source(role());
        cache
            .credentials_for(SESSION, &source, &aws, Duration::ZERO)
            .unwrap();
        clock.advance(Duration::from_secs(450));
        *aws.stderr.lock().unwrap() = Some(
            include_str!("../tests/fixtures/aws-sts-assume-role-access-denied.txt").to_owned(),
        );
        let error = cache
            .credentials_for(SESSION, &source, &aws, Duration::ZERO)
            .map(|_| ())
            .unwrap_err();
        let reported = format!("{error:#}");
        // The person is told what AWS said and how to proceed; the helper's
        // other output (here, key material on stdout) is never repeated.
        assert!(reported.contains("AccessDenied"), "{reported}");
        // The remedy is the trust-policy fix only; the scope is never offered
        // for removal, which would widen access.
        assert!(
            reported.contains("named in the role's trust policy")
                || reported.contains("name it in the role's trust policy"),
            "{reported}"
        );
        assert!(!reported.contains("remove [profiles."), "{reported}");
        assert!(!reported.contains("unscoped"), "{reported}");
        assert!(!reported.contains("private-key-material"), "{reported}");
        // AWS recovered: the next pass renews without needing the old entry.
        *aws.stderr.lock().unwrap() = None;
        cache
            .credentials_for(SESSION, &source, &aws, Duration::ZERO)
            .unwrap();
        assert_eq!(aws.calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn expired_entries_are_dropped_by_the_pass_and_not_by_every_call() {
        let clock = TestClock::new();
        let aws = FakeAws::new(&clock, Duration::from_secs(900));
        let cache = cache(&clock);
        let source = source(role());
        cache
            .credentials_for(SESSION, &source, &aws, Duration::ZERO)
            .unwrap();
        assert_eq!(cache.entries.lock().unwrap().len(), 1);
        clock.advance(Duration::from_secs(901));
        cache
            .credentials_for(
                "fedcba9876543210fedcba9876543210",
                &source,
                &aws,
                Duration::ZERO,
            )
            .unwrap();
        // A call scans nothing but its own key...
        assert_eq!(cache.entries.lock().unwrap().len(), 2);
        // ...the pass drops what has expired.
        cache.sweep_expired();
        let entries = cache.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(
            entries
                .keys()
                .all(|key| key.session_id.as_deref() != Some(SESSION))
        );
    }

    #[test]
    fn concurrent_callers_for_one_session_share_one_mint() {
        let clock = TestClock::new();
        let aws = FakeAws::new(&clock, Duration::from_secs(3600));
        let cache = cache(&clock);
        let source = source(role());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    cache
                        .credentials_for(SESSION, &source, &aws, Duration::ZERO)
                        .unwrap();
                });
            }
        });
        assert_eq!(aws.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_changed_credential_source_mints_again() {
        let clock = TestClock::new();
        let aws = FakeAws::new(&clock, Duration::from_secs(3600));
        let cache = cache(&clock);
        let mut source = source(role());
        cache
            .credentials_for(SESSION, &source, &aws, Duration::ZERO)
            .unwrap();
        cache
            .credentials_for(SESSION, &source, &aws, Duration::ZERO)
            .unwrap();
        assert_eq!(aws.calls.load(Ordering::SeqCst), 1);
        // The same AWS profile name now resolves through another config file.
        source
            .environment
            .insert("AWS_CONFIG_FILE".into(), "/elsewhere/config".into());
        cache
            .credentials_for(SESSION, &source, &aws, Duration::ZERO)
            .unwrap();
        assert_eq!(aws.calls.load(Ordering::SeqCst), 2);
        let commands = aws.commands.lock().unwrap();
        assert_eq!(
            commands[1].env.get("AWS_CONFIG_FILE").map(String::as_str),
            Some("/elsewhere/config")
        );
    }

    #[test]
    fn the_sweep_leaves_an_expired_entry_a_caller_still_holds() {
        let clock = TestClock::new();
        let aws = FakeAws::new(&clock, Duration::from_secs(900));
        let cache = cache(&clock);
        let source = source(role());
        cache
            .credentials_for(SESSION, &source, &aws, Duration::ZERO)
            .unwrap();
        // A racing caller has cloned the cell and waits for its lock.
        let held = Arc::clone(cache.entries.lock().unwrap().values().next().unwrap());
        clock.advance(Duration::from_secs(901));
        cache.sweep_expired();
        assert_eq!(cache.entries.lock().unwrap().len(), 1, "held cells stay");
        // That caller finds the expired entry in its own cell and mints again,
        // into the cell the cache still holds.
        drop(held);
        cache
            .credentials_for(SESSION, &source, &aws, Duration::ZERO)
            .unwrap();
        assert_eq!(cache.entries.lock().unwrap().len(), 1);
        assert_eq!(aws.calls.load(Ordering::SeqCst), 2);
        // Once nobody holds it and it has expired, a pass removes it with no
        // mint at all: no expired secret outlives the periodic sync.
        clock.advance(Duration::from_secs(901));
        cache.sweep_expired();
        assert!(cache.entries.lock().unwrap().is_empty());
        assert_eq!(aws.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_recorded_assume_role_output_becomes_process_format_credentials() {
        let clock = TestClock::new();
        let output = recorded_output(clock.now(), Duration::from_secs(900));
        let credentials = AwsCredentials::from_assume_role("work", &output).unwrap();
        let encoded: serde_json::Value =
            serde_json::from_slice(&credentials.encode().unwrap()).unwrap();
        assert_eq!(encoded["Version"], 1);
        assert_eq!(encoded["AccessKeyId"], "ASIAEXAMPLEEXAMPLE00");
        assert_eq!(encoded["SessionToken"], "example-session-token");
        assert_eq!(encoded["Profile"], "work");
        // The CLI prints the offset as +00:00 rather than Z.
        assert!(encoded["Expiration"].as_str().unwrap().ends_with("+00:00"));
        // Expired output is refused, and the error never repeats a secret.
        let expired = recorded_output(
            clock.now() - Duration::from_secs(3600),
            Duration::from_secs(900),
        );
        let error = AwsCredentials::from_assume_role("work", &expired)
            .err()
            .unwrap();
        assert!(!format!("{error:#}").contains("example-secret"));
    }

    // The command's output is sensitive: the notice holds the error code and
    // the operation, both identifier-shaped, and never free text from the
    // CLI or from a helper it ran.
    #[test]
    fn aws_cli_failures_report_only_the_error_code_and_operation() {
        let denied = include_str!("../tests/fixtures/aws-sts-assume-role-access-denied.txt");
        // A multiline stderr: a helper's lines before and after, one of them
        // a token-like secret, around the CLI's own error.
        let stderr = format!(
            "credential helper: token sk-live-0123456789abcdef\n{denied}\nhelper trace: AKIAIOSFODNN7EXAMPLE secret-access-key\n"
        );
        let message = assume_role_failure(stderr.as_bytes(), 254);
        assert_eq!(message, "AWS refused AssumeRole: AccessDenied");
        // Another operation is named; a range check has none.
        let other = "aws: [ERROR]: An error occurred (ThrottlingException) when calling the GetCallerIdentity operation: token sk-live-0123456789abcdef\n";
        assert_eq!(
            assume_role_failure(other.as_bytes(), 254),
            "AWS refused AssumeRole: ThrottlingException (operation GetCallerIdentity)"
        );
        let range = "\naws: [ERROR]: An error occurred (ParamValidation): Parameter validation failed:\nInvalid value for parameter DurationSeconds, value: 100, valid min value: 900\n";
        assert_eq!(
            assume_role_failure(range.as_bytes(), 252),
            "AWS refused AssumeRole: ParamValidation"
        );
        // A code or operation that is not identifier-shaped is not repeated.
        let spoofed = "An error occurred (sk-live-0123456789abcdef!) when calling the AssumeRole operation: x\nAn error occurred (AccessDenied) when calling the Assume Role/secret operation: x\n";
        let message = assume_role_failure(spoofed.as_bytes(), 255);
        assert!(
            !message.contains("sk-live") && !message.contains("secret"),
            "{message}"
        );
        let helper = "Traceback: secret-token-value";
        let message = assume_role_failure(helper.as_bytes(), 255);
        assert!(!message.contains("secret-token-value") && message.contains("255"));
    }

    #[test]
    fn golden_assume_role_command_lines() {
        let mut full = role();
        full.session_policy = Some(r#"{"Version":"2012-10-17","Statement":[]}"#.into());
        full.session_policy_arns = vec![
            "arn:aws:iam::aws:policy/AmazonBedrockReadOnly".into(),
            "arn:aws:iam::123456789012:policy/team-boundary".into(),
        ];
        full.duration_seconds = Some(900);
        full.source_identity = true;
        let environment = BTreeMap::from([("AWS_PROFILE".into(), "work".into())]);
        let mut rendered = String::new();
        for (label, role) in [("default policy", role()), ("everything configured", full)] {
            let command = assume_role_command(&role, "work", &session_name(SESSION), &environment);
            rendered.push_str(&format!(
                "# {label}\n{} {}\n",
                command.program,
                command.args.join("\n  ")
            ));
        }
        mj_core::golden::assert_golden(
            env!("CARGO_MANIFEST_DIR"),
            "aws-assume-role-command",
            &rendered,
        );
    }
}
