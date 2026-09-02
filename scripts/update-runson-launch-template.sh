#!/usr/bin/env bash
# Refresh the account-owned RunsOn AMI used by Mjolnir and publish it as the
# default version of a stable EC2 launch template. RunsOn deregisters older
# AMIs, so Mjolnir always launches an account-owned copy.
set -euo pipefail

REGION="${AWS_REGION:-us-east-1}"
TEMPLATE_NAME="mj-runson"
SOURCE_OWNER="135269210855"
SOURCE_NAME_PATTERN="runs-on-v2.2-ubuntu26-full-x64-*"
INSTANCE_TYPE="m8i-flex.large"
ROOT_VOLUME_GIB=60
SSH_PUBLIC_KEY="${HOME}/.ssh/vastai.pub"
SSH_IDENTITY_FILE="${HOME}/.ssh/vastai"
SECURITY_GROUP_NAME="mj-runson-ssh"
WRITE_MJ_CONFIG=false
SOURCE_AMI=""

usage() {
    printf '%s\n' \
        'Usage: scripts/update-runson-launch-template.sh [options]' \
        '' \
        'Copies the newest RunsOn Ubuntu 26 AMI into this AWS account, then creates a' \
        'new default version of the mj-runson launch template.' \
        '' \
        'Options:' \
        '  --region REGION            AWS region (default: us-east-1)' \
        '  --template-name NAME       Launch template name (default: mj-runson)' \
        '  --source-ami AMI           Use this source AMI instead of resolving newest' \
        '  --instance-type TYPE       EC2 instance type (default: m8i-flex.large)' \
        '  --root-volume-gib GIB      Root volume size (default: 60)' \
        '  --ssh-public-key PATH      Public key installed for Mjolnir SSH' \
        '  --ssh-identity-file PATH   Matching private key recorded in Mjolnir config' \
        '  --write-mj-config          Append targets.aws-runson if not configured' \
        '  -h, --help                 Show this help'
}

while (($#)); do
    case "$1" in
        --region) REGION="$2"; shift 2 ;;
        --template-name) TEMPLATE_NAME="$2"; shift 2 ;;
        --source-ami) SOURCE_AMI="$2"; shift 2 ;;
        --instance-type) INSTANCE_TYPE="$2"; shift 2 ;;
        --root-volume-gib) ROOT_VOLUME_GIB="$2"; shift 2 ;;
        --ssh-public-key) SSH_PUBLIC_KEY="$2"; shift 2 ;;
        --ssh-identity-file) SSH_IDENTITY_FILE="$2"; shift 2 ;;
        --write-mj-config) WRITE_MJ_CONFIG=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

for command in aws base64 curl jq; do
    command -v "$command" >/dev/null || {
        printf 'required command not found: %s\n' "$command" >&2
        exit 1
    }
done
if [[ ! -r "$SSH_PUBLIC_KEY" || ! -r "$SSH_IDENTITY_FILE" ]]; then
    printf 'SSH public key or identity file is not readable\n' >&2
    exit 1
fi
if ! [[ "$ROOT_VOLUME_GIB" =~ ^[0-9]+$ ]] || ((ROOT_VOLUME_GIB < 30)); then
    printf '%s\n' '--root-volume-gib must be an integer of at least 30' >&2
    exit 2
fi

aws_ec2() { aws --region "$REGION" ec2 "$@"; }

if [[ -z "$SOURCE_AMI" ]]; then
    SOURCE_AMI="$(aws_ec2 describe-images --owners "$SOURCE_OWNER" \
        --filters "Name=name,Values=${SOURCE_NAME_PATTERN}" 'Name=state,Values=available' \
        --query 'reverse(sort_by(Images,&CreationDate))[0].ImageId' --output text)"
fi
if [[ ! "$SOURCE_AMI" =~ ^ami-[0-9a-f]+$ ]]; then
    printf 'could not resolve a RunsOn source AMI: %s\n' "$SOURCE_AMI" >&2
    exit 1
fi
source_state="$(aws_ec2 describe-images --image-ids "$SOURCE_AMI" --query 'Images[0].State' --output text)"
root_device="$(aws_ec2 describe-images --image-ids "$SOURCE_AMI" --query 'Images[0].RootDeviceName' --output text)"
if [[ "$source_state" != available || -z "$root_device" || "$root_device" == None ]]; then
    printf 'source AMI is not launchable: %s\n' "$SOURCE_AMI" >&2
    exit 1
fi

# Reuse the owned copy on retries, while every newly released upstream AMI gets
# one immutable account-owned copy. This is the durable part of the P2T pattern.
owned_ami="$(aws_ec2 describe-images --owners self \
    --filters "Name=tag:MjolnirSourceAmi,Values=${SOURCE_AMI}" \
    --query 'reverse(sort_by(Images,&CreationDate))[0].ImageId' --output text)"
