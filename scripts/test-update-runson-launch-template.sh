#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
helper="$repository_root/scripts/update-runson-launch-template.sh"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT

fake_bin="$test_root/bin"
config_root="$test_root/config"
aws_log="$test_root/aws.log"
aws_state="$test_root/aws-created"
launch_data="$test_root/launch-template-data.json"
mkdir -p "$fake_bin" "$config_root/mjolnir"
printf '%s\n' 'ssh-ed25519 test-key mjolnir-runson-test' >"$test_root/id.pub"
printf '%s\n' 'test-private-key' >"$test_root/id"
printf '%s\n' 'version = 1' >"$config_root/mjolnir/config.toml"

cat >"$fake_bin/aws" <<'FAKE_AWS'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$MJ_RUNSON_TEST_AWS_LOG"
arguments=" $* "

case "$arguments" in
    *" describe-images --owners self "*)
        [[ -f "$MJ_RUNSON_TEST_AWS_STATE" ]] && printf '%s\n' 'ami-deadbeef' || printf '%s\n' 'None'
        ;;
    *" describe-images --image-ids "*" Images[0].State "*) printf '%s\n' 'available' ;;
    *" describe-images --image-ids "*" Images[0].RootDeviceName "*) printf '%s\n' '/dev/sda1' ;;
    *" copy-image "*) printf '%s\n' 'ami-deadbeef' ;;
    *" create-tags "*|*" wait image-available "*) ;;
    *" describe-vpcs "*) printf '%s\n' 'vpc-1234abcd' ;;
    *" describe-security-groups --filters "*)
        [[ -f "$MJ_RUNSON_TEST_AWS_STATE" ]] && printf '%s\n' 'sg-1234abcd' || printf '%s\n' 'None'
        ;;
    *" create-security-group "*) printf '%s\n' 'sg-1234abcd' ;;
    *" authorize-security-group-ingress "*) ;;
    *" describe-launch-template-versions "*) command cat "$MJ_RUNSON_TEST_LAUNCH_DATA" ;;
    *" describe-launch-templates --launch-template-ids "*) printf '%s\n' '1' ;;
    *" describe-launch-templates --filters "*)
        [[ -f "$MJ_RUNSON_TEST_AWS_STATE" ]] && printf '%s\n' 'lt-1234abcd' || printf '%s\n' 'None'
        ;;
    *" create-launch-template "*)
        while (($#)); do
            if [[ "$1" == --launch-template-data ]]; then
                cp "${2#file://}" "$MJ_RUNSON_TEST_LAUNCH_DATA"
                break
            fi
            shift
        done
        touch "$MJ_RUNSON_TEST_AWS_STATE"
        printf '%s\n' '{"LaunchTemplate":{"LaunchTemplateId":"lt-1234abcd","LatestVersionNumber":1}}'
        ;;
    *)
        printf 'unexpected fake AWS invocation: %s\n' "$*" >&2
        exit 1
        ;;
esac
FAKE_AWS

cat >"$fake_bin/curl" <<'FAKE_CURL'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' '203.0.113.10'
FAKE_CURL
chmod +x "$fake_bin/aws" "$fake_bin/curl"

assert_contains() {
    local expected="$1"
    local filename="$2"
    grep -F -- "$expected" "$filename" >/dev/null || {
        printf 'expected %s to contain %q\n' "$filename" "$expected" >&2
        exit 1
    }
}

assert_lacks_legacy_identity() {
    local filename="$1"
    if grep -E -- '(^|[^[:alnum:]_])(Hel|hel)([^[:alnum:]_]|$)' "$filename" >/dev/null; then
        printf 'legacy Hel identity found in %s:\n' "$filename" >&2
        grep -En -- '(^|[^[:alnum:]_])(Hel|hel)([^[:alnum:]_]|$)' "$filename" >&2
        exit 1
    fi
}

run_helper() {
    env \
        PATH="$fake_bin:$PATH" \
        XDG_CONFIG_HOME="$test_root/unused-xdg-config" \
        MJ_CONFIG_DIR="$config_root/mjolnir" \
        MJ_RUNSON_TEST_AWS_LOG="$aws_log" \
        MJ_RUNSON_TEST_AWS_STATE="$aws_state" \
        MJ_RUNSON_TEST_LAUNCH_DATA="$launch_data" \
        bash "$helper" \
        --source-ami ami-1234abcd \
        --ssh-public-key "$test_root/id.pub" \
        --ssh-identity-file "$test_root/id" \
        --write-mj-config
}

bash "$helper" --help >"$test_root/help.out"
assert_contains 'Public key installed for Mjolnir SSH' "$test_root/help.out"
assert_lacks_legacy_identity "$test_root/help.out"

if bash "$helper" --write-hel-config >"$test_root/legacy-alias.out" 2>&1; then
    printf '%s\n' 'legacy --write-hel-config alias unexpectedly succeeded' >&2
    exit 1
fi
assert_contains 'unknown argument: --write-hel-config' "$test_root/legacy-alias.out"

run_helper >"$test_root/first-run.out"
run_helper >"$test_root/second-run.out"

config_file="$config_root/mjolnir/config.toml"
assert_contains 'Added Mjolnir target targets.aws-runson' "$test_root/first-run.out"
assert_contains 'Mjolnir target targets.aws-runson already exists' "$test_root/second-run.out"
assert_contains '[targets.aws-runson]' "$config_file"
assert_contains 'launch_template = "mj-runson"' "$config_file"
[[ "$(grep -Fc '[targets.aws-runson]' "$config_file")" == 1 ]]

assert_contains 'Name=tag:MjolnirSourceAmi,Values=ami-1234abcd' "$aws_log"
assert_contains '--name mjolnir-runson-' "$aws_log"
assert_contains 'Account-owned copy of RunsOn ami-1234abcd for Mjolnir' "$aws_log"
assert_contains 'Key=Project,Value=mjolnir' "$aws_log"
assert_contains 'Key=Purpose,Value=mjolnir-runson' "$aws_log"
assert_contains 'Key=MjolnirSourceOwner,Value=135269210855' "$aws_log"
assert_contains 'SSH access for Mjolnir RunsOn sessions' "$aws_log"
assert_contains 'Description=Mjolnir-controller-ssh' "$aws_log"
assert_contains 'ResourceType=launch-template' "$aws_log"
assert_contains '--version-description Mjolnir RunsOn ami-1234abcd copied as ami-deadbeef' "$aws_log"

jq -r '.UserData' "$launch_data" | base64 --decode >"$test_root/user-data.yaml"
assert_contains 'gecos: Mjolnir session user' "$test_root/user-data.yaml"

[[ "$(grep -c ' copy-image ' "$aws_log")" == 1 ]]
[[ "$(grep -c ' create-security-group ' "$aws_log")" == 1 ]]
[[ "$(grep -c ' create-launch-template ' "$aws_log")" == 1 ]]
assert_contains 'Launch template mj-runson default version 1 already matches.' "$test_root/second-run.out"

for output in "$aws_log" "$launch_data" "$test_root/user-data.yaml" "$config_file" \
    "$test_root/first-run.out" "$test_root/second-run.out"; do
    assert_lacks_legacy_identity "$output"
done
