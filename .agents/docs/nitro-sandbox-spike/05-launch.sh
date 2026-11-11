#!/bin/bash
# Runs ON the parent. Launch sandbox <N> (1..) as transient unit sandbox-<N>.service.
# Usage: 05-launch.sh N [--fresh] [--mem MiB] [--vcpus N] [--no-debug] [--confidential]
#   --fresh: (re)create disks from the base images with cp --reflink=always; otherwise reuse retained disks.
set -euo pipefail
N=$1; shift
FRESH=0; MEM=4096; VCPUS=2; DEBUG=true; DIRECT=true
while [ $# -gt 0 ]; do case $1 in --fresh) FRESH=1;; --mem) MEM=$2; shift;; --vcpus) VCPUS=$2; shift;; --no-debug) DEBUG=false;; --confidential) DIRECT=false;; esac; shift; done
D=/var/lib/sandboxes; S=$D/sbx/$N; R=/run/sandbox/$N; TAP=tap$N; UNIT=sandbox-$N
now() { date +%s.%N; }
t0=$(now)
if systemctl is-active --quiet $UNIT; then echo "$UNIT already running"; exit 1; fi
# $R (/run/sandbox/N) is the unit's RuntimeDirectory: systemd creates it empty at start and deletes it at
# stop, so a stale v.sock (Firecracker: "Address in use") cannot survive between launches.
mkdir -p $S
if [ $FRESH = 1 ] || [ ! -e $S/root.ext4 ]; then
  rm -f $S/root.ext4 $S/workspace.ext4
  s=$(now); cp --reflink=always $D/base/root.ext4 $S/root.ext4; cp --reflink=always $D/base/workspace.ext4 $S/workspace.ext4
  echo "TIME reflink clone of both disks: $(echo "$(now) - $s" | bc) s"
fi
cat > $S/fc.json <<JSON
{
  "boot-source": {
    "kernel_image_path": "$D/kernel/vmlinuz",
    "initrd_path": "$D/kernel/initrd.img",
    "boot_args": "panic=1 initcall_blacklist=i8042_init sbx.name=sbx$N sbx.ip=10.77.$N.2/30 sbx.gw=10.77.$N.1 sbx.dns=169.254.169.253"
  },
  "machine-config": { "vcpu_count": $VCPUS, "mem_size_mib": $MEM },
  "drives": [
    { "drive_id": "rootfs",    "path_on_host": "$S/root.ext4",      "is_root_device": true,  "is_read_only": false },
    { "drive_id": "workspace", "path_on_host": "$S/workspace.ext4", "is_root_device": false, "is_read_only": false }
  ],
  "network-interfaces": [ { "iface_id": "eth0", "host_dev_name": "$TAP", "guest_mac": "06:00:0a:4d:$(printf %02x $N):02" } ],
  "vsock": { "guest_cid": 3, "uds_path": "$R/v.sock" },
  "enclave": { "direct_mode": $DIRECT, "cpu_overcommit": true, "debug_mode": $DEBUG }
}
JSON
echo "$(now)" > $S/launch.ts
sudo systemd-run --quiet --unit=$UNIT --collect --description="Nitro sandbox $N" \
  --property=KillMode=control-group --property=TimeoutStopSec=30 \
  --property=RuntimeDirectory=sandbox/$N --property=RuntimeDirectoryPreserve=no \
  --property="ExecStartPre=/usr/local/bin/sbx-tap up $N" --property="ExecStopPost=/usr/local/bin/sbx-tap down $N" \
  /usr/local/bin/firecracker --no-api --config-file $S/fc.json
# Wait for the vsock agent to answer
for i in $(seq 1 600); do
  if [ -S $R/v.sock ] && out=$(sudo /usr/local/bin/vsh $N root 'echo ready' 2>/dev/null) && [ "$out" != "${out#ready}" ]; then
    echo "TIME launch -> vsock ready: $(echo "$(now) - $t0" | bc) s (sandbox $N)"; exit 0
  fi
  systemctl is-active --quiet $UNIT || { echo "FAILED: $UNIT exited"; journalctl -u $UNIT --no-pager | tail -20; exit 1; }
  sleep 0.1
done
echo "TIMEOUT waiting for vsock"; journalctl -u $UNIT --no-pager | tail -30; exit 1
