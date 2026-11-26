//! Transactional shared EC2 security-group ingress grant state machine.
//!
//! AWS rejects duplicate security-group ingress rules, so concurrent Moves
//! between the same hosts share one ingress rule. Because a SQLite transaction
//! cannot roll back an AWS API call, every AWS call happens between two
//! committed state transitions (`Installing` -> `Installed` -> `Removing`), and
//! pre-existing non-Mjolnir rules are recorded as `External` and never revoked.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Result, bail};
use mj_core::subprocess::run_with_input;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Prefix for Mjolnir-owned EC2 security-group ingress rule descriptions.
pub const EC2_GRANT_DESCRIPTION_PREFIX: &str = "mj-move-grant:";

/// Format the instance-scoped EC2 ingress rule description.
pub fn grant_description(daemon_instance_id: &str) -> String {
    format!("{EC2_GRANT_DESCRIPTION_PREFIX}{daemon_instance_id}")
}

/// Unique key for a shared TCP ingress rule on an EC2 security group. It is
/// exactly the durable primary key `(security_group_id, cidr_ip, port)`, so one
/// rule has one record and one set of users however it was reached.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Ec2GrantKey {
    pub security_group_id: String,
    pub cidr_ip: String,
    pub port: u16,
}

impl Ec2GrantKey {
    pub fn ssh(security_group_id: impl Into<String>, cidr_ip: impl Into<String>) -> Self {
        Self {
            security_group_id: security_group_id.into(),
            cidr_ip: cidr_ip.into(),
            port: 22,
        }
    }
}

/// Where to reach the AWS API for a security group: routing metadata that
/// belongs to the record, never to its identity. `None` uses the client's
/// default region or profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ec2GrantRoute {
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub aws_profile: Option<String>,
}

impl Ec2GrantRoute {
    pub fn new(region: Option<String>, aws_profile: Option<String>) -> Self {
        Self {
            region,
            aws_profile,
        }
    }

    pub fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    pub fn aws_profile(&self) -> Option<&str> {
        self.aws_profile.as_deref()
    }

    pub fn is_default(&self) -> bool {
        self.region.is_none() && self.aws_profile.is_none()
    }

    /// Fill the fields this route lacks from `other`. A field that is already
    /// set keeps its value, so a later caller cannot reroute the first owner.
    pub fn fill_from(&mut self, other: &Self) {
        if self.region.is_none() {
            self.region.clone_from(&other.region);
        }
        if self.aws_profile.is_none() {
            self.aws_profile.clone_from(&other.aws_profile);
        }
    }
}

/// A rule to open and the AWS region and profile that own its security group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ec2GrantTarget {
    pub key: Ec2GrantKey,
    pub route: Ec2GrantRoute,
}

impl Ec2GrantTarget {
    pub fn new(key: Ec2GrantKey, route: Ec2GrantRoute) -> Self {
        Self { key, route }
    }
}

/// Durable lifecycle state of a shared EC2 security-group ingress grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ec2GrantState {
    Installing,
    Installed,
    Removing,
    External,
}

/// Durable record for one shared EC2 security-group ingress grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ec2GrantRecord {
    pub key: Ec2GrantKey,
    pub state: Ec2GrantState,
    pub users: BTreeSet<String>,
    /// Number of consecutive separated AWS `describe` checks that reported the
    /// rule absent while in `Removing`. A record is dropped only once this
    /// reaches 2 (or after a revoke succeeds).
    #[serde(default)]
    pub absent_checks: u32,
    /// Monotonic generation identifying the install attempt so reconciliation
    /// never transitions a newly-acquired `Installing` record against a stale
    /// pre-acquire AWS `describe` snapshot.
    #[serde(default)]
    pub attempt_id: u64,
    /// Where AWS calls for this rule go. Shared by every user of the record.
    #[serde(default)]
    pub route: Ec2GrantRoute,
}

/// One TCP ingress rule returned by AWS `describe-security-groups`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ec2IngressRule {
    pub port: u16,
    pub cidr_ip: String,
    pub description: Option<String>,
}

/// Typed AWS EC2 error classification for grant state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ec2ApiError {
    /// AWS returned `InvalidPermission.Duplicate`.
    Duplicate,
    /// AWS returned `InvalidPermission.NotFound` on revoke.
    NotFound,
    /// Any other or ambiguous failure (timeout, lost response, transient 5xx).
    Other(String),
}

impl std::fmt::Display for Ec2ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duplicate => write!(f, "InvalidPermission.Duplicate"),
            Self::NotFound => write!(f, "InvalidPermission.NotFound"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Ec2ApiError {}

/// Outcome of attempting to acquire a shared EC2 security-group grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// The ingress rule is ready (`Installed` or `External`) and this operation
    /// is registered as a user. Caller must call `release` on every exit path.
    Granted,
    /// Another move is still installing or AWS could not be queried within the
    /// wait budget; this operation's registration has been released so the
    /// caller should fall back to `TransferRoute::DaemonStream`.
    FallbackToDaemonStream,
}

/// Summary of one `reconcile_group` pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub revoked_rules: usize,
    pub dropped_records: usize,
    pub resolved_installed: usize,
    pub remaining_pending: usize,
}

/// Transactional persistence abstraction for EC2 grant records.
///
/// Slice 5D plugs SQLite persistence into this trait; tests use
/// [`InMemoryGrantStore`].
pub trait Ec2GrantStore: Send + Sync {
    /// The one owner of in-flight work for this store. Every manager over the
    /// same durable records must share it, or a reconciler built beside a live
    /// Move cannot see that Move's pending AWS call.
    fn coordination(&self) -> Arc<Ec2GrantCoordination>;

    fn with_transaction<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut BTreeMap<Ec2GrantKey, Ec2GrantRecord>) -> Result<R>;
}

/// In-flight AWS work and attempt numbering shared by every
/// [`Ec2GrantManager`] over one store.
#[derive(Debug, Default)]
pub struct Ec2GrantCoordination {
    last_attempt_id: AtomicU64,
    in_flight_installs: Mutex<BTreeSet<(Ec2GrantKey, u64)>>,
    in_flight_revokes: Mutex<BTreeSet<Ec2GrantKey>>,
}

impl Ec2GrantCoordination {
    /// The next attempt number: above every number this process handed out and
    /// above `persisted_max`, so a restart never reuses a stored generation.
    fn next_attempt_id(&self, persisted_max: u64) -> u64 {
        let previous = self
            .last_attempt_id
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |last| {
                Some(last.max(persisted_max) + 1)
            })
            .expect("the update always succeeds");
        previous.max(persisted_max) + 1
    }
}

/// In-memory mutex-backed [`Ec2GrantStore`] for tests and standalone use.
#[derive(Debug, Default)]
pub struct InMemoryGrantStore {
    records: Mutex<BTreeMap<Ec2GrantKey, Ec2GrantRecord>>,
    coordination: Arc<Ec2GrantCoordination>,
}

impl InMemoryGrantStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> BTreeMap<Ec2GrantKey, Ec2GrantRecord> {
        self.records.lock().expect("grant store lock").clone()
    }
}

impl Ec2GrantStore for InMemoryGrantStore {
    fn coordination(&self) -> Arc<Ec2GrantCoordination> {
        self.coordination.clone()
    }

    fn with_transaction<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut BTreeMap<Ec2GrantKey, Ec2GrantRecord>) -> Result<R>,
    {
        let mut guard = self.records.lock().expect("grant store lock");
        f(&mut guard)
    }
}

/// One coordination per daemon database: every Move and reconciler in this
/// process that reads the same `ec2_move_grants` table shares it.
fn sqlite_coordination() -> Arc<Ec2GrantCoordination> {
    static BY_DATA_DIR: OnceLock<Mutex<HashMap<PathBuf, Arc<Ec2GrantCoordination>>>> =
        OnceLock::new();
    BY_DATA_DIR
        .get_or_init(Default::default)
        .lock()
        .expect("coordination registry lock")
        .entry(mj_core::config::DataLayout::current().root().to_path_buf())
        .or_default()
        .clone()
}

/// SQLite-backed [`Ec2GrantStore`] persisted in the daemon's `ec2_move_grants` table.
#[derive(Debug, Default, Clone, Copy)]
pub struct SqliteEc2GrantStore;

impl SqliteEc2GrantStore {
    fn state_to_str(state: Ec2GrantState) -> &'static str {
        match state {
            Ec2GrantState::Installing => "installing",
            Ec2GrantState::Installed => "installed",
            Ec2GrantState::Removing => "removing",
            Ec2GrantState::External => "external",
        }
    }

    fn str_to_state(state: &str) -> Result<Ec2GrantState> {
        match state {
            "installing" => Ok(Ec2GrantState::Installing),
            "installed" => Ok(Ec2GrantState::Installed),
            "removing" => Ok(Ec2GrantState::Removing),
            "external" => Ok(Ec2GrantState::External),
            other => bail!("unknown ec2_move_grants state `{other}`"),
        }
    }
}

impl Ec2GrantStore for SqliteEc2GrantStore {
    fn coordination(&self) -> Arc<Ec2GrantCoordination> {
        sqlite_coordination()
    }

