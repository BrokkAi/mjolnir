// Port of mj-core/src/config/aws.rs.
//
// The IAM role that narrows the AWS credentials a Bedrock profile gives
// sessions.

package core

import (
	"encoding/json"
	"fmt"
	"slices"
	"strings"
	"unicode"
)

// DefaultBedrockSessionPolicy grants the permissions to invoke and look up
// Bedrock models, used as the session policy when a profile names a role and
// no policy of its own. A session policy can only remove permissions from the
// role, never add them, so this is a narrowing: the role's own Bedrock
// resource limits still apply.
//
// Claude Code needs the two invoke actions; Codex needs InvokeModel. The list
// and get actions let a harness discover models and inference profiles.
const DefaultBedrockSessionPolicy = `{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["bedrock:InvokeModel","bedrock:InvokeModelWithResponseStream","bedrock:ListInferenceProfiles","bedrock:GetInferenceProfile","bedrock:ListFoundationModels","bedrock:GetFoundationModel"],"Resource":"*"}]}`

// AwsRoleConfig is [profiles.<id>.aws]: the role Mjolnir assumes for each
// session of a profile that authenticates with the AWS credential chain
// (Bedrock), so the session receives short-lived credentials limited to that
// role instead of a copy of the controller host's own credentials.
type AwsRoleConfig struct {
	// RoleARN is the role to assume, arn:aws:iam::<account>:role/<name>. The
	// host's AWS login must be allowed to assume it.
	RoleARN string `json:"role_arn" toml:"role_arn"`
	// SessionPolicy is an inline session policy (JSON text) that narrows the
	// role further. With neither this nor SessionPolicyARNs, a built-in
	// Bedrock invocation policy is used.
	SessionPolicy *string `json:"session_policy,omitempty" toml:"session_policy,omitempty"`
	// SessionPolicyARNs is managed policies (ARNs) that narrow the role
	// further.
	SessionPolicyARNs []string `json:"session_policy_arns,omitempty" toml:"session_policy_arns,omitempty"`
	// DurationSeconds is how long each set of credentials lasts. Unset leaves
	// the choice to AWS, whose default is 3600 and is valid for every role.
	DurationSeconds *uint32 `json:"duration_seconds,omitempty" toml:"duration_seconds,omitempty"`
	// SourceIdentity also sets the session id as the AWS SourceIdentity,
	// which CloudTrail records and which survives role chaining. The role's
	// trust policy must then allow sts:SetSourceIdentity.
	SourceIdentity bool `json:"source_identity,omitempty" toml:"source_identity,omitempty"`
}

// UnmarshalJSON requires `role_arn`, as serde does.
func (a *AwsRoleConfig) UnmarshalJSON(data []byte) error {
	fields, err := decodeObject(data)
	if err != nil {
		return err
	}
	if _, ok := fields["role_arn"]; !ok {
		return missingField("role_arn")
	}
	type plain AwsRoleConfig
	var value plain
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*a = AwsRoleConfig(value)
	return nil
}

// Validate makes shape checks only. Policy sizes, the duration range and
// whether the role exists are limits AWS owns and reports when it refuses a
// request.
func (a AwsRoleConfig) Validate(id string) error {
	if !validRoleARN(a.RoleARN) {
		return fmt.Errorf("profile %s: [profiles.%s.aws] role_arn %s is not a role ARN such as arn:aws:iam::123456789012:role/name",
			debugString(id), id, debugString(a.RoleARN))
	}
	if a.SessionPolicy != nil {
		var document any
		if err := json.Unmarshal([]byte(*a.SessionPolicy), &document); err != nil {
			return fmt.Errorf("profile %s: [profiles.%s.aws] session_policy is not a JSON policy document", debugString(id), id)
		}
		if _, ok := document.(map[string]any); !ok {
			return fmt.Errorf("profile %s: [profiles.%s.aws] session_policy is not a JSON policy document", debugString(id), id)
		}
	}
	for _, arn := range a.SessionPolicyARNs {
		if !strings.HasPrefix(arn, "arn:") {
			return fmt.Errorf("profile %s: [profiles.%s.aws] session_policy_arns entry %s is not an ARN",
				debugString(id), id, debugString(arn))
		}
	}
	return nil
}

// SessionPolicyFingerprint is a fingerprint of what narrows the role: the
// effective inline policy and the managed policy ARNs, in a fixed order.
func (a AwsRoleConfig) SessionPolicyFingerprint() string {
	arns := slices.Clone(a.SessionPolicyARNs)
	slices.Sort(arns)
	policy, _ := a.EffectiveSessionPolicy()
	return CredentialFingerprint([]byte(policy + "\n" + strings.Join(arns, "\n")))
}

// EffectiveSessionPolicy is the inline session policy to send, or false when
// only managed policies narrow the role.
func (a AwsRoleConfig) EffectiveSessionPolicy() (string, bool) {
	switch {
	case a.SessionPolicy != nil:
		return *a.SessionPolicy, true
	case len(a.SessionPolicyARNs) == 0:
		return DefaultBedrockSessionPolicy, true
	}
	return "", false
}

// validRoleARN accepts arn:<partition>:iam::<12 digits>:role/<path and name>.
func validRoleARN(arn string) bool {
	parts := strings.SplitN(arn, ":", 6)
	if len(parts) != 6 || parts[0] != "arn" || parts[2] != "iam" || parts[3] != "" {
		return false
	}
	partition, account, resource := parts[1], parts[4], parts[5]
	if partition == "" || len(account) != 12 {
		return false
	}
	for i := 0; i < len(account); i++ {
		if account[i] < '0' || account[i] > '9' {
			return false
		}
	}
	name, ok := strings.CutPrefix(resource, "role/")
	return ok && name != "" && !strings.ContainsFunc(name, unicode.IsSpace)
}
