#!/usr/bin/env python3
"""Provision and clean up the disposable EC2 host used by ssh-docker tests.

This is intentionally an opt-in operator tool.  It uses the AWS CLI and the
system OpenSSH utilities instead of a cloud SDK, and it never installs Docker
on the controller.  Every resource is tagged and recorded before the next
resource is created so an interrupted run can be cleaned up by exact ID.
"""

from __future__ import annotations

import argparse
import datetime as dt
import ipaddress
import json
import os
import pathlib
import shlex
import subprocess
import sys
import tempfile
import time
import urllib.request
import uuid
from typing import Any, Callable


DEFAULT_PROFILE = "default"
DEFAULT_REGION = "us-east-1"
DEFAULT_INSTANCE_TYPE = "t3.large"
DEFAULT_AZ = "us-east-1a"
DEFAULT_VOLUME_SIZE = 60
DEFAULT_USER = "ubuntu"
DEFAULT_AMI_PARAMETER = (
    "/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/"
    "hvm/ebs-gp3/ami-id"
)
DEFAULT_ARTIFACT_ROOT = pathlib.Path("target/ssh-docker-e2e")
LEDGER_NAME = "ledger.json"
KEY_NAME = "id_ed25519"
PUBLIC_KEY_NAME = "id_ed25519.pub"
KNOWN_HOSTS_NAME = "known_hosts"
USERDATA_NAME = "user-data.sh"
MAX_ERROR_OUTPUT = 4_000


class LabError(RuntimeError):
    """An expected lab failure with an operator-facing message."""


class CommandError(LabError):
    def __init__(self, argv: list[str], returncode: int, stderr: str) -> None:
        command = shlex.join(argv)
        detail = stderr.strip()[-MAX_ERROR_OUTPUT:] or "no stderr output"
        super().__init__(f"command failed ({returncode}): {command}: {detail}")
        self.argv = argv
        self.returncode = returncode
        self.stderr = stderr


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).replace(microsecond=0).isoformat()


def validate_ipv4(value: str) -> str:
    try:
        address = ipaddress.ip_address(value)
    except ValueError as error:
        raise LabError(f"controller address is not an IPv4 address: {value!r}") from error
    if address.version != 4:
        raise LabError(f"controller address is not IPv4: {value!r}")
    return str(address)


def run_command(
    argv: list[str],
    *,
    timeout: float,
    input_text: str | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            argv,
            input=input_text,
            text=True,
            capture_output=True,
            timeout=timeout,
            check=False,
        )
    except FileNotFoundError as error:
        raise LabError(f"required executable is unavailable: {argv[0]}") from error
    except subprocess.TimeoutExpired as error:
        raise LabError(f"command timed out after {timeout:.0f}s: {shlex.join(argv)}") from error
    if check and result.returncode != 0:
        raise CommandError(argv, result.returncode, result.stderr)
    return result