    fn with_transaction<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut BTreeMap<Ec2GrantKey, Ec2GrantRecord>) -> Result<R>,
    {
        let (to_caller_tx, to_caller_rx) =
            std::sync::mpsc::sync_channel::<BTreeMap<Ec2GrantKey, Ec2GrantRecord>>(1);
        let (to_writer_tx, to_writer_rx) =
            std::sync::mpsc::sync_channel::<BTreeMap<Ec2GrantKey, Ec2GrantRecord>>(1);

        std::thread::scope(|scope| {
            let tx_handle = scope.spawn(move || {
                crate::database::with_ec2_move_grants_transaction(move |rows| {
                    let mut map = BTreeMap::new();
                    for (k, v) in rows.iter() {
                        let key = Ec2GrantKey {
                            security_group_id: k.security_group_id.clone(),
                            cidr_ip: k.cidr_ip.clone(),
                            port: k.port,
                        };
                        map.insert(
                            key.clone(),
                            Ec2GrantRecord {
                                key,
                                state: Self::str_to_state(&v.state)?,
                                users: v.users.clone(),
                                absent_checks: v.absent_checks,
                                attempt_id: v.attempt_id,
                                route: Ec2GrantRoute::new(v.region.clone(), v.aws_profile.clone()),
                            },
                        );
                    }
                    to_caller_tx
                        .send(map)
                        .map_err(|_| anyhow::anyhow!("ec2 grant transaction caller dropped"))?;
                    let updated = to_writer_rx
                        .recv()
                        .map_err(|_| anyhow::anyhow!("ec2 grant transaction aborted"))?;
                    rows.clear();
                    for (k, v) in updated {
                        let key_row = crate::database::Ec2MoveGrantKeyRow {
                            security_group_id: k.security_group_id,
                            cidr_ip: k.cidr_ip,
                            port: k.port,
                        };
                        rows.insert(
                            key_row.clone(),
                            crate::database::Ec2MoveGrantRow {
                                key: key_row,
                                state: Self::state_to_str(v.state).to_owned(),
                                users: v.users,
                                absent_checks: v.absent_checks,
                                attempt_id: v.attempt_id,
                                region: v.route.region,
                                aws_profile: v.route.aws_profile,
                            },
                        );
                    }
                    Ok(())
                })
            });

            let mut map = match to_caller_rx.recv() {
                Ok(map) => map,
                Err(_) => {
                    drop(to_writer_tx);
                    match tx_handle.join() {
                        Ok(Err(err)) => return Err(err),
                        Ok(Ok(())) => bail!("ec2 grant transaction closed without yielding state"),
                        Err(_) => bail!("ec2 grant transaction thread panicked"),
                    }
                }
            };
            match f(&mut map) {
                Ok(value) => {
                    let _ = to_writer_tx.send(map);
                    match tx_handle.join() {
                        Ok(Ok(())) => Ok(value),
                        Ok(Err(err)) => Err(err),
                        Err(_) => bail!("ec2 grant transaction thread panicked"),
                    }
                }
                Err(err) => {
                    // Dropping `to_writer_tx` aborts and rolls back the SQLite transaction.
                    drop(to_writer_tx);
                    let _ = tx_handle.join();
                    Err(err)
                }
            }
        })
    }
}

/// Network placement of one EC2 instance from `describe-instances`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ec2InstanceNetwork {
    pub vpc_id: Option<String>,
    pub private_ip: Option<String>,
    pub public_ip: Option<String>,
    pub security_group_ids: Vec<String>,
}

/// Abstraction over AWS EC2 API calls. Every call carries the region and AWS
/// CLI profile of the resource; `None` falls back to the client's default.
pub trait Ec2SecurityGroupClient: Send + Sync {
    /// Authorize `key.port` from `key.cidr_ip` on `key.security_group_id`.
    fn authorize_ingress(
        &self,
        key: &Ec2GrantKey,
        route: &Ec2GrantRoute,
        description: &str,
    ) -> std::result::Result<(), Ec2ApiError>;

    fn revoke_ingress(
        &self,
        key: &Ec2GrantKey,
        route: &Ec2GrantRoute,
    ) -> std::result::Result<(), Ec2ApiError>;

    fn describe_ingress_rules(
        &self,
        group_id: &str,
        region: Option<&str>,
        aws_profile: Option<&str>,
    ) -> std::result::Result<Vec<Ec2IngressRule>, Ec2ApiError>;

    fn describe_instance(
        &self,
        instance_id: &str,
        region: Option<&str>,
        aws_profile: Option<&str>,
    ) -> std::result::Result<Ec2InstanceNetwork, Ec2ApiError>;
}

/// Real AWS CLI implementation patterned after `mj-cli/src/ec2.rs:295-345, 1145`.
#[derive(Debug, Clone)]
pub struct AwsCliEc2Client {
    pub program: String,
    pub region: Option<String>,
    pub aws_profile: Option<String>,
}

impl Default for AwsCliEc2Client {
    fn default() -> Self {
        Self {
            program: "aws".to_owned(),
            region: None,
            aws_profile: None,
        }
    }
}

impl AwsCliEc2Client {
    fn ec2(
        &self,
        operation: &str,
        region: Option<&str>,
        aws_profile: Option<&str>,
        request: Value,
    ) -> std::result::Result<Value, Ec2ApiError> {
        let mut command = Command::new(&self.program);
        command.args([
            "--no-cli-pager",
            "--output",
            "json",
            "--cli-binary-format",
            "base64",
        ]);
        if let Some(region) = region.or(self.region.as_deref()) {
            command.args(["--region", region]);
        }
        if let Some(profile) = aws_profile.or(self.aws_profile.as_deref()) {
            command.args(["--profile", profile]);
        }
        command
            .arg("ec2")
            .arg(operation)
            .arg("--cli-input-json")
            .arg(request.to_string());
        let output = run_with_input(&mut command, &[])
            .map_err(|err| Ec2ApiError::Other(format!("aws ec2 {operation}: {err}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = stderr
                .split_once("An error occurred (")
                .and_then(|(_, rest)| rest.split_once(") when calling "))
                .map(|(code, _)| code);
            return match code {
                Some("InvalidPermission.Duplicate") => Err(Ec2ApiError::Duplicate),
                Some("InvalidPermission.NotFound") => Err(Ec2ApiError::NotFound),
                _ => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    Err(Ec2ApiError::Other(format!(
                        "{}: {}{}",
                        output.status,
                        stdout.trim(),
                        stderr.trim()
                    )))
                }
            };
        }
        if output.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|err| Ec2ApiError::Other(format!("decode AWS CLI JSON response: {err}")))
    }
}

impl Ec2SecurityGroupClient for AwsCliEc2Client {
    fn authorize_ingress(
        &self,
        key: &Ec2GrantKey,
        route: &Ec2GrantRoute,
        description: &str,
    ) -> std::result::Result<(), Ec2ApiError> {
        self.ec2(
            "authorize-security-group-ingress",
            route.region(),
            route.aws_profile(),
            json!({
                "GroupId": key.security_group_id,
                "IpPermissions": [{
                    "IpProtocol": "tcp",
                    "FromPort": key.port,
                    "ToPort": key.port,
                    "IpRanges": [{
                        "CidrIp": key.cidr_ip,
                        "Description": description,
                    }]
                }]
            }),
        )?;
        Ok(())
    }

    fn revoke_ingress(
        &self,
        key: &Ec2GrantKey,
        route: &Ec2GrantRoute,
    ) -> std::result::Result<(), Ec2ApiError> {
        self.ec2(
            "revoke-security-group-ingress",
            route.region(),
            route.aws_profile(),
            json!({
                "GroupId": key.security_group_id,
                "IpPermissions": [{
                    "IpProtocol": "tcp",
                    "FromPort": key.port,
                    "ToPort": key.port,
                    "IpRanges": [{
                        "CidrIp": key.cidr_ip,
                    }]
                }]
            }),
        )?;
        Ok(())
    }

    fn describe_instance(
        &self,
        instance_id: &str,
        region: Option<&str>,
        aws_profile: Option<&str>,
    ) -> std::result::Result<Ec2InstanceNetwork, Ec2ApiError> {
        let response = self.ec2(
            "describe-instances",
            region,
            aws_profile,
            json!({ "InstanceIds": [instance_id] }),
        )?;
        let instance = &response["Reservations"][0]["Instances"][0];
        if !instance.is_object() {
            return Err(Ec2ApiError::Other(format!(
                "describe-instances returned no instance {instance_id}"
            )));
        }
        let text = |value: &Value| value.as_str().map(ToOwned::to_owned);
        Ok(Ec2InstanceNetwork {
            vpc_id: text(&instance["VpcId"]),
            private_ip: text(&instance["PrivateIpAddress"]),
            public_ip: text(&instance["PublicIpAddress"]),
            security_group_ids: instance["SecurityGroups"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|group| text(&group["GroupId"]))
                .collect(),
        })
    }

    fn describe_ingress_rules(
        &self,
        group_id: &str,
        region: Option<&str>,
        aws_profile: Option<&str>,
    ) -> std::result::Result<Vec<Ec2IngressRule>, Ec2ApiError> {
        let response = self.ec2(
            "describe-security-groups",
            region,
            aws_profile,
            json!({ "GroupIds": [group_id] }),
        )?;
        let mut rules = Vec::new();
        let Some(groups) = response["SecurityGroups"].as_array() else {
            return Ok(rules);
        };
        for group in groups {
            let Some(perms) = group["IpPermissions"].as_array() else {
                continue;
            };
            for perm in perms {
                if perm["IpProtocol"].as_str() != Some("tcp") {
                    continue;
                }
                let Some(from_port) = perm["FromPort"].as_u64() else {
                    continue;
                };
                let Some(to_port) = perm["ToPort"].as_u64() else {
                    continue;
                };
                if from_port != to_port {
                    continue;
                }
                let Ok(port) = u16::try_from(from_port) else {
                    continue;
                };
                let Some(ranges) = perm["IpRanges"].as_array() else {
                    continue;
                };
                for range in ranges {
                    let Some(cidr_ip) = range["CidrIp"].as_str() else {
                        continue;
                    };
                    rules.push(Ec2IngressRule {
                        port,
                        cidr_ip: cidr_ip.to_owned(),
                        description: range["Description"].as_str().map(ToOwned::to_owned),
                    });
                }
            }
        }
        Ok(rules)
    }
}

