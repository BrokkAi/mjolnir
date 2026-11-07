//! The IAM role that narrows the AWS credentials a Bedrock profile gives sessions.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Permissions to invoke and look up Bedrock models, used as the session
/// policy when a profile names a role and no policy of its own. A session
/// policy can only remove permissions from the role, never add them, so this
/// is a narrowing: the role's own Bedrock resource limits still apply.
///
/// Claude Code needs the two invoke actions; Codex needs `InvokeModel`. The
/// list and get actions let a harness discover models and inference profiles.
pub const DEFAULT_BEDROCK_SESSION_POLICY: &str = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["bedrock:InvokeModel","bedrock:InvokeModelWithResponseStream","bedrock:ListInferenceProfiles","bedrock:GetInferenceProfile","bedrock:ListFoundationModels","bedrock:GetFoundationModel"],"Resource":"*"}]}"#;

/// `[profiles.<id>.aws]`: the role Mjolnir assumes for each session of a
/// profile that authenticates with the AWS credential chain (Bedrock), so the
/// session receives short-lived credentials limited to that role instead of a
/// copy of the controller host's own credentials.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsRoleConfig {
    /// The role to assume, `arn:aws:iam::<account>:role/<name>`. The host's
    /// AWS login must be allowed to assume it.
    pub role_arn: String,
    /// An inline session policy (JSON text) that narrows the role further.
    /// With neither this nor `session_policy_arns`, a built-in Bedrock
    /// invocation policy is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_policy: Option<String>,
    /// Managed policies (ARNs) that narrow the role further.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_policy_arns: Vec<String>,
    /// Seconds each set of credentials lasts. Unset leaves the choice to AWS,
    /// whose default is 3600 and is valid for every role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u32>,
    /// Also set the session id as the AWS `SourceIdentity`, which CloudTrail
    /// records and which survives role chaining. The role's trust policy must
    /// then allow `sts:SetSourceIdentity`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub source_identity: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl AwsRoleConfig {
    /// Shape checks only. Policy sizes, the duration range and whether the
    /// role exists are limits AWS owns and reports when it refuses a request.
    pub fn validate(&self, id: &str) -> Result<()> {
        if !valid_role_arn(&self.role_arn) {
            bail!(
                "profile {id:?}: [profiles.{id}.aws] role_arn {:?} is not a role ARN such as arn:aws:iam::123456789012:role/name",
                self.role_arn
            );
        }
        if let Some(policy) = &self.session_policy
            && !serde_json::from_str::<serde_json::Value>(policy)
                .is_ok_and(|value| value.is_object())
        {
            bail!(
                "profile {id:?}: [profiles.{id}.aws] session_policy is not a JSON policy document"
            );
        }
        if let Some(arn) = self
            .session_policy_arns
            .iter()
            .find(|arn| !arn.starts_with("arn:"))
        {
            bail!(
                "profile {id:?}: [profiles.{id}.aws] session_policy_arns entry {arn:?} is not an ARN"
            );
        }
        Ok(())
    }

    /// The inline session policy to send, or `None` when only managed
    /// policies narrow the role.
    pub fn effective_session_policy(&self) -> Option<&str> {
        match (&self.session_policy, self.session_policy_arns.is_empty()) {
            (Some(policy), _) => Some(policy),
            (None, true) => Some(DEFAULT_BEDROCK_SESSION_POLICY),
            (None, false) => None,
        }
    }
}

/// `arn:<partition>:iam::<12 digits>:role/<path and name>`.
fn valid_role_arn(arn: &str) -> bool {
    let mut parts = arn.splitn(6, ':');
    let (Some("arn"), Some(partition), Some("iam"), Some(""), Some(account), Some(resource)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return false;
    };
    !partition.is_empty()
        && account.len() == 12
        && account.bytes().all(|byte| byte.is_ascii_digit())
        && resource
            .strip_prefix("role/")
            .is_some_and(|name| !name.is_empty() && !name.chars().any(char::is_whitespace))
}