def atomic_write(path: pathlib.Path, content: str, *, mode: int = 0o600) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.parent.chmod(0o700)
    temporary: pathlib.Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            encoding="utf-8",
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
        ) as stream:
            temporary = pathlib.Path(stream.name)
            os.chmod(temporary, mode)
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        os.chmod(path, mode)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def ensure_private_directory(path: pathlib.Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    path.chmod(0o700)


def dump_json(path: pathlib.Path, value: dict[str, Any]) -> None:
    atomic_write(path, json.dumps(value, indent=2, sort_keys=True) + "\n")


def load_json(path: pathlib.Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise LabError(f"ledger does not exist: {path}") from error
    except json.JSONDecodeError as error:
        raise LabError(f"ledger is not valid JSON: {path}: {error}") from error
    if not isinstance(value, dict) or value.get("schema") != 1:
        raise LabError(f"unsupported lab ledger: {path}")
    return value


class Aws:
    def __init__(self, profile: str, region: str, timeout: float) -> None:
        self.profile = profile
        self.region = region
        self.timeout = timeout

    def command(self, service: str, *arguments: str) -> list[str]:
        return [
            "aws",
            "--profile",
            self.profile,
            "--region",
            self.region,
            service,
            *arguments,
        ]

    def json(self, service: str, *arguments: str) -> dict[str, Any]:
        result = run_command(
            self.command(service, *arguments, "--output", "json"),
            timeout=self.timeout,
        )
        # Successful EC2 mutations such as create-tags and delete-security-group
        # have no response body, even with --output json.
        if not result.stdout.strip():
            return {}
        try:
            value = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise LabError(f"AWS returned invalid JSON for {service}: {error}") from error
        if not isinstance(value, dict):
            raise LabError(f"AWS returned a non-object response for {service}")
        return value

    def text(self, service: str, *arguments: str) -> str:
        result = run_command(
            self.command(service, *arguments, "--output", "text"),
            timeout=self.timeout,
        )
        return result.stdout.strip()

    def optional_json(self, service: str, *arguments: str) -> dict[str, Any] | None:
        try:
            return self.json(service, *arguments)
        except CommandError as error:
            if any(
                marker in error.stderr
                for marker in (
                    "InvalidInstanceID.NotFound",
                    "InvalidGroup.NotFound",
                    "InvalidKeyPair.NotFound",
                    "InvalidVolume.NotFound",
                    "ResourceNotFoundException",
                )
            ):
                return None
            raise


def tags(run_tag: str, name: str) -> str:
    return json.dumps(
        [
            {"Key": "Name", "Value": name},
            {"Key": "mj-ssh-docker-run", "Value": run_tag},
        ],
        separators=(",", ":"),
    )


def instance_tag_specification(run_tag: str) -> str:
    return json.dumps(
        [
            {
                "ResourceType": "instance",
                "Tags": [
                    {"Key": "Name", "Value": run_tag},
                    {"Key": "mj-ssh-docker-run", "Value": run_tag},
                ],
            },
            {
                "ResourceType": "volume",
                "Tags": [
                    {"Key": "Name", "Value": run_tag},
                    {"Key": "mj-ssh-docker-run", "Value": run_tag},
                ],
            },
        ],
        separators=(",", ":"),
    )


def resource_tag_specification(run_tag: str, resource_type: str = "security-group") -> str:
    return json.dumps(
        [
            {
                "ResourceType": resource_type,
                "Tags": [
                    {"Key": "Name", "Value": run_tag},
                    {"Key": "mj-ssh-docker-run", "Value": run_tag},
                ],
            }
        ],
        separators=(",", ":"),
    )


def tag_map(resource: dict[str, Any]) -> dict[str, str]:
    return {
        item.get("Key", ""): item.get("Value", "")
        for item in resource.get("Tags", [])
        if isinstance(item, dict) and isinstance(item.get("Key"), str)
    }


def public_controller_ipv4(timeout: float) -> str:
    request = urllib.request.Request(
        "https://checkip.amazonaws.com/",
        headers={"User-Agent": "mjolnir-ssh-docker-lab/1"},
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            value = response.read(128).decode("ascii", "strict").strip()
    except (OSError, UnicodeError) as error:
        raise LabError(f"could not determine controller public IPv4: {error}") from error
    return validate_ipv4(value)


def user_data() -> str:
    # The shutdown timer is installed before any package operation.  A failed
    # bootstrap therefore still expires instead of leaving an EC2 host running.
    return """#!/bin/bash
set -euo pipefail
exec > >(tee -a /var/log/mj-ssh-docker-user-data.log | logger -t mj-ssh-docker-user-data) 2>&1

mkdir -p /var/lib/mjolnir
touch /var/lib/mjolnir/ssh-docker-bootstrap-started
systemd-run --unit=mj-ssh-docker-expiry --description='Mjolnir disposable lab expiry' \\
  --on-active=4h --property=RemainAfterExit=no /usr/bin/systemctl poweroff

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install --yes ca-certificates curl gnupg
install -m 0755 -d /etc/apt/keyrings
curl --fail --silent --show-error --location \\
  https://download.docker.com/linux/ubuntu/gpg \\
  --output /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \\
  https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \\
  > /etc/apt/sources.list.d/docker.list
apt-get update
apt-get install --yes docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
usermod --append --groups docker ubuntu
systemctl enable --now docker
docker version --format '{{.Server.Version}}' > /var/lib/mjolnir/docker-server-version
docker info --format '{{json .Driver}}' > /var/lib/mjolnir/docker-driver
touch /var/lib/mjolnir/ssh-docker-ready
"""


def resolve_ami(aws: Aws, parameter: str) -> str:
    value = aws.text(
        "ssm",
        "get-parameter",
        "--name",
        parameter,
        "--query",
        "Parameter.Value",
    )
    if not value:
        raise LabError(f"SSM parameter did not contain an AMI ID: {parameter}")
    return value


def resolve_default_vpc_and_subnet(aws: Aws, availability_zone: str) -> tuple[str, str]:
    vpcs = aws.json(
        "ec2",
        "describe-vpcs",
        "--filters",
        "Name=is-default,Values=true",
    ).get("Vpcs", [])
    if len(vpcs) != 1 or not isinstance(vpcs[0].get("VpcId"), str):
        raise LabError(f"expected one default VPC in {aws.region}, found {len(vpcs)}")
    vpc_id = vpcs[0]["VpcId"]
    subnets = aws.json(
        "ec2",
        "describe-subnets",
        "--filters",
        f"Name=vpc-id,Values={vpc_id}",
        f"Name=availability-zone,Values={availability_zone}",
        "Name=map-public-ip-on-launch,Values=true",
    ).get("Subnets", [])
    if not subnets:
        raise LabError(f"no public default-VPC subnet exists in {availability_zone}")
    subnet_id = subnets[0].get("SubnetId")
    if not isinstance(subnet_id, str) or not subnet_id:
        raise LabError("AWS returned a public subnet without a subnet ID")
    return vpc_id, subnet_id


def generate_keypair(artifact_dir: pathlib.Path, comment: str, timeout: float) -> tuple[pathlib.Path, pathlib.Path]:
    private = artifact_dir / KEY_NAME
    public = artifact_dir / PUBLIC_KEY_NAME
    if private.exists() or public.exists():
        raise LabError(f"refusing to overwrite existing SSH key in {artifact_dir}")
    run_command(
        [
            "ssh-keygen",
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            comment,
            "-f",
            str(private),
        ],
        timeout=timeout,
    )
    os.chmod(private, 0o600)
    os.chmod(public, 0o600)
    return private, public


def ledger_path(artifact_dir: pathlib.Path) -> pathlib.Path:
    return artifact_dir / LEDGER_NAME


def save_ledger(path: pathlib.Path, ledger: dict[str, Any]) -> None:
    ledger["updated_at"] = utc_now()
    dump_json(path, ledger)


def mark(ledger_path_value: pathlib.Path, ledger: dict[str, Any], **values: Any) -> None:
    ledger.update(values)
    save_ledger(ledger_path_value, ledger)


def create_ledger(args: argparse.Namespace, artifact_dir: pathlib.Path) -> dict[str, Any]:
    run_id = f"{dt.datetime.now(dt.timezone.utc):%Y%m%dT%H%M%SZ}-{uuid.uuid4().hex[:10]}"
    run_tag = f"mj-ssh-docker-{run_id}"
    ledger = {
        "schema": 1,
        "run_id": run_id,
        "run_tag": run_tag,
        "created_at": utc_now(),
        "profile": args.profile,
        "region": args.region,
        "availability_zone": args.availability_zone,
        "instance_type": args.instance_type,
        "volume_size_gib": args.volume_size_gib,
        "ami_parameter": args.ami_parameter,
        "resources": {
            "instance_id": None,
            "volume_id": None,
            "security_group_id": None,
            "key_name": f"{run_tag}-key",
            "instance_tag": run_tag,
        },
        "connection": {
            "user": DEFAULT_USER,
            "private_key": str(artifact_dir / KEY_NAME),
            "known_hosts": str(artifact_dir / KNOWN_HOSTS_NAME),
            "address": None,
        },
        "state": "creating",
        "cleanup_errors": [],
    }
    save_ledger(ledger_path(artifact_dir), ledger)
    return ledger


def resource_is_owned(resource: dict[str, Any], run_tag: str) -> bool:
    return tag_map(resource).get("mj-ssh-docker-run") == run_tag


def root_volume_id(instance: dict[str, Any]) -> str | None:
    for mapping in instance.get("BlockDeviceMappings", []):
        if not isinstance(mapping, dict):
            continue
        ebs = mapping.get("Ebs")
        if isinstance(ebs, dict) and isinstance(ebs.get("VolumeId"), str):
            if mapping.get("DeviceName") in {None, "/dev/sda1", "/dev/xvda"}:
                return ebs["VolumeId"]
    return None


def discover_owned_instance(aws: Aws, run_tag: str) -> dict[str, Any] | None:
    response = aws.optional_json(
        "ec2",
        "describe-instances",
        "--filters",
        f"Name=tag:mj-ssh-docker-run,Values={run_tag}",
    )
    instances = [
        instance
        for reservation in (response or {}).get("Reservations", [])
        for instance in reservation.get("Instances", [])
    ]
    if len(instances) > 1:
        raise LabError(f"run tag {run_tag} matched multiple instances")
    return instances[0] if instances else None


def discover_owned_volume(aws: Aws, run_tag: str) -> str | None:
    response = aws.optional_json(
        "ec2",
        "describe-volumes",
        "--filters",
        f"Name=tag:mj-ssh-docker-run,Values={run_tag}",
    )
    volumes = (response or {}).get("Volumes", [])
    if len(volumes) > 1:
        raise LabError(f"run tag {run_tag} matched multiple volumes")
    volume_id = volumes[0].get("VolumeId") if volumes else None
    return volume_id if isinstance(volume_id, str) else None


def discover_owned_security_group(aws: Aws, run_tag: str) -> str | None:
    response = aws.optional_json(
        "ec2",
        "describe-security-groups",
        "--filters",
        f"Name=tag:mj-ssh-docker-run,Values={run_tag}",
    )
    groups = (response or {}).get("SecurityGroups", [])
    if len(groups) > 1:
        raise LabError(f"run tag {run_tag} matched multiple security groups")
    group_id = groups[0].get("GroupId") if groups else None
    return group_id if isinstance(group_id, str) else None


def recover_owned_instance_ids(
    aws: Aws,
    ledger: dict[str, Any],
    path: pathlib.Path,
) -> None:
    resources = ledger["resources"]
    instance: dict[str, Any] | None = None
    instance_id = resources.get("instance_id")
    if isinstance(instance_id, str):
        response = aws.optional_json("ec2", "describe-instances", "--instance-ids", instance_id)
        reservations = (response or {}).get("Reservations", [])
        instances = reservations[0].get("Instances", []) if reservations else []
        instance = instances[0] if instances else None
    if instance is None:
        instance = discover_owned_instance(aws, ledger["run_tag"])
    if instance is not None:
        if not resource_is_owned(instance, ledger["run_tag"]):
            raise LabError(f"discovered instance is not owned by run {ledger['run_tag']}")
        discovered_instance_id = instance.get("InstanceId")
        if isinstance(discovered_instance_id, str):
            resources["instance_id"] = discovered_instance_id
        discovered_volume_id = root_volume_id(instance)
        if discovered_volume_id:
            resources["volume_id"] = discovered_volume_id
        save_ledger(path, ledger)
    if not isinstance(resources.get("volume_id"), str):
        discovered_volume_id = discover_owned_volume(aws, ledger["run_tag"])
        if discovered_volume_id:
            resources["volume_id"] = discovered_volume_id
            save_ledger(path, ledger)


def recover_owned_security_group(
    aws: Aws,
    ledger: dict[str, Any],
    path: pathlib.Path,
) -> None:
    resources = ledger["resources"]
    if isinstance(resources.get("security_group_id"), str):
        return
    group_id = discover_owned_security_group(aws, ledger["run_tag"])
    if group_id:
        resources["security_group_id"] = group_id
        save_ledger(path, ledger)


def create(args: argparse.Namespace) -> int:
    if args.artifact_dir is None:
        run_id_hint = f"{dt.datetime.now(dt.timezone.utc):%Y%m%dT%H%M%SZ}-{uuid.uuid4().hex[:10]}"
        artifact_dir = DEFAULT_ARTIFACT_ROOT / run_id_hint
    else:
        artifact_dir = args.artifact_dir
    ensure_private_directory(artifact_dir)
    path = ledger_path(artifact_dir)
    if path.exists():
        raise LabError(f"refusing to reuse an existing ledger: {path}")
    ledger = create_ledger(args, artifact_dir)
    aws = Aws(args.profile, args.region, args.command_timeout)
    try:
        _, public_key = generate_keypair(
            artifact_dir, ledger["run_tag"], args.command_timeout
        )
        user_data_path = artifact_dir / USERDATA_NAME
        atomic_write(user_data_path, user_data())

        controller_ip = validate_ipv4(args.controller_ip) if args.controller_ip else public_controller_ipv4(8)
        vpc_id, subnet_id = resolve_default_vpc_and_subnet(aws, args.availability_zone)
        ami_id = resolve_ami(aws, args.ami_parameter)
        mark(path, ledger, controller_ip=controller_ip, vpc_id=vpc_id, subnet_id=subnet_id, ami_id=ami_id)

        key_name = ledger["resources"]["key_name"]
        # Record the exact key name before the request: an interrupted import
        # can succeed remotely while the CLI response is lost.
        mark(path, ledger, key_import_requested=True)
        aws.json(
            "ec2",
            "import-key-pair",
            "--key-name",
            key_name,
            "--public-key-material",
            f"fileb://{public_key}",
            "--tag-specifications",
            resource_tag_specification(ledger["run_tag"], "key-pair"),
        )
        mark(path, ledger, key_imported=True)

        try:
            group = aws.json(
                "ec2",
                "create-security-group",
                "--group-name",
                ledger["run_tag"],
                "--description",
                f"Mjolnir disposable ssh-docker lab {ledger['run_id']}",
                "--vpc-id",
                vpc_id,
                "--tag-specifications",
                resource_tag_specification(ledger["run_tag"]),
            )
        except LabError:
            recover_owned_security_group(aws, ledger, path)
            raise
        group_id = group.get("GroupId")
        if not isinstance(group_id, str) or not group_id:
            raise LabError("AWS did not return the security group ID")
        ledger["resources"]["security_group_id"] = group_id
        save_ledger(path, ledger)
        aws.json(
            "ec2",
            "authorize-security-group-ingress",
            "--group-id",
            group_id,
            "--protocol",
            "tcp",
            "--port",
            "22",
            "--cidr",
            f"{controller_ip}/32",
        )
        block_device = json.dumps(
            [
                {
                    "DeviceName": "/dev/sda1",
                    "Ebs": {
                        "VolumeSize": args.volume_size_gib,
                        "VolumeType": "gp3",
                        "Encrypted": True,
                        "DeleteOnTermination": True,
                    },
                },
            ],
            separators=(",", ":"),
        )
        launch_arguments = (
            "--image-id",
            ami_id,
            "--instance-type",
            args.instance_type,
            "--count",
            "1",
            "--key-name",
            key_name,
            "--security-group-ids",
            group_id,
            "--subnet-id",
            subnet_id,
            "--associate-public-ip-address",
            "--instance-initiated-shutdown-behavior",
            "terminate",
            "--credit-specification",
            "CpuCredits=standard",
            "--client-token",
            ledger["run_tag"],
            "--metadata-options",
            "HttpEndpoint=enabled,HttpTokens=required,HttpPutResponseHopLimit=1",
            "--block-device-mappings",
            block_device,
            "--user-data",
            f"fileb://{user_data_path}",
            "--tag-specifications",
            instance_tag_specification(ledger["run_tag"]),
        )
        try:
            instance = aws.json("ec2", "run-instances", *launch_arguments)
        except LabError:
            recover_owned_instance_ids(aws, ledger, path)
            raise
        instances = instance.get("Instances", [])
        if len(instances) != 1:
            raise LabError(f"AWS launched an unexpected number of instances: {len(instances)}")
        instance_id = instances[0].get("InstanceId")
        if not isinstance(instance_id, str) or not instance_id:
            raise LabError("AWS did not return the instance ID")
        volume_id = root_volume_id(instances[0])
        ledger["resources"]["instance_id"] = instance_id
        ledger["resources"]["volume_id"] = volume_id if isinstance(volume_id, str) else None
        save_ledger(path, ledger)
        if isinstance(volume_id, str):
            aws.json(
                "ec2",
                "create-tags",
                "--resources",
                volume_id,
                "--tags",
                tags(ledger["run_tag"], ledger["run_tag"]),
            )

        wait_for_instance_running(aws, instance_id, args.wait_timeout)
        recover_owned_instance_ids(aws, ledger, path)
        volume_id = ledger["resources"].get("volume_id")
        address = instance_address(aws, instance_id)
        if not address:
            raise LabError(f"instance {instance_id} reached running without a public IPv4")
        address = validate_ipv4(address)
        ledger["connection"]["address"] = address
        save_ledger(path, ledger)
        wait_for_ssh_ready(ledger, args.wait_timeout, args.command_timeout)
        mark(path, ledger, state="ready", ready_at=utc_now())
        print_connection(artifact_dir, ledger)
        return 0
    except BaseException as error:
        mark(path, ledger, state="create-failed", create_error=str(error))
        try:
            recover_owned_instance_ids(aws, ledger, path)
            recover_owned_security_group(aws, ledger, path)
        except BaseException as recovery_error:
            print(f"resource recovery before cleanup failed: {recovery_error}", file=sys.stderr)
        try:
            cleanup_ledger(args, artifact_dir, ledger)
        except BaseException as cleanup_error:
            print(f"automatic cleanup also failed: {cleanup_error}", file=sys.stderr)
        if isinstance(error, LabError):
            raise
        raise LabError(str(error)) from error


def instance_address(aws: Aws, instance_id: str) -> str | None:
    response = aws.json(
        "ec2",
        "describe-instances",
        "--instance-ids",
        instance_id,
    )
    reservations = response.get("Reservations", [])
    instances = reservations[0].get("Instances", []) if reservations else []
    if not instances:
        return None
    address = instances[0].get("PublicIpAddress")
    return address if isinstance(address, str) else None


def wait_for_instance_running(aws: Aws, instance_id: str, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        response = aws.json("ec2", "describe-instances", "--instance-ids", instance_id)
        reservations = response.get("Reservations", [])
        instances = reservations[0].get("Instances", []) if reservations else []
        state = instances[0].get("State", {}).get("Name") if instances else None
        if state == "running":
            return
        if state in {"shutting-down", "terminated"}:
            raise LabError(f"instance {instance_id} entered terminal state {state}")
        time.sleep(min(10, max(1, deadline - time.monotonic())))
    raise LabError(f"instance {instance_id} did not reach running before timeout")


def ssh_arguments(ledger: dict[str, Any], address: str, remote_command: str) -> list[str]:
    connection = ledger["connection"]
    return [
        "ssh",
        "-4",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=8",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        f"UserKnownHostsFile={connection['known_hosts']}",
        "-i",
        connection["private_key"],
        f"{connection['user']}@{address}",
        remote_command,
    ]


def update_known_hosts(ledger: dict[str, Any], address: str, timeout: float) -> None:
    known_hosts = pathlib.Path(ledger["connection"]["known_hosts"])
    # The first key scan pins this disposable host.  Never silently append a
    # changed key on a later poll: a replaced host must be investigated or the
    # private artifact directory must be discarded and recreated.
    if known_hosts.exists() and known_hosts.stat().st_size > 0:
        return
    result = run_command(["ssh-keyscan", "-4", "-T", "8", address], timeout=timeout)
    lines = [line for line in result.stdout.splitlines() if line and not line.startswith("#")]
    if not lines:
        raise LabError(f"ssh-keyscan returned no host key for {address}")
    atomic_write(known_hosts, "\n".join(lines) + "\n")


def ssh(ledger: dict[str, Any], address: str, command: str, timeout: float) -> str:
    result = run_command(ssh_arguments(ledger, address, command), timeout=timeout)
    return result.stdout


def wait_for_ssh_ready(ledger: dict[str, Any], timeout: float, command_timeout: float) -> None:
    address = ledger["connection"].get("address")
    if not isinstance(address, str):
        raise LabError("ledger has no instance address")
    deadline = time.monotonic() + timeout
    last_error = "SSH has not connected yet"
    while time.monotonic() < deadline:
        try:
            update_known_hosts(ledger, address, command_timeout)
            output = ssh(
                ledger,
                address,
                "test -f /var/lib/mjolnir/ssh-docker-ready && docker version --format '{{.Server.Version}}'",
                command_timeout,
            )
            if output.strip():
                ledger["docker_version"] = output.strip().splitlines()[-1]
                return
            last_error = "Docker readiness marker has not appeared"
        except LabError as error:
            last_error = str(error)
        time.sleep(min(15, max(1, deadline - time.monotonic())))
    raise LabError(f"SSH/cloud-init readiness timed out for {address}: {last_error}")


def print_connection(artifact_dir: pathlib.Path, ledger: dict[str, Any]) -> None:
    address = ledger["connection"].get("address")
    command = ssh_arguments(ledger, str(address), "docker version")[:-1]
    print(f"ssh-docker lab ready: run={ledger['run_id']} instance={ledger['resources']['instance_id']}")
    print(f"address={address} user={ledger['connection']['user']} region={ledger['region']}")
    print(f"artifacts={artifact_dir}")
    print(f"ssh={shlex.join(command)}")
    print("The host is intentionally left running for the parent acceptance tests.")


def describe_instance(aws: Aws, instance_id: str) -> dict[str, Any] | None:
    response = aws.optional_json("ec2", "describe-instances", "--instance-ids", instance_id)
    if response is None:
        return None
    reservations = response.get("Reservations", [])
    instances = reservations[0].get("Instances", []) if reservations else []
    return instances[0] if instances else None


def status(args: argparse.Namespace) -> int:
    artifact_dir, ledger, path = open_lab(args)
    aws = Aws(ledger["profile"], ledger["region"], args.command_timeout)
    instance_id = ledger["resources"].get("instance_id")
    if not isinstance(instance_id, str):
        raise LabError("ledger has no instance ID")
    instance = describe_instance(aws, instance_id)
    if instance is None:
        mark(path, ledger, state="instance-missing")
        print(f"instance {instance_id} is absent")
        return 1
    state = instance.get("State", {}).get("Name", "unknown")
    address = instance.get("PublicIpAddress")
    if isinstance(address, str):
        address = validate_ipv4(address)
        ledger["connection"]["address"] = address
        save_ledger(path, ledger)
    ready = False
    if args.wait and state == "running" and isinstance(address, str):
        wait_for_ssh_ready(ledger, args.wait_timeout, args.command_timeout)
        ready = True
    mark(path, ledger, state="ready" if ready else state, public_ip=address)
    print(json.dumps({"run": ledger["run_id"], "instance": instance_id, "state": state, "public_ip": address, "ready": ready}, sort_keys=True))
    return 0 if state not in {"shutting-down", "terminated"} else 1


def collect(args: argparse.Namespace) -> int:
    artifact_dir, ledger, path = open_lab(args)
    aws = Aws(ledger["profile"], ledger["region"], args.command_timeout)
    instance_id = ledger["resources"].get("instance_id")
    address = ledger["connection"].get("address")
    if not isinstance(instance_id, str) or not isinstance(address, str):
        raise LabError("ledger has no instance connection")
    address = validate_ipv4(address)
    if describe_instance(aws, instance_id) is None:
        raise LabError(f"instance {instance_id} is absent")
    try:
        update_known_hosts(ledger, address, args.command_timeout)
    except LabError as error:
        # Keep going so console output and any already reachable logs are still
        # collected when SSH itself is the failing part of bootstrap.
        print(f"SSH host-key collection failed: {error}", file=sys.stderr)
    collect_dir = artifact_dir / "collect"
    ensure_private_directory(collect_dir)
    commands = {
        "cloud-init-status.log": "sudo cloud-init status --long",
        "cloud-init-output.log": "sudo cat /var/log/cloud-init-output.log",
        "user-data.log": "sudo cat /var/log/mj-ssh-docker-user-data.log",
        "docker-version.log": "docker version",
        # Select fields rather than dumping Docker's full info structure,
        # which can include registry or proxy configuration.
        "docker-info.log": "docker info --format 'Server={{.ServerVersion}} Driver={{.Driver}} Containers={{.Containers}} Images={{.Images}}'",
        "docker-service.log": "sudo systemctl --no-pager --full status docker",
    }
    errors: dict[str, str] = {}
    for filename, command in commands.items():
        try:
            content = ssh(ledger, address, command, args.command_timeout)
        except LabError as error:
            content = f"collection failed: {error}\n"
            errors[filename] = str(error)
        atomic_write(collect_dir / filename, content)
    # Console output is useful when the instance never became reachable and
    # contains no credentials from this runner.
    try:
        console = aws.json("ec2", "get-console-output", "--instance-id", instance_id, "--latest")
        atomic_write(collect_dir / "console-output.json", json.dumps(console, indent=2) + "\n")
    except LabError as error:
        errors["console-output.json"] = str(error)
    mark(path, ledger, collected_at=utc_now(), collect_errors=errors)
    print(f"collected={collect_dir}")
    if errors:
        print(f"collection completed with {len(errors)} errors", file=sys.stderr)
        return 1
    return 0


def poll_absent(
    description: str,
    timeout: float,
    fetch: Callable[[], dict[str, Any] | None],
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if fetch() is None:
            return
        time.sleep(min(10, max(1, deadline - time.monotonic())))
    raise LabError(f"{description} was not removed before timeout")


def cleanup_ledger(args: argparse.Namespace, artifact_dir: pathlib.Path, ledger: dict[str, Any]) -> bool:
    path = ledger_path(artifact_dir)
    aws = Aws(ledger["profile"], ledger["region"], args.command_timeout)
    run_tag = ledger["run_tag"]
    resources = ledger["resources"]
    errors: list[str] = []
    try:
        # A lost run-instances response must not turn into an orphan.  The
        # unique ownership tag recovers the exact ID before destructive work.
        recover_owned_instance_ids(aws, ledger, path)
        recover_owned_security_group(aws, ledger, path)
        resources = ledger["resources"]
    except LabError as error:
        errors.append(str(error))
    instance_id = resources.get("instance_id")
    if isinstance(instance_id, str):
        try:
            instance = describe_instance(aws, instance_id)
            if instance is not None:
                if not resource_is_owned(instance, run_tag):
                    raise LabError(f"instance {instance_id} is not owned by run {run_tag}")
                state = instance.get("State", {}).get("Name")
                if state not in {"shutting-down", "terminated"}:
                    aws.json("ec2", "terminate-instances", "--instance-ids", instance_id)
                poll_absent(
                    f"instance {instance_id}",
                    args.cleanup_timeout,
                    lambda: (
                        None
                        if (current := describe_instance(aws, instance_id)) is None
                        or current.get("State", {}).get("Name") == "terminated"
                        else current
                    ),
                )
        except LabError as error:
            errors.append(str(error))

    volume_id = resources.get("volume_id")
    if isinstance(volume_id, str):
        try:
            response = aws.optional_json("ec2", "describe-volumes", "--volume-ids", volume_id)
            volumes = response.get("Volumes", []) if response else []
            volume = volumes[0] if volumes else None
            if volume is not None:
                if not resource_is_owned(volume, run_tag):
                    raise LabError(f"volume {volume_id} is not owned by run {run_tag}")
                if volume.get("State") == "in-use":
                    raise LabError(f"owned volume {volume_id} is still in use")
                try:
                    aws.json("ec2", "delete-volume", "--volume-id", volume_id)
                except CommandError as error:
                    if "InvalidVolume.NotFound" not in str(error):
                        raise
                poll_absent(
                    f"volume {volume_id}",
                    args.cleanup_timeout,
                    lambda: aws.optional_json("ec2", "describe-volumes", "--volume-ids", volume_id),
                )
        except LabError as error:
            errors.append(str(error))

    group_id = resources.get("security_group_id")
    if isinstance(group_id, str):
        try:
            response = aws.optional_json("ec2", "describe-security-groups", "--group-ids", group_id)
            groups = response.get("SecurityGroups", []) if response else []
            if groups:
                if not resource_is_owned(groups[0], run_tag):
                    raise LabError(f"security group {group_id} is not owned by run {run_tag}")
                try:
                    aws.json("ec2", "delete-security-group", "--group-id", group_id)
                except CommandError as error:
                    if "InvalidGroup.NotFound" not in str(error):
                        raise
        except LabError as error:
            errors.append(str(error))

    key_name = resources.get("key_name")
    if isinstance(key_name, str) and (
        ledger.get("key_imported") or ledger.get("key_import_requested")
    ):
        try:
            response = aws.optional_json("ec2", "describe-key-pairs", "--key-names", key_name)
            keys = response.get("KeyPairs", []) if response else []
            if keys:
                if not resource_is_owned(keys[0], run_tag):
                    raise LabError(f"key pair {key_name} is not owned by run {run_tag}")
                aws.json("ec2", "delete-key-pair", "--key-name", key_name)
        except LabError as error:
            if "InvalidKeyPair.NotFound" not in str(error):
                errors.append(str(error))

    if errors:
        mark(path, ledger, state="cleanup-incomplete", cleanup_errors=errors)
        print("cleanup incomplete; exact remaining resources:", file=sys.stderr)
        for error in errors:
            print(f"  {error}", file=sys.stderr)
        return False
    mark(path, ledger, state="cleaned", cleaned_at=utc_now(), cleanup_errors=[])
    for filename in (KEY_NAME, PUBLIC_KEY_NAME, KNOWN_HOSTS_NAME):
        (artifact_dir / filename).unlink(missing_ok=True)
    print(f"cleaned run={ledger['run_id']} artifacts={artifact_dir}")
    return True


def open_lab(args: argparse.Namespace) -> tuple[pathlib.Path, dict[str, Any], pathlib.Path]:
    if args.artifact_dir is None:
        raise LabError("--artifact-dir is required for this command")
    artifact_dir = args.artifact_dir
    ensure_private_directory(artifact_dir)
    path = ledger_path(artifact_dir)
    ledger = load_json(path)
    return artifact_dir, ledger, path


def cleanup(args: argparse.Namespace) -> int:
    artifact_dir, ledger, _ = open_lab(args)
    return 0 if cleanup_ledger(args, artifact_dir, ledger) else 1


def add_common_options(parser: argparse.ArgumentParser, *, suppress_defaults: bool) -> None:
    default = argparse.SUPPRESS if suppress_defaults else None
    parser.add_argument("--profile", default=default or DEFAULT_PROFILE)
    parser.add_argument("--region", default=default or DEFAULT_REGION)
    parser.add_argument("--artifact-dir", type=pathlib.Path, default=default)
    parser.add_argument("--command-timeout", type=float, default=default or 30.0)
    parser.add_argument("--wait-timeout", type=float, default=default or 900.0)
    parser.add_argument("--cleanup-timeout", type=float, default=default or 600.0)


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    add_common_options(root, suppress_defaults=False)
    commands = root.add_subparsers(dest="command", required=True)
    create_parser = commands.add_parser("create", help="create and bootstrap one EC2 Docker host")
    add_common_options(create_parser, suppress_defaults=True)
    create_parser.add_argument("--availability-zone", default=DEFAULT_AZ)
    create_parser.add_argument("--instance-type", default=DEFAULT_INSTANCE_TYPE)
    create_parser.add_argument("--volume-size-gib", type=int, default=DEFAULT_VOLUME_SIZE)
    create_parser.add_argument("--ami-parameter", default=DEFAULT_AMI_PARAMETER)
    create_parser.add_argument("--controller-ip", help="override public IPv4 detection for a controlled lab")
    status_parser = commands.add_parser("status", help="show instance and bounded SSH readiness")
    add_common_options(status_parser, suppress_defaults=True)
    status_parser.add_argument(
        "--wait",
        dest="wait",
        action="store_true",
        default=True,
        help="wait for SSH/cloud-init/Docker readiness (the default)",
    )
    status_parser.add_argument(
        "--no-wait",
        dest="wait",
        action="store_false",
        help="report the EC2 state without waiting for bootstrap",
    )
    collect_parser = commands.add_parser("collect", help="collect non-secret cloud-init and Docker logs")
    add_common_options(collect_parser, suppress_defaults=True)
    cleanup_parser = commands.add_parser("cleanup", help="terminate and remove exactly this run's resources")
    add_common_options(cleanup_parser, suppress_defaults=True)
    return root


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "create":
            return create(args)
        if args.command == "status":
            return status(args)
        if args.command == "collect":
            return collect(args)
        if args.command == "cleanup":
            return cleanup(args)
        raise LabError(f"unknown command: {args.command}")
    except LabError as error:
        print(f"ssh-docker lab: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
