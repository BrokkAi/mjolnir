# Rootless Podman containers share the one host user's inotify instances and
# keyrings. Ubuntu's defaults fail worker bootstrap with EMFILE at about 30
# containers and container launches at about 35 (#1019, #1031, #1540).
cat > /etc/sysctl.d/90-mj-limits.conf <<'MJ_LIMITS'
fs.inotify.max_user_instances = 8192
fs.inotify.max_user_watches = 1048576
user.max_inotify_instances = 8192
kernel.keys.maxkeys = 4096
kernel.keys.maxbytes = 409600
MJ_LIMITS
sysctl --system
