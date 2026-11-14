#!/bin/bash
# Runs ON the parent. Flattens the OCI image to a directory tree, adds the guest-side packages the
# image lacks (podman, iproute2, kmod, socat), and builds raw ext4 root + workspace images.
# Usage: 02-build-image.sh [image-ref]    Env: SKIP_EXPORT=1 reuses an existing tree.
set -euo pipefail
IMG=${1:-ghcr.io/brokkai/mjolnir/agent-dev:latest}
D=/var/lib/sandboxes; sudo mkdir -p $D/base $D/sbx; sudo chown ec2-user: $D $D/base $D/sbx
T=$D/base/rootfs-extract
now() { date +%s.%N; }
el() { echo "$(now) - $1" | bc; }

if [ "${SKIP_EXPORT:-0}" != 1 ]; then
  echo "== export OCI image to tree"; s=$(now)
  sudo rm -rf $T; sudo mkdir -p $T
  crane export --platform linux/amd64 "$IMG" - | sudo tar -x --numeric-owner --xattrs -C $T
  echo "TIME export+extract: $(el $s) s"
  sudo du -sm $T | awk '{print "extracted MiB:",$1}'
fi

echo "== customize tree (chroot apt-get)"; s=$(now)
sudo mkdir -p $T/proc $T/sys $T/dev $T/run $T/tmp $T/workspace
sudo mount --bind /dev $T/dev; sudo mount -t proc proc $T/proc
trap 'sudo umount $T/proc $T/dev 2>/dev/null || true' EXIT
sudo chroot $T /bin/bash -euxc '
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends podman crun iproute2 kmod socat uidmap fuse-overlayfs passt netavark aardvark-dns nftables iputils-ping dnsutils ca-certificates util-linux
  rm -rf /var/lib/apt/lists/*
  # rootless podman wants the hel user to own its runtime dir; created at boot (tmpfs /run)
  # The image ships /etc/containers/* for podman-remote and symlinks /usr/local/bin/podman -> podman-remote.
  # Use the real engine by absolute path (/usr/bin/podman) and add only drop-ins; do not clobber image files.
  mkdir -p /etc/containers/containers.conf.d /etc/containers/registries.conf.d
  printf "[engine]\ncgroup_manager=\"cgroupfs\"\nevents_logger=\"file\"\n" > /etc/containers/containers.conf.d/50-sandbox.conf
' 2>&1 | tail -5
sudo umount $T/proc $T/dev; trap - EXIT
sudo install -m 755 /dev/stdin $T/usr/local/bin/vsock-exec <<'XEOF'
#!/bin/bash
# One connection = one request. First line: "<user>:<command>". Output: command stdout+stderr, then "EXIT:<rc>".
IFS= read -r line
u=${line%%:*}; c=${line#*:}
if [ "$u" = root ]; then bash -c "$c" 2>&1; else runuser -u "$u" -- env XDG_RUNTIME_DIR=/run/user/$(id -u "$u") HOME=/home/$u bash -c "cd /workspace 2>/dev/null; $c" 2>&1; fi
echo "EXIT:$?"
XEOF
sudo install -m 755 /dev/stdin $T/usr/local/bin/sandbox-agent <<'XEOF'
#!/bin/bash
# PID 1 after switch_root (spike stand-in for the Rust agent): listen on vsock port 1024.
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
mkdir -p /run/user/1000 && chown 1000:1000 /run/user/1000 && chmod 700 /run/user/1000
echo "agent-ready $(cut -d' ' -f1 /proc/uptime)" > /dev/kmsg 2>/dev/null
exec socat -T600 VSOCK-LISTEN:1024,reuseaddr,fork EXEC:/usr/local/bin/vsock-exec
XEOF
echo "TIME customize: $(el $s) s"; sudo du -sm $T | awk '{print "tree MiB after customize:",$1}'

echo "== mkfs.ext4 -d root"; s=$(now)
USED_MB=$(sudo du -sm $T | cut -f1)
SIZE_MB=$(( USED_MB * 12 / 10 + 1024 ))   # +20% and 1 GiB headroom
rm -f $D/base/root.ext4; truncate -s ${SIZE_MB}M $D/base/root.ext4
sudo mkfs.ext4 -q -F -E root_owner=0:0 -L rootfs -d $T $D/base/root.ext4
echo "TIME mkfs.ext4 -d: $(el $s) s; image ${SIZE_MB} MiB (apparent)"
rm -f $D/base/workspace.ext4; truncate -s 20G $D/base/workspace.ext4
sudo mkfs.ext4 -q -F -E root_owner=1000:1000 -L workspace $D/base/workspace.ext4
ls -ls $D/base/