/// Tuning parameters for backoff waits in [`Ec2GrantManager`].
#[derive(Debug, Clone, Copy)]
pub struct Ec2GrantConfig {
    pub backoff: Duration,
    pub max_wait_attempts: usize,
}

impl Default for Ec2GrantConfig {
    fn default() -> Self {
        Self {
            backoff: Duration::from_millis(50),
            max_wait_attempts: 40,
        }
    }
}

/// Coordinates transactional shared EC2 security-group ingress grants.
pub struct Ec2GrantManager<S, C> {
    store: S,
    client: C,
    daemon_instance_id: String,
    config: Ec2GrantConfig,
    /// Owned by the store, not the manager; see [`Ec2GrantStore::coordination`].
    coordination: Arc<Ec2GrantCoordination>,
}

struct InFlightTokenGuard<'a> {
    set: &'a Mutex<BTreeSet<(Ec2GrantKey, u64)>>,
    token: (Ec2GrantKey, u64),
}

impl Drop for InFlightTokenGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().expect("in-flight lock").remove(&self.token);
    }
}

struct InFlightRevokeGuard<'a> {
    set: &'a Mutex<BTreeSet<Ec2GrantKey>>,
    key: Ec2GrantKey,
}

impl Drop for InFlightRevokeGuard<'_> {
    fn drop(&mut self) {
        self.set
            .lock()
            .expect("in-flight revoke lock")
            .remove(&self.key);
    }
}

enum AcquireStep {
    Granted,
    BecomeInstaller(u64),
    WaitForInstaller,
    WaitForRemoval,
}