if [[ -z "$owned_ami" || "$owned_ami" == None ]]; then
    image_name="mjolnir-runson-$(date -u +%Y%m%d%H%M%S)"
    printf 'Copying RunsOn %s into this account as %s...\n' "$SOURCE_AMI" "$image_name"
    owned_ami="$(aws_ec2 copy-image --source-region "$REGION" --source-image-id "$SOURCE_AMI" \
        --name "$image_name" --description "Account-owned copy of RunsOn ${SOURCE_AMI} for Mjolnir" \
        --query ImageId --output text)"
    aws_ec2 create-tags --resources "$owned_ami" --tags \
        "Key=Name,Value=${image_name}" 'Key=Project,Value=mjolnir' 'Key=Purpose,Value=mjolnir-runson' \
        'Key=ManagedBy,Value=scripts/update-runson-launch-template.sh' \
        "Key=MjolnirSourceAmi,Value=${SOURCE_AMI}" "Key=MjolnirSourceOwner,Value=${SOURCE_OWNER}"
else
    printf 'Reusing account-owned copy %s of RunsOn %s.\n' "$owned_ami" "$SOURCE_AMI"
fi
printf 'Waiting for copied AMI %s...\n' "$owned_ami"
aws_ec2 wait image-available --image-ids "$owned_ami"

vpc_id="$(aws_ec2 describe-vpcs --filters 'Name=is-default,Values=true' --query 'Vpcs[0].VpcId' --output text)"
if [[ -z "$vpc_id" || "$vpc_id" == None ]]; then
    printf 'no default VPC found in %s\n' "$REGION" >&2
    exit 1
fi
security_group_id="$(aws_ec2 describe-security-groups \
    --filters "Name=vpc-id,Values=${vpc_id}" "Name=group-name,Values=${SECURITY_GROUP_NAME}" \
    --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true)"
if [[ -z "$security_group_id" || "$security_group_id" == None ]]; then
    security_group_id="$(aws_ec2 create-security-group --group-name "$SECURITY_GROUP_NAME" \
        --description 'SSH access for Mjolnir RunsOn sessions' --vpc-id "$vpc_id" \
        --tag-specifications "ResourceType=security-group,Tags=[{Key=Name,Value=${SECURITY_GROUP_NAME}},{Key=Project,Value=mjolnir},{Key=Purpose,Value=mjolnir-runson},{Key=ManagedBy,Value=scripts/update-runson-launch-template.sh}]" \
        --query GroupId --output text)"
fi
controller_ip="$(curl --fail --silent --show-error https://checkip.amazonaws.com | tr -d '[:space:]')"
if [[ ! "$controller_ip" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]]; then
    printf 'could not determine controller public IPv4 address: %s\n' "$controller_ip" >&2
    exit 1
