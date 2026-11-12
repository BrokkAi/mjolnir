# Ubuntu's ssh.service creates /run/sshd; ssh.socket alone does not.
ssh_unit=
for candidate in ssh.service sshd.service; do
    if systemctl cat "$candidate" >/dev/null 2>&1; then
        ssh_unit=$candidate
        break
    fi
done
if [ -z "$ssh_unit" ]; then
    echo "Neither ssh.service nor sshd.service exists; cannot start SSH" >&2
    exit 1
fi
systemctl start "$ssh_unit"
# Measured relay-session refusals at ~40 containers on morannon (Series 20/21).
mkdir -p /etc/ssh/sshd_config.d
cat > /etc/ssh/sshd_config.d/60-mj-mux.conf <<'MJ_SSHD'
# mj multiplexes worker relay sessions over shared SSH connections.
# Measured relay-session refusals at ~40 containers on morannon (Series 20/21).
MaxSessions 200
MaxStartups 100:30:300
MJ_SSHD
# Prepend the include so earlier global settings cannot override the drop-in.
sed -i '1i Include /etc/ssh/sshd_config.d/*.conf' /etc/ssh/sshd_config
sshd -t
systemctl reload "$ssh_unit"
