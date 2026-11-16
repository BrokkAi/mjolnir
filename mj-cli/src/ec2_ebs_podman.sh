# `install -d` creates missing parents as root, so make ~/.config and ~/.local
# the user's own first; otherwise ubuntu cannot create directories beside these.
install -d -o ubuntu -g ubuntu /home/ubuntu/.config
install -d -o ubuntu -g ubuntu /home/ubuntu/.config/mbx
printf 'cache_dir = "/mnt/ebs/mbx"\n\n[linker]\ndefault = "path:/usr/bin/mold"\n' > /home/ubuntu/.config/mbx/config.toml
chown ubuntu:ubuntu /home/ubuntu/.config/mbx/config.toml

# Configure storage before the first Podman command initializes its database.
# /home/ubuntu is mounted on XFS (ftype=1), so native kernel rootless overlay
# works directly without fuse-overlayfs and containers' /tmp volumes use EBS XFS.
install -d -o ubuntu -g ubuntu /home/ubuntu/.local
install -d -m 0700 -o ubuntu -g ubuntu /home/ubuntu/.local/share
install -d -m 0700 -o ubuntu -g ubuntu /home/ubuntu/.local/share/containers
install -d -m 0700 -o ubuntu -g ubuntu /home/ubuntu/.local/share/containers/storage
install -d -m 0700 -o ubuntu -g ubuntu /home/ubuntu/.config/containers
cat > /home/ubuntu/.config/containers/storage.conf <<'STORAGE'
[storage]
driver = "overlay"
runroot = "/run/user/1000/containers"
graphroot = "/home/ubuntu/.local/share/containers/storage"
STORAGE
chown ubuntu:ubuntu /home/ubuntu/.config/containers/storage.conf
chmod 0600 /home/ubuntu/.config/containers/storage.conf