fi
if ! aws_ec2 authorize-security-group-ingress --group-id "$security_group_id" \
    --ip-permissions "IpProtocol=tcp,FromPort=22,ToPort=22,IpRanges=[{CidrIp=${controller_ip}/32,Description=Mjolnir-controller-ssh}]" \
    >/dev/null 2>&1; then
    existing_cidrs="$(aws_ec2 describe-security-groups --group-ids "$security_group_id" \
        --query "SecurityGroups[0].IpPermissions[?FromPort==\`22\`].IpRanges[].CidrIp" --output text)"
    if [[ " $existing_cidrs " != *" ${controller_ip}/32 "* ]]; then
        printf 'failed to grant SSH access from %s/32\n' "$controller_ip" >&2
        exit 1
    fi
fi

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT
user_data_file="$work_dir/user-data.yaml"
launch_data_file="$work_dir/launch-template-data.json"
ssh_key="$(<"$SSH_PUBLIC_KEY")"
printf '%s\n' \
    '#cloud-config' \
    'ssh_pwauth: false' \
    'users:' \
    '  - default' \
    '  - name: ubuntu' \
    '    gecos: Mjolnir session user' \
    '    groups: [adm, sudo]' \
    '    sudo: ALL=(ALL) NOPASSWD:ALL' \
    '    shell: /bin/bash' \
    '    ssh_authorized_keys:' \
    "      - ${ssh_key}" \
    'runcmd:' \
    "  - [bash, -lc, 'if ! dpkg -s openssh-server >/dev/null 2>&1; then apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y openssh-server; fi']" \
    "  - [bash, -lc, 'systemctl unmask ssh || true; systemctl enable --now ssh || systemctl enable --now sshd || true']" \
    >"$user_data_file"
user_data="$(base64 --wrap=0 "$user_data_file")"
jq -n --arg image_id "$owned_ami" --arg instance_type "$INSTANCE_TYPE" \
    --arg security_group_id "$security_group_id" --arg user_data "$user_data" \
    --arg root_device "$root_device" --argjson root_volume_gib "$ROOT_VOLUME_GIB" \
    '{ImageId:$image_id, InstanceType:$instance_type, SecurityGroupIds:[$security_group_id],
      UserData:$user_data, MetadataOptions:{HttpTokens:"required",HttpEndpoint:"enabled",
      HttpPutResponseHopLimit:1,InstanceMetadataTags:"disabled"},
      BlockDeviceMappings:[{DeviceName:$root_device,Ebs:{DeleteOnTermination:true,Encrypted:false,
      VolumeSize:$root_volume_gib,VolumeType:"gp3",Iops:3000,Throughput:125}}]}' >"$launch_data_file"

template_id="$(aws_ec2 describe-launch-templates --filters "Name=launch-template-name,Values=${TEMPLATE_NAME}" \
    --query 'LaunchTemplates[0].LaunchTemplateId' --output text 2>/dev/null || true)"
version_description="Mjolnir RunsOn ${SOURCE_AMI} copied as ${owned_ami} on $(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [[ -z "$template_id" || "$template_id" == None ]]; then
    create_result="$(aws_ec2 create-launch-template --launch-template-name "$TEMPLATE_NAME" \
        --version-description "$version_description" --launch-template-data "file://${launch_data_file}" \
        --tag-specifications "ResourceType=launch-template,Tags=[{Key=Name,Value=${TEMPLATE_NAME}},{Key=Project,Value=mjolnir},{Key=Purpose,Value=mjolnir-runson},{Key=ManagedBy,Value=scripts/update-runson-launch-template.sh}]" --output json)"
    template_id="$(jq -r '.LaunchTemplate.LaunchTemplateId' <<<"$create_result")"
    template_version="$(jq -r '.LaunchTemplate.LatestVersionNumber' <<<"$create_result")"
else
    current_version="$(aws_ec2 describe-launch-templates --launch-template-ids "$template_id" \
        --query 'LaunchTemplates[0].DefaultVersionNumber' --output text)"
    current_data="$(aws_ec2 describe-launch-template-versions --launch-template-id "$template_id" \
        --versions "$current_version" --query 'LaunchTemplateVersions[0].LaunchTemplateData' --output json)"
    if [[ "$(jq --sort-keys --compact-output . <<<"$current_data")" == "$(jq --sort-keys --compact-output . "$launch_data_file")" ]]; then
        template_version="$current_version"
        printf 'Launch template %s default version %s already matches.\n' "$TEMPLATE_NAME" "$template_version"
    else
        create_result="$(aws_ec2 create-launch-template-version --launch-template-id "$template_id" \
            --version-description "$version_description" --launch-template-data "file://${launch_data_file}" --output json)"
        template_version="$(jq -r '.LaunchTemplateVersion.VersionNumber' <<<"$create_result")"
        aws_ec2 modify-launch-template --launch-template-id "$template_id" --default-version "$template_version" >/dev/null
    fi
fi

if [[ "$WRITE_MJ_CONFIG" == true ]]; then
    mj_config_dir="${MJ_CONFIG_DIR:-${XDG_CONFIG_HOME:-${HOME}/.config}/mjolnir}"
    mj_config="$mj_config_dir/config.toml"
    if [[ ! -f "$mj_config" ]]; then
        printf 'Mjolnir config does not exist: %s\n' "$mj_config" >&2
        exit 1
    fi
    if grep -Fqx '[targets.aws-runson]' "$mj_config"; then
        printf '%s\n' 'Mjolnir target targets.aws-runson already exists; leaving it unchanged.'
    else
        printf '\n[targets.aws-runson]\nkind = "aws-ec2"\nregion = "%s"\nlaunch_template = "%s"\nssh_user = "ubuntu"\naddress_source = "public-ip"\nidentity_file = "%s"\n' \
            "$REGION" "$TEMPLATE_NAME" "$SSH_IDENTITY_FILE" >>"$mj_config"
        printf 'Added Mjolnir target targets.aws-runson to %s.\n' "$mj_config"
    fi
fi

printf 'launch_template=%s\nlaunch_template_id=%s\ndefault_version=%s\nsource_ami=%s\nowned_ami=%s\nsecurity_group=%s\ncontroller_ssh_cidr=%s/32\n' \
    "$TEMPLATE_NAME" "$template_id" "$template_version" "$SOURCE_AMI" "$owned_ami" "$security_group_id" "$controller_ip"
