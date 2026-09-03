#!/bin/sh
# Example host helper for Podman workspace_storage = { kind = "host-helper", ... }.
#
# Copy this file to a root-owned path such as
# /usr/local/libexec/mj-zfs-workspace-helper, edit the two constants below,
# and permit only that path through sudo. Mj invokes:
#
#   sudo -n /usr/local/libexec/mj-zfs-workspace-helper create RESOURCE
#   sudo -n /usr/local/libexec/mj-zfs-workspace-helper destroy RESOURCE
#   sudo -n /usr/local/libexec/mj-zfs-workspace-helper status RESOURCE
#
# Do not grant direct sudo access to zfs: this script's fixed parent dataset
# and strict resource-name check are the security boundary.

set -eu

ZFS=/usr/sbin/zfs
CHOWN=/usr/bin/chown
GREP=/usr/bin/grep
ID=/usr/bin/id
DATASET_ROOT=nvme/mj-workspaces
MOUNT_ROOT=/mnt/nvme/mj-workspaces

action=${1-}
resource=${2-}
[ "$#" -eq 2 ] || {
    echo "usage: $0 {create|destroy|status} RESOURCE" >&2
    exit 2
}
if ! printf '%s\n' "$resource" | "$GREP" -Eq '^mj-[a-z0-9]{1,12}-[0-9a-f]{6}-workspace$'; then
    echo "refusing invalid Mjolnir workspace resource: $resource" >&2
    exit 2
fi

dataset=$DATASET_ROOT/$resource
mountpoint=$MOUNT_ROOT/$resource

present() {
    "$ZFS" list -H -o name "$dataset" >/dev/null 2>&1
}

case $action in
    status)
        if present; then printf '%s\n' present; else printf '%s\n' absent; fi
        ;;
    create)
        [ "$("$ID" -u)" -eq 0 ] || { echo "create requires root" >&2; exit 1; }
        if ! present; then
            "$ZFS" create -o "mountpoint=$mountpoint" "$dataset"
        fi
        [ "$("$ZFS" get -H -o value mountpoint "$dataset")" = "$mountpoint" ] || {
            echo "refusing dataset with unexpected mountpoint: $dataset" >&2
            exit 1
        }
        case ${SUDO_UID-}:${SUDO_GID-} in
            ''|:|*[!0-9:]*) echo "create requires numeric SUDO_UID and SUDO_GID" >&2; exit 1 ;;
        esac
        "$CHOWN" "${SUDO_UID}:${SUDO_GID}" "$mountpoint"
        ;;
    destroy)
        [ "$("$ID" -u)" -eq 0 ] || { echo "destroy requires root" >&2; exit 1; }
        if present; then
            [ "$("$ZFS" get -H -o value mountpoint "$dataset")" = "$mountpoint" ] || {
                echo "refusing dataset with unexpected mountpoint: $dataset" >&2
                exit 1
            }
            "$ZFS" destroy -r "$dataset"
        fi
        ;;
    *) echo "unknown action: $action" >&2; exit 2 ;;
esac