impl<S: Ec2GrantStore, C: Ec2SecurityGroupClient> Ec2GrantManager<S, C> {
    pub fn new(
        store: S,
        client: C,
        daemon_instance_id: impl Into<String>,
        config: Ec2GrantConfig,
    ) -> Self {
        let coordination = store.coordination();
        Self {
            store,
            client,
            daemon_instance_id: daemon_instance_id.into(),
            config,
            coordination,
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn client(&self) -> &C {
        &self.client
    }

    pub fn description(&self) -> String {
        grant_description(&self.daemon_instance_id)
    }

    /// Acquire a shared EC2 security-group ingress grant for `operation_id`.
    ///
    /// The grant's identity is `target.key` alone. If a record for it already
    /// exists, this operation joins its users and the record keeps its route;
    /// the caller's route only fills a field the record lacks.
    pub fn acquire(&self, target: &Ec2GrantTarget, operation_id: &str) -> Result<AcquireOutcome> {
        let key = &target.key;
        let mut attempts = 0usize;
        loop {
            // Hold `in_flight_installs` across the store transaction (consistent
            // lock order: `in_flight_installs` -> `store`) so `reconcile_group`
            // can never observe a newly committed `Installing` record without
            // also seeing it in `in_flight_installs`.
            let step = {
                let mut in_flight = self
                    .coordination
                    .in_flight_installs
                    .lock()
                    .expect("in_flight_installs lock");
                let step = self.store.with_transaction(|records| {
                    if let Some(record) = records.get_mut(key) {
                        match record.state {
                            Ec2GrantState::Installed | Ec2GrantState::External => {
                                record.users.insert(operation_id.to_owned());
                                record.route.fill_from(&target.route);
                                Ok(AcquireStep::Granted)
                            }
                            Ec2GrantState::Installing => {
                                record.users.insert(operation_id.to_owned());
                                record.route.fill_from(&target.route);
                                Ok(AcquireStep::WaitForInstaller)
                            }
                            Ec2GrantState::Removing => {
                                // Do not register while removing; wait until gone.
                                Ok(AcquireStep::WaitForRemoval)
                            }
                        }
                    } else {
                        let mut users = BTreeSet::new();
                        users.insert(operation_id.to_owned());
                        let attempt_id = self.next_attempt_id(records);
                        records.insert(
                            key.clone(),
                            Ec2GrantRecord {
                                key: key.clone(),
                                state: Ec2GrantState::Installing,
                                users,
                                absent_checks: 0,
                                attempt_id,
                                route: target.route.clone(),
                            },
                        );
                        Ok(AcquireStep::BecomeInstaller(attempt_id))
                    }
                })?;
                if let AcquireStep::BecomeInstaller(attempt_id) = step {
                    in_flight.insert((key.clone(), attempt_id));
                }
                step
            };

            match step {
                AcquireStep::Granted => return Ok(AcquireOutcome::Granted),
                AcquireStep::BecomeInstaller(attempt_id) => {
                    let _guard = InFlightTokenGuard {
                        set: &self.coordination.in_flight_installs,
                        token: (key.clone(), attempt_id),
                    };
                    return self.run_installer(target, attempt_id, operation_id);
                }
                AcquireStep::WaitForInstaller => {
                    attempts += 1;
                    if attempts > self.config.max_wait_attempts {
                        // Use atomic `release` so if the installer finished and
                        // released before we timed out, we transition `Installed`
                        // -> `Removing` and revoke instead of leaking the rule.
                        self.release(key, operation_id)?;
                        return Ok(AcquireOutcome::FallbackToDaemonStream);
                    }
                    std::thread::sleep(self.config.backoff);
                }
                AcquireStep::WaitForRemoval => {
                    attempts += 1;
                    if attempts > self.config.max_wait_attempts {
                        return Ok(AcquireOutcome::FallbackToDaemonStream);
                    }
                    std::thread::sleep(self.config.backoff);
                }
            }
        }
    }

    /// A fresh attempt number above every stored and every handed-out one.
    fn next_attempt_id(&self, records: &BTreeMap<Ec2GrantKey, Ec2GrantRecord>) -> u64 {
        let persisted = records.values().map(|r| r.attempt_id).max().unwrap_or(0);
        self.coordination.next_attempt_id(persisted)
    }

    fn run_installer(
        &self,
        target: &Ec2GrantTarget,
        attempt_id: u64,
        operation_id: &str,
    ) -> Result<AcquireOutcome> {
        let key = &target.key;
        let route = &target.route;
        let our_desc = self.description();
        match self.client.authorize_ingress(key, route, &our_desc) {
            Ok(()) => self.finish_installed_or_revoke(key, attempt_id, Ec2GrantState::Installed),
            Err(Ec2ApiError::Duplicate) => {
                // Rule already existed; inspect description to distinguish
                // Mjolnir's rule from an external user-managed rule.
                let rules = match self.client.describe_ingress_rules(
                    &key.security_group_id,
                    route.region(),
                    route.aws_profile(),
                ) {
                    Ok(rules) => rules,
                    Err(_) => {
                        self.unregister_user_only(key, operation_id)?;
                        return Ok(AcquireOutcome::FallbackToDaemonStream);
                    }
                };
                let matching = find_matching_rule(&rules, key);
                match matching {
                    Some(rule) if rule.description.as_deref() == Some(our_desc.as_str()) => {
                        self.finish_installed_or_revoke(key, attempt_id, Ec2GrantState::Installed)
                    }
                    Some(_) => {
                        self.finish_installed_or_revoke(key, attempt_id, Ec2GrantState::External)
                    }
                    None => {
                        // Eventual consistency: AWS reported Duplicate, so a
                        // pre-existing user-managed rule may exist. NEVER
                        // revoke a rule whose ownership is unknown; release
                        // this user, leave `Installing` for reconciliation, and
                        // fall back to daemon stream.
                        self.unregister_user_only(key, operation_id)?;
                        Ok(AcquireOutcome::FallbackToDaemonStream)
                    }
                }
            }
            Err(Ec2ApiError::Other(_) | Ec2ApiError::NotFound) => {
                // Ambiguous failure: query AWS before changing state.
                let rules = match self.client.describe_ingress_rules(
                    &key.security_group_id,
                    route.region(),
                    route.aws_profile(),
                ) {
                    Ok(rules) => rules,
                    Err(_) => {
                        // Query itself failed: leave record in `Installing` for
                        // background/startup reconciliation, release this user,
                        // and fall back to daemon stream.
                        self.unregister_user_only(key, operation_id)?;
                        return Ok(AcquireOutcome::FallbackToDaemonStream);
                    }
                };
                if let Some(rule) = find_matching_rule(&rules, key) {
                    if rule.description.as_deref() == Some(our_desc.as_str()) {
                        return self.finish_installed_or_revoke(
                            key,
                            attempt_id,
                            Ec2GrantState::Installed,
                        );
                    }
                    return self.finish_installed_or_revoke(
                        key,
                        attempt_id,
                        Ec2GrantState::External,
                    );
                }
                // Absent on first check: move to `Removing` with `absent_checks = 1`
                // and NEVER call `revoke_ingress` without positively observing
                // our own description. Reconciliation will either observe our
                // description later and revoke it, or drop the record on a
                // second separated absent check.
                self.transition_installing_to_removing_without_revoke(key, attempt_id)?;
                Ok(AcquireOutcome::FallbackToDaemonStream)
            }
        }
    }

    fn finish_installed_or_revoke(
        &self,
        key: &Ec2GrantKey,
        attempt_id: u64,
        target_state: Ec2GrantState,
    ) -> Result<AcquireOutcome> {
        enum FinishDecision {
            Granted,
            Revoke,
            Fallback,
        }
        let decision = self.store.with_transaction(|records| {
            let Some(record) = records.get_mut(key) else {
                return Ok(FinishDecision::Fallback);
            };
            if record.state != Ec2GrantState::Installing || record.attempt_id != attempt_id {
                return Ok(FinishDecision::Fallback);
            }
            if record.users.is_empty() {
                if target_state == Ec2GrantState::External {
                    records.remove(key);
                    return Ok(FinishDecision::Fallback);
                }
                record.state = Ec2GrantState::Removing;
                record.absent_checks = 0;
                return Ok(FinishDecision::Revoke);
            }
            record.state = target_state;
            record.absent_checks = 0;
            Ok(FinishDecision::Granted)
        })?;
        match decision {
            FinishDecision::Granted => Ok(AcquireOutcome::Granted),
            FinishDecision::Revoke => {
                self.revoke_confirmed_and_delete(key, attempt_id)?;
                Ok(AcquireOutcome::FallbackToDaemonStream)
            }
            FinishDecision::Fallback => Ok(AcquireOutcome::FallbackToDaemonStream),
        }
    }

    fn transition_installing_to_removing_without_revoke(
        &self,
        key: &Ec2GrantKey,
        attempt_id: u64,
    ) -> Result<()> {
        self.store.with_transaction(|records| {
            if let Some(record) = records.get_mut(key)
                && record.state == Ec2GrantState::Installing
                && record.attempt_id == attempt_id
            {
                record.users.clear();
                record.state = Ec2GrantState::Removing;
                record.absent_checks = 1;
            }
            Ok(())
        })
    }

    fn unregister_user_only(&self, key: &Ec2GrantKey, operation_id: &str) -> Result<()> {
        self.store.with_transaction(|records| {
            if let Some(record) = records.get_mut(key) {
                record.users.remove(operation_id);
            }
            Ok(())
        })
    }

    /// Release `operation_id`'s use of `key`. If it was the last user of a
    /// Mjolnir-created rule, marks `Removing`, revokes in AWS, and deletes the
    /// record. Never revokes an `External` rule.
    pub fn release(&self, key: &Ec2GrantKey, operation_id: &str) -> Result<()> {
        let revoke_attempt = self.store.with_transaction(|records| {
            let Some(record) = records.get_mut(key) else {
                return Ok(None);
            };
            record.users.remove(operation_id);
            if !record.users.is_empty() {
                return Ok(None);
            }
            match record.state {
                Ec2GrantState::External => {
                    records.remove(key);
                    Ok(None)
                }
                Ec2GrantState::Installed => {
                    record.state = Ec2GrantState::Removing;
                    record.absent_checks = 0;
                    Ok(Some(record.attempt_id))
                }
                Ec2GrantState::Installing | Ec2GrantState::Removing => Ok(None),
            }
        })?;

        if let Some(attempt_id) = revoke_attempt {
            self.revoke_confirmed_and_delete(key, attempt_id)?;
        }
        Ok(())
    }

    /// Revoke `(key, expected_attempt_id)` ONLY after `describe_ingress_rules`
    /// positively confirms a rule carrying this daemon instance's description
    /// (`mj-move-grant:<id>`). Serialized per `(key, expected_attempt_id)` via
    /// `in_flight_revokes` so a concurrent reconciler never issues a duplicate
    /// revoke or deletes a re-acquired record with a newer `attempt_id`.
    ///
    /// Returns `Ok(true)` if this call executed a revoke, `Ok(false)` if
    /// skipped because another task is already revoking this generation or the
    /// rule was not owned by us.
    fn revoke_confirmed_and_delete(
        &self,
        key: &Ec2GrantKey,
        expected_attempt_id: u64,
    ) -> Result<bool> {
        {
            let mut revokes = self
                .coordination
                .in_flight_revokes
                .lock()
                .expect("in-flight revoke lock");
            if !revokes.insert(key.clone()) {
                return Ok(false);
            }
        }
        let _guard = InFlightRevokeGuard {
            set: &self.coordination.in_flight_revokes,
            key: key.clone(),
        };

        // Re-verify under the store transaction BEFORE any AWS call that this
        // key is still in `Removing` for `expected_attempt_id`. Because `acquire`
        // waits while `Removing`, no newer generation can start until we finish.
        let route = self.store.with_transaction(|records| {
            Ok(records
                .get(key)
                .filter(|r| {
                    r.state == Ec2GrantState::Removing && r.attempt_id == expected_attempt_id
                })
                .map(|r| r.route.clone()))
        })?;
        let Some(route) = route else {
            return Ok(false);
        };

        let our_desc = self.description();
        let rules = self
            .client
            .describe_ingress_rules(&key.security_group_id, route.region(), route.aws_profile())
            .map_err(|err| {
                anyhow::anyhow!(
                    "describe EC2 security group {} before revoke: {err}",
                    key.security_group_id
                )
            })?;
        let owned_by_us = find_matching_rule(&rules, key)
            .is_some_and(|rule| rule.description.as_deref() == Some(our_desc.as_str()));
        if !owned_by_us {
            // Confirm absence across two separated checks before dropping,
            // only if the stored record still matches `expected_attempt_id`.
            self.store.with_transaction(|records| {
                if let Some(record) = records.get_mut(key)
                    && record.attempt_id == expected_attempt_id
                {
                    record.absent_checks = record.absent_checks.saturating_add(1);
                    if record.absent_checks >= 2 {
                        records.remove(key);
                    }
                }
                Ok(())
            })?;
            return Ok(false);
        }

        match self.client.revoke_ingress(key, &route) {
            Ok(()) => {
                self.store.with_transaction(|records| {
                    if records
                        .get(key)
                        .is_some_and(|r| r.attempt_id == expected_attempt_id)
                    {
                        records.remove(key);
                    }
                    Ok(())
                })?;
                Ok(true)
            }
            Err(Ec2ApiError::NotFound) => {
                self.store.with_transaction(|records| {
                    if let Some(record) = records.get_mut(key)
                        && record.attempt_id == expected_attempt_id
                    {
                        record.absent_checks = record.absent_checks.saturating_add(1);
                        if record.absent_checks >= 2 {
                            records.remove(key);
                        }
                    }
                    Ok(())
                })?;
                Ok(false)
            }
            Err(err) => bail!(
                "revoke EC2 ingress rule {}:{}/{} failed: {err}",
                key.security_group_id,
                key.port,
                key.cidr_ip
            ),
        }
    }

    /// Live reconciliation for `group_id`.
    ///
    /// Never prunes registered users against an external snapshot (so a
    /// concurrently acquired Move user can never be pruned). Only cleans up
    /// zero-user records, advances orphaned `Installing`/`Removing` records,
    /// and claims/revokes AWS rules carrying this daemon instance's description
    /// (`mj-move-grant:<daemon_instance_id>`) that have no live users.
    pub fn reconcile_group(
        &self,
        group_id: &str,
        route: &Ec2GrantRoute,
    ) -> Result<ReconcileReport> {
        self.reconcile_group_inner(group_id, route, None)
    }

    /// Startup reconciliation for `group_id`.
    ///
    /// Must run before live Move admission (or with a store-backed `is_active`
    /// predicate evaluated inside `with_transaction`). Prunes any recorded
    /// users for which `is_active(op_id)` returns `false`, then reconciles
    /// orphaned AWS rules and `Installing`/`Removing` records.
    pub fn reconcile_group_startup(
        &self,
        group_id: &str,
        route: &Ec2GrantRoute,
        is_active: impl Fn(&str) -> bool,
    ) -> Result<ReconcileReport> {
        self.reconcile_group_inner(group_id, route, Some(&is_active))
    }

    fn reconcile_group_inner(
        &self,
        group_id: &str,
        route: &Ec2GrantRoute,
        startup_is_active: Option<&dyn Fn(&str) -> bool>,
    ) -> Result<ReconcileReport> {
        let mut report = ReconcileReport::default();
        let our_desc = self.description();

        // 1. On startup reconciliation, prune inactive users inside the store
        // transaction; on live reconciliation, never prune registered users.
        // Always snapshot which `Installing` attempts existed BEFORE `describe`.
        let mut keys_to_revoke: Vec<(Ec2GrantKey, u64)> = Vec::new();
        let mut pre_describe_installing: BTreeMap<Ec2GrantKey, u64> = BTreeMap::new();
        self.store.with_transaction(|records| {
            let group_keys: Vec<Ec2GrantKey> = records
                .keys()
                .filter(|k| k.security_group_id == group_id)
                .cloned()
                .collect();
            for key in group_keys {
                let remove_record = if let Some(record) = records.get_mut(&key) {
                    record.route.fill_from(route);
                    if let Some(is_active) = startup_is_active {
                        record.users.retain(|op| is_active(op));
                    }
                    if record.state == Ec2GrantState::Installing {
                        pre_describe_installing.insert(key.clone(), record.attempt_id);
                    }
                    if record.users.is_empty() {
                        match record.state {
                            Ec2GrantState::External => true,
                            Ec2GrantState::Installed => {
                                record.state = Ec2GrantState::Removing;
                                record.absent_checks = 0;
                                keys_to_revoke.push((key.clone(), record.attempt_id));
                                false
                            }
                            Ec2GrantState::Installing | Ec2GrantState::Removing => false,
                        }
                    } else {
                        false
                    }
                } else {
                    false
                };
                if remove_record {
                    records.remove(&key);
                    report.dropped_records += 1;
                }
            }
            Ok(())
        })?;

        // 2. Query AWS for current rules in `group_id`.
        let aws_rules = self
            .client
            .describe_ingress_rules(group_id, route.region(), route.aws_profile())
            .map_err(|err| anyhow::anyhow!("describe EC2 security group {group_id}: {err}"))?;

        let our_aws_keys: BTreeSet<Ec2GrantKey> = aws_rules
            .iter()
            .filter(|rule| rule.description.as_deref() == Some(our_desc.as_str()))
            .map(|rule| Ec2GrantKey {
                security_group_id: group_id.to_owned(),
                cidr_ip: rule.cidr_ip.clone(),
                port: rule.port,
            })
            .collect();

        // 3. Reconcile recorded `Installing` / `Removing` and orphaned AWS rules
        // in one transaction while holding `in_flight_installs` (same lock order
        // as `acquire`: `in_flight_installs` -> `store`).
        let mut revoke_candidates: BTreeSet<(Ec2GrantKey, u64)> =
            keys_to_revoke.into_iter().collect();
        {
            let in_flight = self
                .coordination
                .in_flight_installs
                .lock()
                .expect("in_flight_installs lock");
            self.store.with_transaction(|records| {
                // Claim any AWS rule with our description that has no live user record.
                for key in &our_aws_keys {
                    match records.get_mut(key) {
                        None => {
                            let attempt_id = self.next_attempt_id(records);
                            records.insert(
                                key.clone(),
                                Ec2GrantRecord {
                                    key: key.clone(),
                                    state: Ec2GrantState::Removing,
                                    users: BTreeSet::new(),
                                    absent_checks: 0,
                                    attempt_id,
                                    route: route.clone(),
                                },
                            );
                            revoke_candidates.insert((key.clone(), attempt_id));
                        }
                        Some(record) if record.users.is_empty() => {
                            record.state = Ec2GrantState::Removing;
                            record.absent_checks = 0;
                            revoke_candidates.insert((key.clone(), record.attempt_id));
                        }
                        Some(_) => {}
                    }
                }

                // Advance recorded `Installing` and `Removing` records.
                let group_keys: Vec<Ec2GrantKey> = records
                    .keys()
                    .filter(|k| k.security_group_id == group_id)
                    .cloned()
                    .collect();
                for key in group_keys {
                    let present = our_aws_keys.contains(&key);
                    let mut drop_now = false;
                    if let Some(record) = records.get_mut(&key) {
                        match record.state {
                            Ec2GrantState::Installing => {
                                // Never clobber an `Installing` attempt that is
                                // still actively running in this daemon, or that
                                // was acquired after `describe_ingress_rules` began.
                                if in_flight.contains(&(key.clone(), record.attempt_id)) {
                                    continue;
                                }
                                if pre_describe_installing.get(&key) != Some(&record.attempt_id) {
                                    continue;
                                }
                                if present {
                                    if record.users.is_empty() {
                                        record.state = Ec2GrantState::Removing;
                                        record.absent_checks = 0;
                                        revoke_candidates.insert((key.clone(), record.attempt_id));
                                    } else {
                                        record.state = Ec2GrantState::Installed;
                                        record.absent_checks = 0;
                                        report.resolved_installed += 1;
                                    }
                                } else {
                                    record.state = Ec2GrantState::Removing;
                                    record.absent_checks = 1;
                                    report.remaining_pending += 1;
                                }
                            }
                            Ec2GrantState::Removing => {
                                let revoke_in_flight = self
                                    .coordination
                                    .in_flight_revokes
                                    .lock()
                                    .expect("in-flight revoke lock")
                                    .contains(&key);
                                if revoke_in_flight {
                                    // Keep the Removing reservation intact until
                                    // the active revoke completes so a concurrent
                                    // acquire cannot adopt the rule mid-revoke.
                                    report.remaining_pending += 1;
                                } else if present {
                                    record.absent_checks = 0;
                                    revoke_candidates.insert((key.clone(), record.attempt_id));
                                } else {
                                    record.absent_checks = record.absent_checks.saturating_add(1);
                                    if record.absent_checks >= 2 {
                                        drop_now = true;
                                    } else {
                                        report.remaining_pending += 1;
                                    }
                                }
                            }
                            Ec2GrantState::Installed | Ec2GrantState::External => {}
                        }
                    }
                    if drop_now {
                        records.remove(&key);
                        report.dropped_records += 1;
                    }
                }
                Ok(())
            })?;
        }

        // 4. Execute revokes outside the transaction, serialized per `(key, attempt_id)`.
        for (key, attempt_id) in revoke_candidates {
            if self.revoke_confirmed_and_delete(&key, attempt_id)? {
                report.revoked_rules += 1;
            }
        }

        Ok(report)
    }
}

fn find_matching_rule<'a>(
    rules: &'a [Ec2IngressRule],
    key: &Ec2GrantKey,
) -> Option<&'a Ec2IngressRule> {
    rules
        .iter()
        .find(|rule| rule.port == key.port && rule.cidr_ip == key.cidr_ip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Default)]
    struct FakeEc2State {
        rules: BTreeMap<String, Vec<Ec2IngressRule>>,
        authorize_calls: usize,
        revoke_calls: usize,
        fail_next_authorize_after_creating: bool,
        fail_next_authorize_without_creating: bool,
        force_next_authorize_duplicate: bool,
        on_describe: Option<Box<dyn Fn() + Send>>,
        /// Runs once at the start of the next `authorize_ingress`, with the
        /// fake's lock released, before the rule exists.
        on_authorize: Option<Box<dyn Fn() + Send>>,
        /// `(operation, group, region, aws_profile)` for every scoped call.
        scopes: Vec<(&'static str, String, Option<String>, Option<String>)>,
        instances: BTreeMap<String, Ec2InstanceNetwork>,
    }

    #[derive(Clone, Default)]
    struct FakeEc2Client {
        state: Arc<Mutex<FakeEc2State>>,
    }

    impl FakeEc2Client {
        fn add_rule(&self, group_id: &str, rule: Ec2IngressRule) {
            let mut st = self.state.lock().expect("fake lock");
            st.rules.entry(group_id.to_owned()).or_default().push(rule);
        }

        fn rules_for(&self, group_id: &str) -> Vec<Ec2IngressRule> {
            self.state
                .lock()
                .expect("fake lock")
                .rules
                .get(group_id)
                .cloned()
                .unwrap_or_default()
        }

        fn revoke_calls(&self) -> usize {
            self.state.lock().expect("fake lock").revoke_calls
        }

        fn scopes(&self) -> Vec<(&'static str, String, Option<String>, Option<String>)> {
            self.state.lock().expect("fake lock").scopes.clone()
        }
    }

    impl Ec2SecurityGroupClient for FakeEc2Client {
        fn authorize_ingress(
            &self,
            key: &Ec2GrantKey,
            route: &Ec2GrantRoute,
            description: &str,
        ) -> std::result::Result<(), Ec2ApiError> {
            let (group_id, port, cidr_ip) = (&key.security_group_id, key.port, &key.cidr_ip);
            let hook = self.state.lock().expect("fake lock").on_authorize.take();
            if let Some(hook) = hook {
                hook();
            }
            let mut st = self.state.lock().expect("fake lock");
            st.scopes.push((
                "authorize",
                group_id.clone(),
                route.region.clone(),
                route.aws_profile.clone(),
            ));
            st.authorize_calls += 1;
            if st.force_next_authorize_duplicate {
                st.force_next_authorize_duplicate = false;
                return Err(Ec2ApiError::Duplicate);
            }
            if st.fail_next_authorize_without_creating {
                st.fail_next_authorize_without_creating = false;
                return Err(Ec2ApiError::Other("simulated timeout".into()));
            }
            let list = st.rules.entry(group_id.clone()).or_default();
            if list.iter().any(|r| r.port == port && &r.cidr_ip == cidr_ip) {
                return Err(Ec2ApiError::Duplicate);
            }
            list.push(Ec2IngressRule {
                port,
                cidr_ip: cidr_ip.clone(),
                description: Some(description.to_owned()),
            });
            if st.fail_next_authorize_after_creating {
                st.fail_next_authorize_after_creating = false;
                return Err(Ec2ApiError::Other("simulated lost response".into()));
            }
            Ok(())
        }

        fn revoke_ingress(
            &self,
            key: &Ec2GrantKey,
            route: &Ec2GrantRoute,
        ) -> std::result::Result<(), Ec2ApiError> {
            let (group_id, port, cidr_ip) = (&key.security_group_id, key.port, &key.cidr_ip);
            let mut st = self.state.lock().expect("fake lock");
            st.scopes.push((
                "revoke",
                group_id.clone(),
                route.region.clone(),
                route.aws_profile.clone(),
            ));
            st.revoke_calls += 1;
            let list = st.rules.entry(group_id.clone()).or_default();
            let before = list.len();
            list.retain(|r| !(r.port == port && &r.cidr_ip == cidr_ip));
            if list.len() == before {
                return Err(Ec2ApiError::NotFound);
            }
            Ok(())
        }

        fn describe_ingress_rules(
            &self,
            group_id: &str,
            region: Option<&str>,
            aws_profile: Option<&str>,
        ) -> std::result::Result<Vec<Ec2IngressRule>, Ec2ApiError> {
            let hook = {
                let mut st = self.state.lock().expect("fake lock");
                st.scopes.push((
                    "describe",
                    group_id.to_owned(),
                    region.map(ToOwned::to_owned),
                    aws_profile.map(ToOwned::to_owned),
                ));
                st.on_describe.take()
            };
            if let Some(f) = hook {
                f();
            }
            Ok(self.rules_for(group_id))
        }

        fn describe_instance(
            &self,
            instance_id: &str,
            _region: Option<&str>,
            _aws_profile: Option<&str>,
        ) -> std::result::Result<Ec2InstanceNetwork, Ec2ApiError> {
            self.state
                .lock()
                .expect("fake lock")
                .instances
                .get(instance_id)
                .cloned()
                .ok_or_else(|| Ec2ApiError::Other(format!("no fake instance {instance_id}")))
        }
    }

    fn target(key: &Ec2GrantKey) -> Ec2GrantTarget {
        Ec2GrantTarget::new(key.clone(), Ec2GrantRoute::default())
    }

    fn fast_config() -> Ec2GrantConfig {
        Ec2GrantConfig {
            backoff: Duration::from_millis(1),
            max_wait_attempts: 5,
        }
    }

    #[test]
    fn concurrent_sharers_revoke_only_when_last_user_releases() {
        let store = InMemoryGrantStore::new();
        let client = FakeEc2Client::default();
        let mgr = Ec2GrantManager::new(store, client.clone(), "daemon-a", fast_config());
        let key = Ec2GrantKey::ssh("sg-1", "10.0.0.5/32");

        assert_eq!(
            mgr.acquire(&target(&key), "op-1").expect("acquire 1"),
            AcquireOutcome::Granted
        );
        assert_eq!(
            mgr.acquire(&target(&key), "op-2").expect("acquire 2"),
            AcquireOutcome::Granted
        );
        assert_eq!(client.rules_for("sg-1").len(), 1);
        assert_eq!(client.revoke_calls(), 0);

        mgr.release(&key, "op-1").expect("release 1");
        assert_eq!(
            client.rules_for("sg-1").len(),
            1,
            "rule must remain while op-2 holds it"
        );
        assert_eq!(client.revoke_calls(), 0);

        mgr.release(&key, "op-2").expect("release 2");
        assert!(
            client.rules_for("sg-1").is_empty(),
            "rule must be revoked on last release"
        );
        assert_eq!(client.revoke_calls(), 1);
        assert!(mgr.store().snapshot().is_empty());
    }

    #[test]
    fn external_preexisting_rule_is_never_revoked() {
        let store = InMemoryGrantStore::new();
        let client = FakeEc2Client::default();
        client.add_rule(
            "sg-ext",
            Ec2IngressRule {
                port: 22,
                cidr_ip: "10.0.0.9/32".into(),
                description: Some("user-managed-ssh".into()),
            },
        );
        let mgr = Ec2GrantManager::new(store, client.clone(), "daemon-a", fast_config());
        let key = Ec2GrantKey::ssh("sg-ext", "10.0.0.9/32");

        assert_eq!(
            mgr.acquire(&target(&key), "op-1").expect("acquire"),
            AcquireOutcome::Granted
        );
        assert_eq!(
            mgr.store().snapshot().get(&key).map(|r| r.state),
            Some(Ec2GrantState::External)
        );

        mgr.release(&key, "op-1").expect("release");
        assert_eq!(
            client.rules_for("sg-ext").len(),
            1,
            "external rule must never be revoked"
        );
        assert_eq!(client.revoke_calls(), 0);
        assert!(mgr.store().snapshot().is_empty());
    }

    #[test]
    fn ambiguous_install_failure_with_no_users_reconciles_to_revoked() {
        let store = InMemoryGrantStore::new();
        let client = FakeEc2Client::default();
        // Simulate a rule that AWS created late while the record sat in
        // `Installing` with 0 remaining users (the move fell back to daemon stream).
        client.add_rule(
            "sg-ambig",
            Ec2IngressRule {
                port: 22,
                cidr_ip: "10.0.0.12/32".into(),
                description: Some(grant_description("daemon-a")),
            },
        );
        let key = Ec2GrantKey::ssh("sg-ambig", "10.0.0.12/32");
        store
            .with_transaction(|records| {
                records.insert(
                    key.clone(),
                    Ec2GrantRecord {
                        key: key.clone(),
                        state: Ec2GrantState::Installing,
                        users: BTreeSet::new(),
                        absent_checks: 0,
                        attempt_id: 1,
                        route: Default::default(),
                    },
                );
                Ok(())
            })
            .expect("seed installing");

        let mgr = Ec2GrantManager::new(store, client.clone(), "daemon-a", fast_config());
        let report = mgr
            .reconcile_group("sg-ambig", &Ec2GrantRoute::default())
            .expect("reconcile");
        assert_eq!(report.revoked_rules, 1);
        assert!(client.rules_for("sg-ambig").is_empty());
        assert!(mgr.store().snapshot().is_empty());
    }

    #[test]
    fn instance_scoped_orphan_reconciliation_ignores_other_instances() {
        let store = InMemoryGrantStore::new();
        let client = FakeEc2Client::default();
        client.add_rule(
            "sg-shared",
            Ec2IngressRule {
                port: 22,
                cidr_ip: "10.0.0.21/32".into(),
                description: Some(grant_description("daemon-a")),
            },
        );
        client.add_rule(
            "sg-shared",
            Ec2IngressRule {
                port: 22,
                cidr_ip: "10.0.0.22/32".into(),
                description: Some(grant_description("daemon-b")),
            },
        );
        let mgr = Ec2GrantManager::new(store, client.clone(), "daemon-a", fast_config());

        let report = mgr
            .reconcile_group("sg-shared", &Ec2GrantRoute::default())
            .expect("reconcile");
        assert_eq!(report.revoked_rules, 1);

        let remaining = client.rules_for("sg-shared");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].cidr_ip, "10.0.0.22/32");
        assert_eq!(
            remaining[0].description.as_deref(),
            Some("mj-move-grant:daemon-b")
        );
    }

    #[test]
    fn ambiguous_absent_install_requires_two_separated_checks_to_drop() {
        let store = InMemoryGrantStore::new();
        let client = FakeEc2Client::default();
        client
            .state
            .lock()
            .unwrap()
            .fail_next_authorize_without_creating = true;
        let mgr = Ec2GrantManager::new(store, client.clone(), "daemon-a", fast_config());
        let key = Ec2GrantKey::ssh("sg-abs", "10.0.0.30/32");

        let outcome = mgr.acquire(&target(&key), "op-1").expect("acquire");
        assert_eq!(outcome, AcquireOutcome::FallbackToDaemonStream);
        // After 1st absent check, record must still be in `Removing` (not dropped).
        let snap = mgr.store().snapshot();
        assert_eq!(
            snap.get(&key).map(|r| r.state),
            Some(Ec2GrantState::Removing)
        );
        assert_eq!(snap.get(&key).map(|r| r.absent_checks), Some(1));

        // Second separated reconcile check confirms absence and drops the record.
        let report = mgr
            .reconcile_group("sg-abs", &Ec2GrantRoute::default())
            .expect("reconcile 2");
        assert_eq!(report.dropped_records, 1);
        assert!(mgr.store().snapshot().is_empty());
    }

    #[test]
    fn duplicate_with_empty_describe_never_revokes_unknown_rule() {
        let store = InMemoryGrantStore::new();
        let client = FakeEc2Client::default();
        client.state.lock().unwrap().force_next_authorize_duplicate = true;
        let mgr = Ec2GrantManager::new(store, client.clone(), "daemon-a", fast_config());
        let key = Ec2GrantKey::ssh("sg-dup", "10.0.0.40/32");

        let outcome = mgr.acquire(&target(&key), "op-1").expect("acquire");
        assert_eq!(outcome, AcquireOutcome::FallbackToDaemonStream);
        assert_eq!(
            client.revoke_calls(),
            0,
            "Duplicate + empty describe must NEVER invoke revoke_ingress"
        );
        assert_eq!(
            mgr.store().snapshot().get(&key).map(|r| r.state),
            Some(Ec2GrantState::Installing)
        );
    }

    #[test]
    fn reconcile_does_not_clobber_grant_acquired_during_describe() {
        let store = Arc::new(InMemoryGrantStore::new());
        let client = FakeEc2Client::default();
        let new_key = Ec2GrantKey::ssh("sg-race", "10.0.0.50/32");

        // While `describe_ingress_rules` is in flight, simulate a concurrent
        // `acquire` inserting a brand-new `Installing` record into the store.
        let store_for_hook = Arc::clone(&store);
        let key_for_hook = new_key.clone();
        client.state.lock().unwrap().on_describe = Some(Box::new(move || {
            store_for_hook
                .with_transaction(|records| {
                    let mut users = BTreeSet::new();
                    users.insert("op-concurrent".to_owned());
                    records.insert(
                        key_for_hook.clone(),
                        Ec2GrantRecord {
                            key: key_for_hook.clone(),
                            state: Ec2GrantState::Installing,
                            users,
                            absent_checks: 0,
                            attempt_id: 99,
                            route: Default::default(),
                        },
                    );
                    Ok(())
                })
                .expect("insert concurrent grant");
        }));

        // Wrap Arc<InMemoryGrantStore> via a thin forwarding adapter.
        struct SharedStore(Arc<InMemoryGrantStore>);
        impl Ec2GrantStore for SharedStore {
            fn coordination(&self) -> Arc<Ec2GrantCoordination> {
                self.0.coordination()
            }

            fn with_transaction<R, F>(&self, f: F) -> Result<R>
            where
                F: FnOnce(&mut BTreeMap<Ec2GrantKey, Ec2GrantRecord>) -> Result<R>,
            {
                self.0.with_transaction(f)
            }
        }

        let mgr = Ec2GrantManager::new(
            SharedStore(Arc::clone(&store)),
            client.clone(),
            "daemon-a",
            fast_config(),
        );
        mgr.reconcile_group("sg-race", &Ec2GrantRoute::default())
            .expect("reconcile");

        assert_eq!(
            store.snapshot().get(&new_key).map(|r| r.state),
            Some(Ec2GrantState::Installing),
            "grant acquired during describe must not be clobbered to Removing"
        );
        assert_eq!(client.revoke_calls(), 0);
    }

    #[test]
    fn concurrent_reconcile_skips_in_flight_revoke_and_preserves_reacquired_grant() {
        let store = InMemoryGrantStore::new();
        let client = FakeEc2Client::default();
        let key = Ec2GrantKey::ssh("sg-rev", "10.0.0.60/32");
        client.add_rule(
            "sg-rev",
            Ec2IngressRule {
                port: 22,
                cidr_ip: "10.0.0.60/32".into(),
                description: Some(grant_description("daemon-a")),
            },
        );
        store
            .with_transaction(|records| {
                records.insert(
                    key.clone(),
                    Ec2GrantRecord {
                        key: key.clone(),
                        state: Ec2GrantState::Removing,
                        users: BTreeSet::new(),
                        absent_checks: 0,
                        attempt_id: 7,
                        route: Default::default(),
                    },
                );
                Ok(())
            })
            .expect("seed removing");

        let mgr = Ec2GrantManager::new(store, client.clone(), "daemon-a", fast_config());
        // Simulate another task currently revoking `key`.
        mgr.coordination
            .in_flight_revokes
            .lock()
            .unwrap()
            .insert(key.clone());

        let report = mgr
            .reconcile_group("sg-rev", &Ec2GrantRoute::default())
            .expect("concurrent reconcile");
        assert_eq!(
            report.revoked_rules, 0,
            "second reconciler must skip key already in in_flight_revokes"
        );
        assert_eq!(client.revoke_calls(), 0);

        // Even if AWS describes the rule as absent across multiple passes while
        // the revoke is still in flight, reconciliation must keep the `Removing`
        // reservation intact so a concurrent acquire cannot race the revoke.
        let saved_rules = std::mem::take(&mut client.state.lock().unwrap().rules);
        for _ in 0..3 {
            let absent_report = mgr
                .reconcile_group("sg-rev", &Ec2GrantRoute::default())
                .expect("reconcile while absent and revoking");
            assert_eq!(absent_report.dropped_records, 0);
            assert_eq!(absent_report.remaining_pending, 1);
        }
        let still_reserved = mgr
            .store()
            .with_transaction(|records| {
                Ok(records
                    .get(&key)
                    .is_some_and(|r| r.state == Ec2GrantState::Removing && r.absent_checks == 0))
            })
            .unwrap();
        assert!(
            still_reserved,
            "Removing grant must remain reserved while revoke is in flight"
        );
        client.state.lock().unwrap().rules = saved_rules;

        // Now finish the first revoke, re-acquire a fresh grant (generation 42 != 7),
        // and verify a stale revoke for generation 7 makes ZERO AWS revoke calls
        // and leaves the new `Installed` grant intact.
        mgr.coordination.in_flight_revokes.lock().unwrap().clear();
        mgr.store()
            .with_transaction(|records| {
                let mut users = BTreeSet::new();
                users.insert("op-new".to_owned());
                records.insert(
                    key.clone(),
                    Ec2GrantRecord {
                        key: key.clone(),
                        state: Ec2GrantState::Installed,
                        users,
                        absent_checks: 0,
                        attempt_id: 42,
                        route: Default::default(),
                    },
                );
                Ok(())
            })
            .expect("seed reacquired grant");

        let revoked = mgr
            .revoke_confirmed_and_delete(&key, 7)
            .expect("stale revoke returns Ok(false)");
        assert!(!revoked);
        assert_eq!(
            client.revoke_calls(),
            0,
            "stale revoke for old generation must make ZERO AWS revoke calls"
        );
        assert_eq!(
            mgr.store().snapshot().get(&key).map(|r| r.attempt_id),
            Some(42),
            "stale revoke for old generation must not delete re-acquired grant"
        );
        assert_eq!(client.rules_for("sg-rev").len(), 1);
    }

    #[test]
    fn timed_out_waiter_revokes_newly_installed_unused_grant() {
        let store = InMemoryGrantStore::new();
        let client = FakeEc2Client::default();
        let key = Ec2GrantKey::ssh("sg-wait", "10.0.0.70/32");
        client.add_rule(
            "sg-wait",
            Ec2IngressRule {
                port: 22,
                cidr_ip: "10.0.0.70/32".into(),
                description: Some(grant_description("daemon-a")),
            },
        );
        // Seed an `Installing` grant owned by `op-installer` with `max_wait_attempts = 0`
        // so the waiter registers on attempt 1, then immediately hits timeout.
        store
            .with_transaction(|records| {
                let mut users = BTreeSet::new();
                users.insert("op-installer".to_owned());
                records.insert(
                    key.clone(),
                    Ec2GrantRecord {
                        key: key.clone(),
                        state: Ec2GrantState::Installing,
                        users,
                        absent_checks: 0,
                        attempt_id: 1,
                        route: Default::default(),
                    },
                );
                Ok(())
            })
            .expect("seed installing");

        // Use a custom store wrapper that transitions the grant to `Installed`
        // (and removes `op-installer`) right after `op-waiter` registers,
        // simulating the installer finishing & releasing right before waiter timeout.
        struct RaceStore {
            inner: InMemoryGrantStore,
            key: Ec2GrantKey,
        }
        impl Ec2GrantStore for RaceStore {
            fn coordination(&self) -> Arc<Ec2GrantCoordination> {
                self.inner.coordination()
            }

            fn with_transaction<R, F>(&self, f: F) -> Result<R>
            where
                F: FnOnce(&mut BTreeMap<Ec2GrantKey, Ec2GrantRecord>) -> Result<R>,
            {
                self.inner.with_transaction(|records| {
                    let res = f(records)?;
                    if let Some(rec) = records.get_mut(&self.key)
                        && rec.state == Ec2GrantState::Installing
                        && rec.users.contains("op-waiter")
                    {
                        rec.users.remove("op-installer");
                        rec.state = Ec2GrantState::Installed;
                    }
                    Ok(res)
                })
            }
        }

        let mgr = Ec2GrantManager::new(
            RaceStore {
                inner: store,
                key: key.clone(),
            },
            client.clone(),
            "daemon-a",
            Ec2GrantConfig {
                backoff: Duration::from_millis(1),
                max_wait_attempts: 0,
            },
        );

        let outcome = mgr
            .acquire(&target(&key), "op-waiter")
            .expect("waiter acquire");
        assert_eq!(outcome, AcquireOutcome::FallbackToDaemonStream);
        assert!(
            client.rules_for("sg-wait").is_empty(),
            "timed-out waiter must revoke now-unused Installed rule"
        );
        assert_eq!(client.revoke_calls(), 1);
        assert!(mgr.store().inner.snapshot().is_empty());
    }

    #[test]
    fn live_reconcile_never_prunes_active_users_while_startup_prunes_dead_users() {
        let store = InMemoryGrantStore::new();
        let client = FakeEc2Client::default();
        let mgr = Ec2GrantManager::new(store, client.clone(), "daemon-a", fast_config());
        let key = Ec2GrantKey::ssh("sg-live", "10.0.0.80/32");

        assert_eq!(
            mgr.acquire(&target(&key), "op-live").expect("acquire"),
            AcquireOutcome::Granted
        );

        // Live reconciliation must never prune `op-live` or revoke its rule.
        let live_report = mgr
            .reconcile_group("sg-live", &Ec2GrantRoute::default())
            .expect("live reconcile");
        assert_eq!(live_report.revoked_rules, 0);
        assert_eq!(client.rules_for("sg-live").len(), 1);
        assert!(
            mgr.store()
                .snapshot()
                .get(&key)
                .is_some_and(|r| r.users.contains("op-live"))
        );

        // Startup reconciliation with `is_active = false` prunes the dead op and revokes.
        let startup_report = mgr
            .reconcile_group_startup("sg-live", &Ec2GrantRoute::default(), |_| false)
            .expect("startup reconcile");
        assert_eq!(startup_report.revoked_rules, 1);
        assert!(client.rules_for("sg-live").is_empty());
        assert!(mgr.store().snapshot().is_empty());
    }

    #[test]
    fn sqlite_ec2_grant_store_commits_and_rolls_back_atomically() {
        let name = crate::controller::test_support::test_name(
            module_path!(),
            "sqlite_ec2_grant_store_commits_and_rolls_back_atomically",
        );
        if !crate::controller::move_session::test_fixtures::isolated_test_child(
            &name,
            "MJ_SQLITE_EC2_GRANT_CHILD",
        ) {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let store = SqliteEc2GrantStore;
        let key = Ec2GrantKey::ssh("sg-sqlite", "10.0.1.5/32");

        // Commit a grant row atomically.
        store
            .with_transaction(|map| {
                map.insert(
                    key.clone(),
                    Ec2GrantRecord {
                        key: key.clone(),
                        state: Ec2GrantState::Installed,
                        users: BTreeSet::from(["op-commit".to_owned()]),
                        absent_checks: 0,
                        attempt_id: 1,
                        route: Default::default(),
                    },
                );
                Ok(())
            })
            .expect("commit grant");

        // Return Err after mutating: SQLite must roll back and retain the committed row.
        let err = store
            .with_transaction(|map| -> Result<()> {
                map.get_mut(&key)
                    .unwrap()
                    .users
                    .insert("op-rolled-back".into());
                bail!("simulated rollback")
            })
            .unwrap_err();
        assert!(err.to_string().contains("simulated rollback"));

        // Verify only the committed user persisted.
        let users = store
            .with_transaction(|map| Ok(map.get(&key).map(|rec| rec.users.clone())))
            .expect("read grant");
        assert_eq!(users, Some(BTreeSet::from(["op-commit".to_owned()])));

        // The route is part of the record, so it survives the SQLite round trip.
        let regional = Ec2GrantKey::ssh("sg-regional", "10.0.2.5/32");
        let route = Ec2GrantRoute::new(Some("eu-west-1".into()), Some("team".into()));
        store
            .with_transaction(|map| {
                map.insert(
                    regional.clone(),
                    Ec2GrantRecord {
                        key: regional.clone(),
                        state: Ec2GrantState::Installed,
                        users: BTreeSet::from(["op-regional".to_owned()]),
                        absent_checks: 0,
                        attempt_id: 2,
                        route: route.clone(),
                    },
                );
                Ok(())
            })
            .expect("commit regional grant");
        let reloaded = store
            .with_transaction(|map| Ok(map.get(&regional).map(|r| r.route.clone())))
            .expect("reload grants");
        assert_eq!(reloaded, Some(route));
        assert!(Arc::ptr_eq(
            &SqliteEc2GrantStore.coordination(),
            &SqliteEc2GrantStore.coordination()
        ));
    }

    /// One rule reached with different routes is one record with one set of
    /// users: the first non-empty route is kept and the rule is revoked only
    /// when its last user leaves.
    fn shared_rule_scenario(store: impl Ec2GrantStore) {
        let client = FakeEc2Client::default();
        let mgr = Ec2GrantManager::new(store, client.clone(), "daemon-a", fast_config());
        let key = Ec2GrantKey::ssh("sg-shared-route", "10.0.0.5/32");
        let east = Ec2GrantRoute::new(Some("us-east-1".into()), Some("team".into()));
        let west = Ec2GrantRoute::new(Some("eu-west-1".into()), Some("other".into()));
        let acquire = |route: &Ec2GrantRoute, op: &str| {
            mgr.acquire(&Ec2GrantTarget::new(key.clone(), route.clone()), op)
                .unwrap()
        };
        // A legacy record without a route, then an explicit route, then another.
        assert_eq!(
            acquire(&Ec2GrantRoute::default(), "op-legacy"),
            AcquireOutcome::Granted
        );
        assert_eq!(acquire(&east, "op-east"), AcquireOutcome::Granted);
        assert_eq!(acquire(&west, "op-west"), AcquireOutcome::Granted);

        let records = mgr.store().with_transaction(|map| Ok(map.clone())).unwrap();
        assert_eq!(records.len(), 1, "one rule is one record: {records:?}");
        let record = &records[&key];
        assert_eq!(
            record.users,
            BTreeSet::from(["op-legacy".into(), "op-east".into(), "op-west".into()])
        );
        assert_eq!(
            record.route, east,
            "a later caller cannot reroute the record"
        );
        assert_eq!(client.rules_for("sg-shared-route").len(), 1);

        mgr.release(&key, "op-legacy").unwrap();
        mgr.release(&key, "op-east").unwrap();
        assert_eq!(client.revoke_calls(), 0, "a user is still active");
        assert_eq!(client.rules_for("sg-shared-route").len(), 1);
        mgr.release(&key, "op-west").unwrap();
        assert_eq!(client.revoke_calls(), 1);
        assert!(client.rules_for("sg-shared-route").is_empty());
        let revoked = client
            .scopes()
            .into_iter()
            .find(|(operation, ..)| *operation == "revoke")
            .unwrap();
        assert_eq!(
            (revoked.2.as_deref(), revoked.3.as_deref()),
            (Some("us-east-1"), Some("team")),
            "AWS calls follow the record's route"
        );
    }

    #[test]
    fn in_memory_rule_reached_with_different_routes_shares_one_record_and_user_set() {
        shared_rule_scenario(InMemoryGrantStore::new());
    }

    #[test]
    fn sqlite_rule_reached_with_different_routes_shares_one_record_and_user_set() {
        let name = crate::controller::test_support::test_name(
            module_path!(),
            "sqlite_rule_reached_with_different_routes_shares_one_record_and_user_set",
        );
        if !crate::controller::move_session::test_fixtures::isolated_test_child(
            &name,
            "MJ_SQLITE_EC2_SHARED_ROUTE_CHILD",
        ) {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        shared_rule_scenario(SqliteEc2GrantStore);
    }

    /// A reconciler beside a live Move (two managers, one store) must see the
    /// Move's pending `authorize-security-group-ingress` and leave its
    /// `Installing` record alone, even though AWS does not show the rule yet.
    #[test]
    fn reconciler_built_beside_a_live_install_does_not_remove_its_installing_record() {
        struct SharedStore(Arc<InMemoryGrantStore>);
        impl Ec2GrantStore for SharedStore {
            fn coordination(&self) -> Arc<Ec2GrantCoordination> {
                self.0.coordination()
            }
            fn with_transaction<R, F>(&self, f: F) -> Result<R>
            where
                F: FnOnce(&mut BTreeMap<Ec2GrantKey, Ec2GrantRecord>) -> Result<R>,
            {
                self.0.with_transaction(f)
            }
        }
        let store = Arc::new(InMemoryGrantStore::new());
        let client = FakeEc2Client::default();
        let (in_authorize_tx, in_authorize) = std::sync::mpsc::channel::<()>();
        let (proceed_tx, proceed) = std::sync::mpsc::channel::<()>();
        client.state.lock().unwrap().on_authorize = Some(Box::new({
            let in_authorize_tx = Mutex::new(in_authorize_tx);
            let proceed = Mutex::new(proceed);
            move || {
                in_authorize_tx.lock().unwrap().send(()).unwrap();
                proceed.lock().unwrap().recv().unwrap();
            }
        }));
        let key = Ec2GrantKey::ssh("sg-live-install", "10.0.0.70/32");
        let installer = Ec2GrantManager::new(
            SharedStore(store.clone()),
            client.clone(),
            "daemon-a",
            fast_config(),
        );
        let reconciler = Ec2GrantManager::new(
            SharedStore(store.clone()),
            client.clone(),
            "daemon-a",
            fast_config(),
        );
        std::thread::scope(|scope| {
            let acquired = scope.spawn(|| installer.acquire(&target(&key), "op-live").unwrap());
            in_authorize.recv().unwrap();
            let before = store.snapshot()[&key].clone();
            assert_eq!(before.state, Ec2GrantState::Installing);

            let report = reconciler
                .reconcile_group("sg-live-install", &Ec2GrantRoute::default())
                .unwrap();
            let after = store.snapshot()[&key].clone();
            assert_eq!(after.state, Ec2GrantState::Installing, "{report:?}");
            assert_eq!(after.attempt_id, before.attempt_id);
            assert_eq!(after.users, before.users);
            assert_eq!(client.revoke_calls(), 0);

            proceed_tx.send(()).unwrap();
            assert_eq!(acquired.join().unwrap(), AcquireOutcome::Granted);
        });
        assert_eq!(store.snapshot()[&key].state, Ec2GrantState::Installed);
    }

    #[test]
    fn attempt_ids_never_reuse_a_stored_or_handed_out_generation() {
        let coordination = Ec2GrantCoordination::default();
        assert_eq!(coordination.next_attempt_id(0), 1);
        assert_eq!(coordination.next_attempt_id(0), 2);
        assert_eq!(coordination.next_attempt_id(40), 41, "above a persisted id");
        assert_eq!(coordination.next_attempt_id(3), 42);
    }

    #[test]
    fn grant_calls_carry_the_records_region_and_profile_through_acquire_release_and_reconcile() {
        let client = FakeEc2Client::default();
        let mgr = Ec2GrantManager::new(
            InMemoryGrantStore::new(),
            client.clone(),
            "daemon-a",
            fast_config(),
        );
        let key = Ec2GrantKey::ssh("sg-east", "10.0.0.5/32");
        let route = Ec2GrantRoute::new(Some("us-east-2".into()), Some("team".into()));
        assert_eq!(
            mgr.acquire(&Ec2GrantTarget::new(key.clone(), route.clone()), "op-1")
                .unwrap(),
            AcquireOutcome::Granted
        );
        mgr.release(&key, "op-1").unwrap();
        mgr.reconcile_group("sg-east", &route).unwrap();
        let scope = |operation: &'static str| {
            (
                operation,
                "sg-east".to_owned(),
                Some("us-east-2".to_owned()),
                Some("team".to_owned()),
            )
        };
        let scopes = client.scopes();
        assert!(!scopes.is_empty());
        for expected in ["authorize", "describe", "revoke"] {
            assert!(scopes.contains(&scope(expected)), "{expected}: {scopes:?}");
        }
        assert!(
            scopes.iter().all(
                |(_, _, region, profile)| region.as_deref() == Some("us-east-2")
                    && profile.as_deref() == Some("team")
            ),
            "{scopes:?}"
        );
    }
}
