#!/bin/bash
# Acceptance on a fresh systemd VM. A successful run ends with a purged package
# and the test node's retained data, so removal semantics can be inspected.
set -euo pipefail
trap 'echo "Package acceptance failed at line $LINENO" >&2' ERR
if [[ ${1:-} != --disposable-systemd-host || $# != 3 || $EUID != 0 ]]; then
    echo "usage: sudo $0 --disposable-systemd-host previous.deb current.deb" >&2
    exit 2
fi
previous=$(realpath "$2")
package=$(realpath "$3")
[[ $(dpkg-deb -f "$previous" Package) == psios-core ]]
old_version=$(dpkg-deb -f "$previous" Version)
new_version=$(dpkg-deb -f "$package" Version)
dpkg --compare-versions "$old_version" lt "$new_version"
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

dpkg -i "$previous"
[[ $(dpkg-query -W -f='${Version}' psios-core) == "$old_version" ]]
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
owner_ref=$(ctl get owner-delegation | jq -c .reference)
task=$(echo '{}' | ctl submit system.status --key package-acceptance | jq -er '.task_id')
ctl wait "$task" | jq -e '.lifecycle == "Succeeded"' >/dev/null
cp /var/lib/psios.witness/owner.token "$scratch/token"
pid=$(systemctl show -p MainPID --value psiosd)
DEBIAN_FRONTEND=noninteractive apt-get install -y "$package"
[[ $(dpkg-query -W -f='${Version}' psios-core) == "$new_version" ]]
ready
[[ $(systemctl show -p MainPID --value psiosd) != "$pid" ]]
cmp "$scratch/local.json" "$config"
runuser -u psios -- /usr/bin/psiosctl node | jq -e --arg id "$node" '.reference.resource_id == $id' >/dev/null
# Explicit socket callers can still operate a running authority while a broken
# default config is being repaired. Default callers must report the parse error.
echo '{' > "$config"
if runuser -u psios -- /usr/bin/psiosctl whoami; then exit 1; fi
ctl whoami >/dev/null
cat "$scratch/local.json" > "$config"
cmp "$scratch/token" /var/lib/psios.witness/owner.token
ctl node | jq -e --arg id "$node" '.reference.resource_id == $id and .state.configuration.worker_count == 2' >/dev/null
ctl wait "$task" | jq -e '.lifecycle == "Succeeded"' >/dev/null

[[ $(ctl get owner-delegation | jq -c .reference) == "$owner_ref" ]]
network_grant=$(jq -n --arg node "$node" '{principal_id:"local-owner",actions:["task.submit","task.read","node.interfaces","node.routes","node.routing_rules"],
    tool_targets:[$node],ttl_seconds:300,units:3}' | ctl call Grant.Issue | jq -er .delegation_ref.resource_id)
network_task=$(echo '{}' | ctl submit node.interfaces --delegation "$network_grant" --key package-network | jq -er .task_id)
ctl wait "$network_task" | jq -e '{reference: .result.artifact_refs[0]}' | ctl call Artifact.Read |
    jq -e '.interfaces | any(.ifname == "lo" and (.addr_info | any(.local == "127.0.0.1")))' >/dev/null
routes_task=$(echo '{"family":"inet","table":255}' | ctl submit node.routes --delegation "$network_grant" --key package-routes | jq -er .task_id)
ctl wait "$routes_task" | jq -e '{reference: .result.artifact_refs[0]}' | ctl call Artifact.Read |
    jq -e '.routes | any(.kind == 2 and .destination == {address:"127.0.0.1",length:32})' >/dev/null
rules_task=$(echo '{"family":"inet"}' | ctl submit node.routing_rules --delegation "$network_grant" --key package-routing-rules | jq -er .task_id)
ctl wait "$rules_task" | jq -e '{reference: .result.artifact_refs[0]}' | ctl call Artifact.Read |
    jq -e '.rules | any(.priority == 0 and .table == 255 and .action == 1)' >/dev/null

systemctl stop psiosd
runuser -u psios -- psiosd --config "$config" verify |
    jq -e --arg id "$node" '.node_id == $id and .witness == "Confirmed"' >/dev/null
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
