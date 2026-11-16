# Format the secondary gp3 EBS volume as XFS (with reflinks for mbx), mount it
# at /mnt/ebs, and bind-mount /tmp and /home/ubuntu onto it so Mjolnir caches,
# uploads, temporary files, and EBS-backed Podman storage stay off the OS disk.
udevadm settle
root_source=$(findmnt -n -o SOURCE /)
root_parent=$(lsblk -ndo PKNAME "$root_source")
if [ -z "$root_parent" ]; then
    echo "Cannot determine parent disk of / ($root_source)" >&2
    exit 1
fi
root_disk="/dev/$root_parent"
mapfile -t ebs_disks < <(for model in /sys/block/nvme*n1/device/model; do
    [ -e "$model" ] || continue
    if [ "$(xargs < "$model")" = 'Amazon Elastic Block Store' ]; then
        device="/dev/${model#/sys/block/}"; device=${device%/device/model}
        if [ "$device" != "$root_disk" ]; then
            printf '%s\n' "$device"
        fi
    fi
done)
if [ "${#ebs_disks[@]}" -eq 0 ] && [ "$root_disk" = /dev/xvda ] && [ -b /dev/xvdf ]; then
    ebs_disks=(/dev/xvdf)
fi
if [ "${#ebs_disks[@]}" -ne 1 ]; then
    echo "Expected 1 secondary EBS volume besides $root_disk, found ${#ebs_disks[@]}: ${ebs_disks[*]:-none}" >&2
    exit 1
fi
ebs_disk=${ebs_disks[0]}
fs_type=$(blkid -s TYPE -o value "$ebs_disk" || true)
if [ -z "$fs_type" ]; then
    signatures=$(wipefs --no-act --noheadings --output TYPE "$ebs_disk")
    if [ -n "$signatures" ]; then
        echo "Refusing to format $ebs_disk with non-filesystem signatures: $signatures" >&2
        exit 1
    fi
    mkfs.xfs -m crc=1,reflink=1 "$ebs_disk"
elif [ "$fs_type" != xfs ]; then
    echo "Expected XFS or blank secondary EBS volume on $ebs_disk, found $fs_type" >&2
    exit 1
fi
udevadm settle
ebs_uuid=$(blkid -s UUID -o value "$ebs_disk")
if [ -z "$ebs_uuid" ]; then
    echo "Secondary EBS volume $ebs_disk has no filesystem UUID" >&2
    exit 1
fi
mkdir -p /mnt/ebs
if ! mountpoint -q /mnt/ebs; then
    mount "UUID=$ebs_uuid" /mnt/ebs
fi
install -d -m 1777 -o root -g root /mnt/ebs/tmp
install -d -o ubuntu -g ubuntu /mnt/ebs/mbx
if [ ! -d /mnt/ebs/ubuntu ]; then
    mkdir -p /mnt/ebs/ubuntu
    cp -a /home/ubuntu/. /mnt/ebs/ubuntu/
fi
awk '$1 !~ /^#/ && $2 == "/mnt/ebs" { found=1 } END { exit !found }' /etc/fstab \
    || printf 'UUID=%s /mnt/ebs xfs defaults 0 2\n' "$ebs_uuid" >> /etc/fstab
awk '$1 !~ /^#/ && $2 == "/tmp" { found=1 } END { exit !found }' /etc/fstab \
    || printf '/mnt/ebs/tmp /tmp none bind,x-systemd.requires-mounts-for=/mnt/ebs 0 0\n' >> /etc/fstab
awk '$1 !~ /^#/ && $2 == "/home/ubuntu" { found=1 } END { exit !found }' /etc/fstab \
    || printf '/mnt/ebs/ubuntu /home/ubuntu none bind,x-systemd.requires-mounts-for=/mnt/ebs 0 0\n' >> /etc/fstab
install -d /etc/systemd/system/user@1000.service.d
cat > /etc/systemd/system/user@1000.service.d/mj-ebs.conf <<'UNIT'
[Unit]
RequiresMountsFor=/mnt/ebs /home/ubuntu /tmp
UNIT
systemctl daemon-reload
mountpoint -q /tmp || mount /tmp
mountpoint -q /home/ubuntu || mount /home/ubuntu
chmod 1777 /tmp
for path in /mnt/ebs /tmp /home/ubuntu; do
    if [ "$(findmnt -n -o FSTYPE --target "$path")" != xfs ]; then
        echo "$path is not mounted on XFS" >&2
        exit 1
    fi
done
reflink_probe=$(mktemp /mnt/ebs/mbx/.mj-reflink.XXXXXX)
printf x > "$reflink_probe"
cp --reflink=always "$reflink_probe" "$reflink_probe.clone"
rm -f "$reflink_probe" "$reflink_probe.clone"
