#!/bin/bash
# Acceptance on a fresh systemd VM. A successful run ends with a purged package
# and the test node's retained data, so removal semantics can be inspected.
set -euo pipefail
if [[ ${1:-} != --disposable-systemd-host || $# != 2 || $EUID != 0 ]]; then
    echo "usage: sudo $0 --disposable-systemd-host path/to/psios-core.deb" >&2
    exit 2
fi
package=$(realpath "$2")
[[ $(dpkg-deb -f "$package" Package) == psios-core && -d /run/systemd/system ]]
for path in /etc/psios /var/lib/psios /var/lib/psios.witness /run/psios /usr/bin/psios{d,ctl}; do
    [[ ! -e $path && ! -L $path ]] || { echo "$path already exists" >&2; exit 2; }
done
if dpkg-query -W psios-core >/dev/null 2>&1 || getent passwd psios >/dev/null ||
    getent group psios >/dev/null || systemctl cat psiosd.service >/dev/null 2>&1 ||
    systemctl cat psios-init.service >/dev/null 2>&1; then
    echo "acceptance requires an unused host" >&2
    exit 2
fi
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT

# Use the same payload at an earlier Debian revision to exercise dpkg's upgrade
# path. Cross-schema upgrades use the actual PR 1 binary in tests/upgrade.py.
dpkg-deb -R "$package" "$scratch/earlier"
sed -i '/^Version:/s/$/~acceptance/' "$scratch/earlier/DEBIAN/control"
dpkg-deb --build --root-owner-group "$scratch/earlier" "$scratch/earlier.deb"
dpkg -i "$scratch/earlier.deb"
[[ ! -e /var/lib/psios.witness/owner.token ]]
if systemctl is-enabled --quiet psiosd || systemctl is-active --quiet psiosd; then exit 1; fi
systemd-analyze verify psios-init.service psiosd.service
config=/etc/psios/core.json
jq '.workers = 2' "$config" > "$scratch/local.json"
cat "$scratch/local.json" > "$config"
systemctl start psios-init
systemctl enable --now psiosd
ctl() { runuser -u psios -- /usr/bin/psiosctl --socket /run/psios/api.sock "$@"; }
ready() {
    for _ in {1..100}; do
        if ctl whoami >/dev/null 2>&1; then return 0; fi
        sleep 0.1
    done
    systemctl status psiosd --no-pager >&2
    return 1
}
ready
for path in /var/lib/psios /var/lib/psios.witness; do
    [[ $(stat -c '%U:%G:%a' "$path") == psios:psios:700 ]]
done
[[ $(stat -c '%U:%G:%a' /var/lib/psios.witness/owner.token) == psios:psios:600 ]]
[[ $(stat -c '%U:%G:%a' /run/psios/api.sock) == psios:psios:660 ]]
node=$(ctl node | jq -er '.reference.resource_id')
task=$(echo '{}' | ctl submit system.status --key package-acceptance | jq -er '.task_id')
ctl wait "$task" | jq -e '.lifecycle == "Succeeded"' >/dev/null
cp /var/lib/psios.witness/owner.token "$scratch/token"
pid=$(systemctl show -p MainPID --value psiosd)
dpkg -i "$package"
ready
[[ $(systemctl show -p MainPID --value psiosd) != "$pid" ]]
cmp "$scratch/local.json" "$config"
cmp "$scratch/token" /var/lib/psios.witness/owner.token
ctl node | jq -e --arg id "$node" '.reference.resource_id == $id and .state.configuration.worker_count == 2' >/dev/null
ctl wait "$task" | jq -e '.lifecycle == "Succeeded"' >/dev/null

systemctl stop psiosd
dpkg -i "$package"
if systemctl is-active --quiet psiosd; then exit 1; fi
# Simulate loss of part of the authority in this test node. Neither package
# configuration nor explicit provisioning may replace it with a fresh one.
mv /var/lib/psios.witness/head "$scratch/head"
if systemctl start psios-init; then exit 1; fi
if runuser -u psios -- psiosd --config "$config" check; then exit 1; fi
[[ ! -e /var/lib/psios.witness/head ]]
cmp "$scratch/token" /var/lib/psios.witness/owner.token
mv "$scratch/head" /var/lib/psios.witness/head
systemctl start psiosd
ready
dpkg --purge psios-core
[[ ! -e /usr/bin/psiosd && ! -e /run/psios/api.sock && ! -e $config ]]
[[ -f /var/lib/psios/state.db ]]
cmp "$scratch/token" /var/lib/psios.witness/owner.token
dpkg -i "$package"
if systemctl is-active --quiet psiosd || systemctl is-enabled --quiet psiosd; then exit 1; fi
systemctl start psiosd
ready
[[ $(ctl node | jq -er '.reference.resource_id') == "$node" ]]
ctl wait "$task" | jq -e '.lifecycle == "Succeeded"' >/dev/null
dpkg --purge psios-core
echo "Package installation, upgrade, recovery refusal, purge and reinstall passed"
