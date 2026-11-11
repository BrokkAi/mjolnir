#!/bin/bash
# Runs ON the parent. Copies the host's EFI-stub kernel and builds the dracut initrd from guest/sandbox-init.
set -euo pipefail
D=/var/lib/sandboxes; mkdir -p $D/kernel
K=$(uname -r)
cp /boot/vmlinuz-$K $D/kernel/vmlinuz
s=$(date +%s.%N)
sudo dracut --force --no-hostonly --reproducible --no-early-microcode \
  --modules bash \
  --add-drivers "virtio_pci virtio_blk virtio_net ext4 vmw_vsock_virtio_transport nsm overlay fuse tun veth bridge br_netfilter nf_tables nft_ct nft_nat nft_masq nft_chain_nat nft_compat nf_conntrack nf_nat nft_reject_inet nft_reject nf_reject_ipv4 xt_MASQUERADE xt_conntrack xt_addrtype xt_comment xt_mark xt_nat iptable_nat" \
  --install "mount umount modprobe ip switch_root mkdir hostname cat cut sleep cp ls seq grep head tee echo ln chmod" \
  --include "$HOME/spike/sandbox-init" /init \
  --kver $K $D/kernel/initrd.img
sudo chown "$(id -un):" $D/kernel/initrd.img
echo "initrd built in $(echo "$(date +%s.%N) - $s" | bc) s"; ls -l $D/kernel
echo $K > $D/kernel/KVER
