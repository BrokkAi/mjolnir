# Only EC2 instance-store devices may enter this pool; EBS holds the OS.
cat > /usr/local/sbin/mj-ec2-cache <<'CACHE'
#!/bin/bash
set -euo pipefail
modprobe zfs
mapfile -t disks < <(for model in /sys/block/nvme*n1/device/model; do
    if [ "$(xargs < "$model")" = 'Amazon EC2 NVMe Instance Storage' ]; then
        device=${model#/sys/block/}; device=${device%%/*}
        printf '/dev/%s\n' "$device"
    fi
done)
if [ "${#disks[@]}" -eq 0 ]; then
    echo 'No local EC2 NVMe instance-store SSDs found' >&2
    exit 1
fi
if ! zpool list -H -o name mjcache >/dev/null 2>&1; then
    signatures=
    for disk in "${disks[@]}"; do
        signatures+=$(wipefs --no-act --noheadings --output TYPE "$disk")
    done
    if [ -z "$signatures" ]; then
        zpool create -o ashift=12 -O compression=lz4 -O recordsize=128K -O xattr=sa \
            -O atime=on -O relatime=on -m /mnt/nvme mjcache "${disks[@]}"
    else
        # Existing signatures authorize only importing our own pool.
        zpool import -d /dev mjcache
    fi
fi
zfs mount -a
install -d -o ubuntu -g ubuntu /mnt/nvme/mbx
install -d -m 0700 -o ubuntu -g ubuntu /mnt/nvme/podman
CACHE
chmod 0755 /usr/local/sbin/mj-ec2-cache
cat > /etc/systemd/system/mj-ec2-cache.service <<'UNIT'
[Unit]
Description=Prepare local NVMe ZFS container storage and build cache
After=zfs-import.target
Before=multi-user.target
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/mj-ec2-cache
RemainAfterExit=yes
[Install]
WantedBy=multi-user.target
UNIT
# Linger services must not initialize a replacement store on EBS while ZFS imports.
install -d /etc/systemd/system/user@1000.service.d
cat > /etc/systemd/system/user@1000.service.d/mj-nvme.conf <<'UNIT'
[Unit]
Requires=mj-ec2-cache.service
After=mj-ec2-cache.service
UNIT
systemctl daemon-reload
systemctl enable --now mj-ec2-cache.service
install -d -o ubuntu -g ubuntu /home/ubuntu/.config/mbx
printf 'cache_dir = "/mnt/nvme/mbx"\n\n[linker]\ndefault = "path:/usr/bin/mold"\n' > /home/ubuntu/.config/mbx/config.toml
chown ubuntu:ubuntu /home/ubuntu/.config/mbx/config.toml

# Configure storage before the first Podman command initializes its database.
# Each mj container's existing dedicated /tmp volume lives under this graphroot.
install -d -m 0700 -o ubuntu -g ubuntu /home/ubuntu/.config/containers
cat > /home/ubuntu/.config/containers/storage.conf <<'STORAGE'
[storage]
driver = "overlay"
runroot = "/run/user/1000/containers"
graphroot = "/mnt/nvme/podman"

[storage.options.overlay]
mount_program = "/usr/bin/fuse-overlayfs"
mountopt = "nodev"
STORAGE
chown ubuntu:ubuntu /home/ubuntu/.config/containers/storage.conf
chmod 0600 /home/ubuntu/.config/containers/storage.conf
