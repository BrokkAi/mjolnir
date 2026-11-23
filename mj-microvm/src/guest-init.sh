#!/bin/bash
# The microVM's /init, run from the dracut initrd. The engine builds the initrd (boot.rs) and
# passes per-VM parameters on the kernel command line:
#   sbx.name=<hostname> sbx.ip=<addr/len> sbx.gw=<addr> sbx.dns=<addr>
#   sbx.vols=<dev>:<destination>:<rw|ro>:<new|old>[,...]   (devices in Firecracker drive order after the root)
# It prepares the machine, then replaces itself with the guest agent (PID 1) in the image's root.
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /dev/shm /run /newroot
mount -t devpts -o gid=5,mode=620 devpts /dev/pts
mount -t tmpfs -o mode=1777 tmpfs /dev/shm
mount -t tmpfs -o mode=755 tmpfs /run
log() { echo "init[$(cut -d' ' -f1 /proc/uptime)]: $*" | tee /dev/kmsg >/dev/console 2>/dev/null; }
fatal() { log "FATAL: $*"; sleep 2; echo o > /proc/sysrq-trigger; exec sleep infinity; }

for a in $(cat /proc/cmdline); do
  case $a in sbx.*=*) k=${a%%=*}; v=${a#*=}; declare "${k//./_}=$v";; esac
done
log "booting name=$sbx_name ip=$sbx_ip gw=$sbx_gw"

# Everything nested containers and nftables need; failures are logged, not fatal.
for m in @GUEST_DRIVERS@; do
  modprobe $m 2>/dev/null || log "modprobe $m failed"
done

# No udev: device modes and the /dev/fd links are set here.
chmod 666 /dev/net/tun /dev/fuse 2>/dev/null
ln -sf /proc/self/fd /dev/fd; ln -sf /proc/self/fd/0 /dev/stdin; ln -sf /proc/self/fd/1 /dev/stdout; ln -sf /proc/self/fd/2 /dev/stderr

# Block devices appear in Firecracker configuration order: vda is the root.
for i in $(seq 1 100); do [ -b /dev/vda ] && break; sleep 0.1; done
[ -b /dev/vda ] || fatal "no /dev/vda"
mount -t ext4 -o rw,noatime /dev/vda /newroot || fatal "mount root failed"

# Volumes. A volume that is new (the engine says `new` until a container has started with it, and it
# holds nothing the image did not put there) is filled from the image directory it
# covers, with that directory's owner and mode, the way a named volume copies up from the image.
IFS=',' read -ra vols <<< "$sbx_vols"
for entry in "${vols[@]}"; do
  [ -n "$entry" ] || continue
  dev=${entry%%:*}; rest=${entry#*:}; dest=${rest%%:*}; opts=${rest#*:}; mode=${opts%%:*}; fresh=${opts#*:}
  for i in $(seq 1 100); do [ -b "/dev/$dev" ] && break; sleep 0.1; done
  [ -b "/dev/$dev" ] || fatal "no /dev/$dev for $dest"
  mkdir -p "/newroot$dest"
  covered=$(stat -c '%u:%g %a' "/newroot$dest")
  # A `new` volume has never been used by a container (the engine attaches its drive writable even
  # for `ro`, this once), so anything on it is from an interrupted copy: start the copy over.
  if [ "$fresh" = new ]; then
    mkdir -p /volume-staging
    mount -t ext4 -o "rw,noatime" "/dev/$dev" /volume-staging || fatal "mount $dev failed"
    (shopt -s dotglob nullglob; for f in /volume-staging/*; do [ "${f##*/}" = lost+found ] || rm -rf "$f"; done)
    cp -a "/newroot$dest/." /volume-staging/ || fatal "copy $dest into its volume failed"
    chown "${covered% *}" /volume-staging; chmod "${covered#* }" /volume-staging
    umount /volume-staging || fatal "unmount staging for $dev failed"
  fi
  mount -t ext4 -o "$mode,noatime" "/dev/$dev" "/newroot$dest" || fatal "mount $dev on $dest failed"
done

# Kernel modules for later modprobe in the guest. A bind mount would be deleted by switch_root
# (same device as the ramfs root); a tmpfs copy survives.
mkdir -p /newroot/usr/lib/modules
mount -t tmpfs -o mode=755 tmpfs /newroot/usr/lib/modules && cp -a /usr/lib/modules/. /newroot/usr/lib/modules/

# The guest agent is the engine binary carried in the initrd; it must live on a filesystem that
# survives switch_root, and it always matches the engine that launched this VM.
mkdir -p /newroot/.mj-microvm
mount -t tmpfs -o mode=755 tmpfs /newroot/.mj-microvm || fatal "mount agent tmpfs failed"
cp /mj-worker /newroot/.mj-microvm/mj-worker || fatal "copy agent failed"
chmod 755 /newroot/.mj-microvm/mj-worker

# Network
ip link set lo up
IF=$(ls /sys/class/net | grep -v '^lo$' | head -1)
[ -n "$IF" ] || fatal "no network interface"
ip addr add "$sbx_ip" dev "$IF"; ip link set "$IF" up
ip route add default via "$sbx_gw"
log "net up on $IF"

# An image may ship these as symlinks (systemd-resolved, /proc/sys): replace them with regular files.
mkdir -p /newroot/etc
rm -f /newroot/etc/hostname /newroot/etc/hosts /newroot/etc/resolv.conf
echo "$sbx_name" > /newroot/etc/hostname; hostname "$sbx_name"
printf '127.0.0.1 localhost\n%s %s\n::1 localhost\n' "${sbx_ip%/*}" "$sbx_name" > /newroot/etc/hosts
printf 'nameserver %s\n' "${sbx_dns:-169.254.169.253}" > /newroot/etc/resolv.conf

[ -e /newroot/var/run ] || [ -L /newroot/var/run ] || (mkdir -p /newroot/var && ln -s /run /newroot/var/run)

if mkdir -p /sys/fs/cgroup && mount -t cgroup2 cgroup2 /sys/fs/cgroup 2>/dev/null; then
  # Put the guest agent and its children in a separate cgroup. Docker creates
  # container cgroups under the root and needs the CPU and memory controllers.
  mkdir -p /sys/fs/cgroup/init && echo 0 > /sys/fs/cgroup/init/cgroup.procs || log "could not move init into its cgroup"
  for c in $(cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null); do
    echo "+$c" > /sys/fs/cgroup/cgroup.subtree_control || log "could not enable cgroup controller $c"
  done
else
  log "cgroup2 mount failed"
fi
echo 4 > /proc/sys/kernel/printk 2>/dev/null   # console: errors and worse
mount --make-rshared /newroot
log "switch_root"
exec switch_root /newroot /.mj-microvm/mj-worker microvm guest-agent
