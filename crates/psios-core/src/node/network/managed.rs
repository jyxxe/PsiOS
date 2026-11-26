//! Bounded local-only NetworkManager profile trials on an explicitly provisioned link.
mod bus;
mod link;
mod profile;

use crate::{Result, err, linux_clock::BootClock, require};
pub(crate) use bus::Identity;
use bus::{Bus, NM, NM_PATH};
pub(crate) use link::Link;
pub(crate) use profile::Profile;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    thread,
    time::Duration,
};

const DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const DEVICE_PATH: &str = "/org/freedesktop/NetworkManager/Devices/";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const ACTIVE_PATH: &str = "/org/freedesktop/NetworkManager/ActiveConnection/";
const IP6_CONFIG: &str = "org.freedesktop.NetworkManager.IP6Config";
const IP6_PATH: &str = "/org/freedesktop/NetworkManager/IP6Config/";
const CHECKPOINT: &str = "org.freedesktop.NetworkManager.Checkpoint";
const CHECKPOINT_PATH: &str = "/org/freedesktop/NetworkManager/Checkpoint/";
const MAX_CHECKPOINTS_TO_INSPECT: usize = 16;
const EFFECT_MARGIN_MS: i64 = 3_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Active {
    pub path: String,
    pub profile_uuid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub identity: Identity,
    pub link: Link,
    pub device_path: String,
    pub profiles: [Profile; 2],
    pub baseline: Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checkpoint {
    pub path: String,
    pub snapshot_digest: String,
    pub created_boottime_ms: i64,
    pub rollback_timeout_seconds: u32,
    pub deadline_boottime_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Activation {
    pub checkpoint_path: String,
    pub active: Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Observation {
    pub checkpoint_present: bool,
    pub active: Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RollbackReceipt {
    pub results: BTreeMap<String, u32>,
    pub observed: Observation,
}

impl RollbackReceipt {
    pub(crate) fn matches(&self, scope: &Snapshot) -> bool {
        self.results.len() == 1
            && self.results.get(&scope.device_path) == Some(&0)
            && !self.observed.checkpoint_present
            && self.observed.active.profile_uuid == scope.baseline.profile_uuid
            && bus::numbered_path(&json!(self.observed.active.path), ACTIVE_PATH).is_ok()
    }
}

fn mode() -> crate::Error {
    err(
        "PreconditionFailed",
        "NetworkManager device is outside the isolated static IPv4 contract",
    )
}

fn uncertain(_: crate::Error) -> crate::Error {
    err(
        "OutcomeUnknown",
        "NetworkManager did not confirm the exact trial effect",
    )
}

fn device(bus: &Bus, link: &Link) -> Result<(String, Value)> {
    let path = bus.read(
        NM_PATH,
        NM,
        "GetDeviceByIpIface",
        &["s", &link.interface],
        "o",
    )?;
    let path = bus::numbered_path(&path, DEVICE_PATH)?;
    Ok((path.to_owned(), device_details(bus, path, link)?))
}

fn device_details(bus: &Bus, path: &str, link: &Link) -> Result<Value> {
    bus::numbered_path(&json!(path), DEVICE_PATH)?;
    let details = bus.get_all(path, DEVICE)?;
    require(
        bus::field(&details, "Interface", "s")? == &link.interface
            && matches!(bus::uint(bus::field(&details, "DeviceType", "u")?)?, 1 | 20)
            && bus::field(&details, "Managed", "b")? == true
            && bus::field(&details, "Real", "b")? == true
            && bus::text(bus::field(&details, "HwAddress", "s")?)?.eq_ignore_ascii_case(&link.mac),
        "PreconditionFailed",
        "NetworkManager device does not match the kernel Ethernet link",
    )?;
    if details.get("Ports").is_some() {
        require(
            bus::field(&details, "Ports", "ao")? == &json!([]),
            "PreconditionFailed",
            "management link has ports",
        )?;
    }
    Ok(details)
}

fn inactive_ipv6(bus: &Bus, device: &Value, active: &Value) -> Result<()> {
    let path = bus::field(active, "Ip6Config", "o")?;
    require(
        path == bus::field(device, "Ip6Config", "o")?,
        "PreconditionFailed",
        "NetworkManager IPv6 status changed during sampling",
    )?;
    if path == "/" {
        return Ok(());
    }
    // NM 1.46 can project an empty configuration object for IPv6=disabled.
    let properties = bus.get_all(bus::numbered_path(path, IP6_PATH)?, IP6_CONFIG)?;
    require(
        bus::field(&properties, "AddressData", "aa{sv}")? == &json!([])
            && bus::field(&properties, "RouteData", "aa{sv}")? == &json!([])
            && bus::field(&properties, "Gateway", "s")? == "",
        "PreconditionFailed",
        "NetworkManager reported an active IPv6 configuration",
    )?;
    for (key, signature) in [
        ("Nameservers", "aay"),
        ("Domains", "as"),
        ("Searches", "as"),
        ("DnsOptions", "as"),
    ] {
        if properties.get(key).is_some() {
            require(
                bus::field(&properties, key, signature)? == &json!([]),
                "PreconditionFailed",
                "NetworkManager reported an active IPv6 resolver",
            )?;
        }
    }
    Ok(())
}

fn active(
    bus: &Bus,
    device: &str,
    details: &Value,
    link: &Link,
    profiles: &[Profile; 2],
) -> Result<Active> {
    require(
        bus::field(details, "State", "u")? == 100
            && bus::field(details, "IpInterface", "s")? == &link.interface,
        "PreconditionFailed",
        "NetworkManager link is not activated on the expected IP interface",
    )?;
    let path = bus::numbered_path(bus::field(details, "ActiveConnection", "o")?, ACTIVE_PATH)?;
    let properties = bus.get_all(path, ACTIVE)?;
    let uuid = bus::text(bus::field(&properties, "Uuid", "s")?)?;
    let profile = profiles
        .iter()
        .find(|profile| profile.uuid == uuid)
        .ok_or_else(mode)?;
    require(
        bus::field(&properties, "Connection", "o")? == &profile.path
            && bus::field(&properties, "Devices", "ao")? == &json!([device])
            && bus::field(&properties, "Type", "s")? == profile::ETHERNET
            && bus::field(&properties, "State", "u")? == 2
            && bus::field(&properties, "Default", "b")? == false
            && bus::field(&properties, "Default6", "b")? == false
            && bus::field(&properties, "Vpn", "b")? == false,
        "PreconditionFailed",
        "NetworkManager active link and profile do not match",
    )?;
    for key in ["Controller", "Master", "Dhcp4Config", "Dhcp6Config"] {
        if properties.get(key).is_some() {
            require(
                bus::field(&properties, key, "o")? == "/",
                "PreconditionFailed",
                "NetworkManager link uses an external active configuration",
            )?;
        }
    }
    inactive_ipv6(bus, details, &properties)?;
    profile::verifies_applied(bus, device, &link.interface, profile)?;
    link::verifies(link, profile.ipv4, [profiles[0].ipv4, profiles[1].ipv4])?;
    let current = device_details(bus, device, link)?;
    require(
        bus::field(&current, "ActiveConnection", "o")? == path
            && bus::field(&current, "State", "u")? == 100
            && bus::field(&current, "IpInterface", "s")? == &link.interface,
        "PreconditionFailed",
        "NetworkManager active connection changed during sampling",
    )?;
    Ok(Active {
        path: path.to_owned(),
        profile_uuid: uuid.to_owned(),
    })
}

fn compatible_profiles(profiles: &[Profile; 2]) -> Result<()> {
    require(
        profiles[0].uuid != profiles[1].uuid
            && profiles[0].path != profiles[1].path
            && profiles[0].ipv4.address != profiles[1].ipv4.address
            && profiles[0].ipv4.prefix == profiles[1].ipv4.prefix
            && profiles[0].ipv4.network() == profiles[1].ipv4.network(),
        "PreconditionFailed",
        "NetworkManager profiles must use distinct IPv4 addresses in one on-link prefix",
    )
}

pub(crate) fn snapshot(interface: &str, profiles: [&str; 2]) -> Result<Snapshot> {
    require(
        profiles[0] != profiles[1],
        "InvalidArgument",
        "two distinct NetworkManager profiles are required",
    )?;
    let bus = Bus::open()?;
    let link = link::read(interface)?;
    let (device_path, details) = device(&bus, &link)?;
    let profiles = [
        profile::read(&bus, interface, profiles[0])?,
        profile::read(&bus, interface, profiles[1])?,
    ];
    compatible_profiles(&profiles)?;
    let baseline = active(&bus, &device_path, &details, &link, &profiles)?;
    bus.verify_owner()?;
    Ok(Snapshot {
        identity: bus.identity,
        link,
        device_path,
        profiles,
        baseline,
    })
}

fn context(scope: &Snapshot) -> Result<Bus> {
    compatible_profiles(&scope.profiles)?;
    let bus = Bus::open()?;
    require(
        bus.identity == scope.identity && link::read(&scope.link.interface)? == scope.link,
        "PreconditionFailed",
        "NetworkManager or kernel link identity changed",
    )?;
    verify_profiles(&bus, scope)?;
    Ok(bus)
}

fn verify_profiles(bus: &Bus, scope: &Snapshot) -> Result<()> {
    for expected in &scope.profiles {
        require(
            profile::read(bus, &scope.link.interface, &expected.uuid)? == *expected,
            "PreconditionFailed",
            "NetworkManager profile changed",
        )?;
    }
    Ok(())
}

fn scope_digest(scope: &Snapshot) -> Result<String> {
    Ok(psios_api::sha256(&psios_api::canonical(
        &serde_json::to_value(scope)?,
    )))
}

fn boottime(bus: &Bus) -> Result<i64> {
    let clock = BootClock::sample()?;
    require(
        clock.boot_id == bus.identity.boot_id,
        "PreconditionFailed",
        "Linux boot identity changed",
    )?;
    Ok(clock.millis)
}

fn timeout_valid(seconds: u32) -> bool {
    (5..=120).contains(&seconds)
}

fn checkpoint_record(bus: &Bus, scope: &Snapshot, path: &str) -> Result<Checkpoint> {
    bus::numbered_path(&json!(path), CHECKPOINT_PATH)?;
    let info = bus.get_all(path, CHECKPOINT)?;
    require(
        bus::field(&info, "Devices", "ao")? == &json!([scope.device_path]),
        "PreconditionFailed",
        "NetworkManager checkpoint has a different device scope",
    )?;
    let timeout = bus::uint(bus::field(&info, "RollbackTimeout", "u")?)?;
    let created = bus::field(&info, "Created", "x")?
        .as_i64()
        .filter(|value| *value >= 0)
        .ok_or_else(|| {
            err(
                "DependencyUnavailable",
                "invalid NetworkManager checkpoint creation time",
            )
        })?;
    require(
        timeout_valid(timeout),
        "PreconditionFailed",
        "NetworkManager checkpoint has an unsupported timeout",
    )?;
    let deadline = created
        .checked_add(i64::from(timeout) * 1000)
        .ok_or_else(|| {
            err(
                "DependencyUnavailable",
                "NetworkManager checkpoint deadline overflowed",
            )
        })?;
    Ok(Checkpoint {
        path: path.to_owned(),
        snapshot_digest: scope_digest(scope)?,
        created_boottime_ms: created,
        rollback_timeout_seconds: timeout,
        deadline_boottime_ms: deadline,
    })
}

fn manager_paths(bus: &Bus, key: &str, prefix: &str, limit: usize) -> Result<BTreeSet<String>> {
    let manager = bus.get_all(NM_PATH, NM)?;
    let list = bus::field(&manager, key, "ao")?
        .as_array()
        .filter(|list| list.len() <= limit)
        .ok_or_else(|| {
            err(
                "DependencyUnavailable",
                "invalid NetworkManager object list",
            )
        })?;
    let mut paths = BTreeSet::new();
    for path in list {
        require(
            paths.insert(bus::numbered_path(path, prefix)?.to_owned()),
            "DependencyUnavailable",
            "duplicate NetworkManager object path",
        )?;
    }
    Ok(paths)
}

fn checkpoint_present(bus: &Bus, checkpoint: &Checkpoint) -> Result<bool> {
    bus::numbered_path(&json!(checkpoint.path), CHECKPOINT_PATH)?;
    Ok(manager_paths(bus, "Checkpoints", CHECKPOINT_PATH, 128)?.contains(&checkpoint.path))
}

fn require_checkpoint_available(bus: &Bus, device: &str, owned: Option<&str>) -> Result<()> {
    let paths = manager_paths(
        bus,
        "Checkpoints",
        CHECKPOINT_PATH,
        MAX_CHECKPOINTS_TO_INSPECT,
    )?;
    for path in paths.iter().filter(|path| Some(path.as_str()) != owned) {
        let checkpoint = bus.get_all(path, CHECKPOINT)?;
        let devices = bus::field(&checkpoint, "Devices", "ao")?
            .as_array()
            .filter(|list| (1..=128).contains(&list.len()))
            .ok_or_else(|| {
                err(
                    "DependencyUnavailable",
                    "invalid NetworkManager checkpoint device list",
                )
            })?;
        let mut unique = BTreeSet::new();
        for entry in devices {
            require(
                unique.insert(bus::numbered_path(entry, DEVICE_PATH)?),
                "DependencyUnavailable",
                "duplicate NetworkManager checkpoint device",
            )?;
        }
        require(
            !unique.contains(device),
            "Conflict",
            "the NetworkManager device already has a live checkpoint",
        )?;
    }
    require(
        manager_paths(
            bus,
            "Checkpoints",
            CHECKPOINT_PATH,
            MAX_CHECKPOINTS_TO_INSPECT,
        )? == paths,
        "Conflict",
        "NetworkManager checkpoints changed during admission",
    )
}

fn deadline(bus: &Bus, checkpoint: &Checkpoint) -> Result<()> {
    let current = boottime(bus)?;
    require(
        current >= checkpoint.created_boottime_ms
            && current.saturating_add(EFFECT_MARGIN_MS) < checkpoint.deadline_boottime_ms,
        "PreconditionFailed",
        "NetworkManager rollback deadline is too close or has expired",
    )
}

fn guarded_checkpoint(bus: &Bus, scope: &Snapshot, checkpoint: &Checkpoint) -> Result<()> {
    require(
        checkpoint_present(bus, checkpoint)?
            && checkpoint_record(bus, scope, &checkpoint.path)? == *checkpoint,
        "PreconditionFailed",
        "NetworkManager checkpoint is absent or no longer matches the bound trial",
    )?;
    deadline(bus, checkpoint)
}

/// Persist the dispatch using the supplied boot clock reading before the host call.
pub(crate) fn checkpoint(
    scope: &Snapshot,
    timeout_seconds: u32,
    before_effect: impl FnOnce(i64) -> Result<()>,
) -> Result<Checkpoint> {
    require(
        timeout_valid(timeout_seconds),
        "InvalidArgument",
        "NetworkManager rollback timeout must be 5 to 120 seconds",
    )?;
    let bus = context(scope)?;
    let details = device_details(&bus, &scope.device_path, &scope.link)?;
    require(
        active(
            &bus,
            &scope.device_path,
            &details,
            &scope.link,
            &scope.profiles,
        )? == scope.baseline,
        "PreconditionFailed",
        "NetworkManager baseline changed before checkpoint creation",
    )?;
    require_checkpoint_available(&bus, &scope.device_path, None)?;
    bus.verify_owner()?;
    let started = boottime(&bus)?;
    before_effect(started)?;
    bus.verify_owner().map_err(uncertain)?;
    let timeout = timeout_seconds.to_string();
    let path = bus
        .mutate(
            "CheckpointCreate",
            &["aouu", "1", &scope.device_path, &timeout, "0"],
            "o",
        )
        .map_err(uncertain)?
        .expect("checkpoint object path");
    let path = bus::numbered_path(&path, CHECKPOINT_PATH).map_err(uncertain)?;
    let verified = checkpoint_record(&bus, scope, path).map_err(uncertain)?;
    bus.verify_owner().map_err(uncertain)?;
    require(
        verified.rollback_timeout_seconds == timeout_seconds
            && (started..=boottime(&bus).map_err(uncertain)?)
                .contains(&verified.created_boottime_ms),
        "OutcomeUnknown",
        "NetworkManager checkpoint receipt is outside its dispatch window",
    )?;
    Ok(verified)
}

pub(crate) fn activate(
    scope: &Snapshot,
    checkpoint: &Checkpoint,
    uuid: &str,
    before_effect: impl FnOnce() -> Result<()>,
) -> Result<Activation> {
    let profile = scope
        .profiles
        .iter()
        .find(|profile| profile.uuid == uuid && profile.uuid != scope.baseline.profile_uuid)
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "activation requires the other approved NetworkManager profile",
            )
        })?;
    let bus = context(scope)?;
    let details = device_details(&bus, &scope.device_path, &scope.link)?;
    require(
        active(
            &bus,
            &scope.device_path,
            &details,
            &scope.link,
            &scope.profiles,
        )? == scope.baseline,
        "PreconditionFailed",
        "NetworkManager baseline changed before activation",
    )?;
    guarded_checkpoint(&bus, scope, checkpoint)?;
    before_effect()?;
    bus.verify_owner().map_err(uncertain)?;
    deadline(&bus, checkpoint).map_err(uncertain)?;
    let path = bus
        .mutate(
            "ActivateConnection",
            &["ooo", &profile.path, &scope.device_path, "/"],
            "o",
        )
        .map_err(uncertain)?
        .expect("activation object path");
    let path = bus::numbered_path(&path, ACTIVE_PATH).map_err(uncertain)?;
    require(
        path != scope.baseline.path,
        "OutcomeUnknown",
        "NetworkManager did not return a distinct trial activation",
    )?;
    bus.verify_owner().map_err(uncertain)?;
    Ok(Activation {
        checkpoint_path: checkpoint.path.clone(),
        active: Active {
            path: path.to_owned(),
            profile_uuid: uuid.to_owned(),
        },
    })
}

pub(crate) fn observe(scope: &Snapshot, checkpoint: &Checkpoint) -> Result<Observation> {
    let bus = context(scope)?;
    let details = device_details(&bus, &scope.device_path, &scope.link)?;
    observe_on(&bus, &details, scope, checkpoint)
}

fn observe_on(
    bus: &Bus,
    details: &Value,
    scope: &Snapshot,
    checkpoint: &Checkpoint,
) -> Result<Observation> {
    require(
        scope_digest(scope)? == checkpoint.snapshot_digest,
        "PreconditionFailed",
        "NetworkManager checkpoint is outside its trial scope",
    )?;
    let active = active(
        bus,
        &scope.device_path,
        details,
        &scope.link,
        &scope.profiles,
    )?;
    let present = checkpoint_present(bus, checkpoint)?;
    if present {
        require(
            checkpoint_record(bus, scope, &checkpoint.path)? == *checkpoint,
            "PreconditionFailed",
            "NetworkManager checkpoint changed",
        )?;
    }
    bus.verify_owner()?;
    Ok(Observation {
        checkpoint_present: present,
        active,
    })
}

fn bound_activation(
    scope: &Snapshot,
    checkpoint: &Checkpoint,
    activation: &Activation,
) -> Result<()> {
    require(
        activation.checkpoint_path == checkpoint.path
            && activation.active.profile_uuid != scope.baseline.profile_uuid
            && scope
                .profiles
                .iter()
                .any(|profile| profile.uuid == activation.active.profile_uuid),
        "PreconditionFailed",
        "NetworkManager activation is outside its trial scope",
    )?;
    bus::numbered_path(&json!(activation.active.path), ACTIVE_PATH)?;
    Ok(())
}

fn activation_ready(
    details: &Value,
    interface: &str,
    baseline: &str,
    candidate: &str,
    verify_pending: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    let path = bus::text(bus::field(details, "ActiveConnection", "o")?)?;
    let state = bus::uint(bus::field(details, "State", "u")?)?;
    let ip_interface = bus::text(bus::field(details, "IpInterface", "s")?)?;
    require(
        path == "/" || path == baseline || path == candidate,
        "OutcomeUnknown",
        "NetworkManager activated an unrelated connection during the trial",
    )?;
    let preparing = matches!(state, 40 | 50 | 70 | 80 | 90);
    if ip_interface.is_empty() && (state == 30 || (path == "/" && (preparing || state == 110))) {
        // Between activations the Device can temporarily lack an active object.
        verify_pending()?;
        return Ok(false);
    }
    // The original activation can be deactivating while the candidate prepares its IP setup.
    let transitioning = preparing || (path == baseline && state == 110);
    require(
        ip_interface == interface || (ip_interface.is_empty() && path != "/" && transitioning),
        "OutcomeUnknown",
        "NetworkManager trial IP interface changed",
    )?;
    Ok(path == candidate && state == 100 && ip_interface == interface)
}

fn pending_activation(bus: &Bus, scope: &Snapshot, activation: &Activation) -> Result<()> {
    let profile = scope
        .profiles
        .iter()
        .find(|profile| profile.uuid == activation.active.profile_uuid)
        .ok_or_else(mode)?;
    let details = bus.get_all(&activation.active.path, ACTIVE)?;
    require(
        bus::field(&details, "Uuid", "s")? == &profile.uuid
            && bus::field(&details, "Connection", "o")? == &profile.path
            && bus::field(&details, "Devices", "ao")? == &json!([scope.device_path]),
        "OutcomeUnknown",
        "NetworkManager did not confirm the exact pending trial activation",
    )?;
    match bus::uint(bus::field(&details, "State", "u")?)? {
        1 => Ok(()),
        2 => {
            // The pending object can finish between the two read-only samples.
            let device = device_details(bus, &scope.device_path, &scope.link)?;
            require(
                bus::field(&device, "State", "u")? == 100
                    && bus::field(&device, "IpInterface", "s")? == &scope.link.interface
                    && bus::field(&device, "ActiveConnection", "o")? == &activation.active.path,
                "OutcomeUnknown",
                "the issued NetworkManager activation has unstable IP identity",
            )
        }
        _ => Err(err(
            "OutcomeUnknown",
            "NetworkManager trial candidate is no longer activating",
        )),
    }
}

/// Poll only the issued activation, without repeating it.
pub(crate) fn await_candidate(
    scope: &Snapshot,
    checkpoint: &Checkpoint,
    activation: &Activation,
) -> Result<Observation> {
    bound_activation(scope, checkpoint, activation)?;
    let stop_at = BootClock::sample()?.millis.saturating_add(5_000).min(
        checkpoint
            .deadline_boottime_ms
            .saturating_sub(EFFECT_MARGIN_MS),
    );
    loop {
        let bus = context(scope)?;
        require(
            boottime(&bus)? < stop_at,
            "OutcomeUnknown",
            "NetworkManager trial activation did not stabilize in time",
        )?;
        guarded_checkpoint(&bus, scope, checkpoint)?;
        let details = device_details(&bus, &scope.device_path, &scope.link)?;
        if activation_ready(
            &details,
            &scope.link.interface,
            &scope.baseline.path,
            &activation.active.path,
            || pending_activation(&bus, scope, activation),
        )? {
            let observed = observe_on(&bus, &details, scope, checkpoint)?;
            require(
                observed.checkpoint_present && observed.active == activation.active,
                "OutcomeUnknown",
                "NetworkManager did not confirm the exact trial connection",
            )?;
            deadline(&bus, checkpoint)?;
            return Ok(observed);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// A recovered baseline must have passed the device checkpoint deadline and have no pending candidate.
pub(crate) fn observe_recovery(
    scope: &Snapshot,
    checkpoint: &Checkpoint,
    activation: Option<&Activation>,
) -> Result<Observation> {
    if let Some(activation) = activation {
        bound_activation(scope, checkpoint, activation)?;
    }
    let bus = context(scope)?;
    let details = device_details(&bus, &scope.device_path, &scope.link)?;
    let observed = observe_on(&bus, &details, scope, checkpoint)?;
    if !observed.checkpoint_present && observed.active.profile_uuid == scope.baseline.profile_uuid {
        require(
            boottime(&bus)? >= checkpoint.deadline_boottime_ms,
            "OutcomeUnknown",
            "NetworkManager checkpoint disappeared before its known deadline",
        )?;
        no_pending_candidate(&bus, scope, activation)?;
    }
    if !observed.checkpoint_present {
        let details = device_details(&bus, &scope.device_path, &scope.link)?;
        require(
            observe_on(&bus, &details, scope, checkpoint)? == observed,
            "OutcomeUnknown",
            "NetworkManager active configuration changed after checkpoint disappearance",
        )?;
    }
    Ok(observed)
}

fn no_pending_candidate(
    bus: &Bus,
    scope: &Snapshot,
    activation: Option<&Activation>,
) -> Result<()> {
    let candidate = scope
        .profiles
        .iter()
        .find(|profile| profile.uuid != scope.baseline.profile_uuid)
        .ok_or_else(mode)?;
    let connections = manager_paths(bus, "ActiveConnections", ACTIVE_PATH, 16)?;
    for path in &connections {
        let details = bus.get_all(path, ACTIVE)?;
        let selected = activation.is_some_and(|activation| path == &activation.active.path);
        let uuid = bus::text(bus::field(&details, "Uuid", "s")?)?;
        let devices = bus::field(&details, "Devices", "ao")?
            .as_array()
            .ok_or_else(|| {
                err(
                    "DependencyUnavailable",
                    "invalid NetworkManager active devices",
                )
            })?;
        if selected
            || (uuid == candidate.uuid
                && (devices.is_empty()
                    || devices.iter().any(|device| device == &scope.device_path)))
        {
            require(
                bus::field(&details, "State", "u")? == 4,
                "OutcomeUnknown",
                "the trial candidate may still become active",
            )?;
        }
    }
    require(
        manager_paths(bus, "ActiveConnections", ACTIVE_PATH, 16)? == connections,
        "OutcomeUnknown",
        "NetworkManager active connections changed during rollback verification",
    )
}

fn await_rollback(
    bus: &Bus,
    scope: &Snapshot,
    checkpoint: &Checkpoint,
    activation: &Activation,
) -> Result<Observation> {
    let stop_at = boottime(bus)?
        .saturating_add(5_000)
        .min(checkpoint.deadline_boottime_ms);
    loop {
        require(
            boottime(bus)? < stop_at,
            "OutcomeUnknown",
            "NetworkManager rollback did not stabilize in time",
        )?;
        let details = device_details(bus, &scope.device_path, &scope.link)?;
        if bus::field(&details, "State", "u")? == 100
            && bus::field(&details, "IpInterface", "s")? == scope.link.interface
        {
            let observed = observe_on(bus, &details, scope, checkpoint)?;
            if !observed.checkpoint_present
                && observed.active.profile_uuid == scope.baseline.profile_uuid
            {
                match no_pending_candidate(bus, scope, Some(activation)) {
                    Ok(()) => {
                        require_checkpoint_available(bus, &scope.device_path, None)?;
                        verify_profiles(bus, scope)?;
                        let details = device_details(bus, &scope.device_path, &scope.link)?;
                        require(
                            observe_on(bus, &details, scope, checkpoint)? == observed
                                && boottime(bus)? < stop_at,
                            "OutcomeUnknown",
                            "NetworkManager baseline changed during rollback verification",
                        )?;
                        return Ok(observed);
                    }
                    Err(error) if error.0.code == "OutcomeUnknown" => (),
                    Err(error) => return Err(error),
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn rollback(
    scope: &Snapshot,
    checkpoint: &Checkpoint,
    activation: &Activation,
    before_effect: impl FnOnce() -> Result<()>,
) -> Result<RollbackReceipt> {
    bound_activation(scope, checkpoint, activation)?;
    let bus = context(scope)?;
    let guard = || -> Result<()> {
        let details = device_details(&bus, &scope.device_path, &scope.link)?;
        require(
            active(
                &bus,
                &scope.device_path,
                &details,
                &scope.link,
                &scope.profiles,
            )? == activation.active,
            "PreconditionFailed",
            "NetworkManager trial connection changed before rollback",
        )?;
        guarded_checkpoint(&bus, scope, checkpoint)?;
        // Rolling back an older checkpoint can discard a newer overlapping one.
        require_checkpoint_available(&bus, &scope.device_path, Some(&checkpoint.path))?;
        bus.verify_owner()?;
        deadline(&bus, checkpoint)
    };
    guard()?;
    before_effect()?;
    verify_profiles(&bus, scope).map_err(uncertain)?;
    guard().map_err(uncertain)?;
    let reply = bus
        .mutate("CheckpointRollback", &["o", &checkpoint.path], "a{su}")
        .map_err(uncertain)?
        .ok_or_else(|| {
            err(
                "OutcomeUnknown",
                "NetworkManager rollback acknowledgement missing",
            )
        })?;
    let results: BTreeMap<String, u32> = crate::decode(&reply).map_err(uncertain)?;
    require(
        results.len() == 1 && results.get(&scope.device_path) == Some(&0),
        "OutcomeUnknown",
        "NetworkManager rollback did not acknowledge the exact trial device",
    )?;
    let observed = await_rollback(&bus, scope, checkpoint, activation).map_err(uncertain)?;
    Ok(RollbackReceipt { results, observed })
}

pub(crate) fn commit(
    scope: &Snapshot,
    checkpoint: &Checkpoint,
    activation: &Activation,
    before_effect: impl FnOnce() -> Result<()>,
) -> Result<Observation> {
    bound_activation(scope, checkpoint, activation)?;
    let bus = context(scope)?;
    let details = device_details(&bus, &scope.device_path, &scope.link)?;
    require(
        active(
            &bus,
            &scope.device_path,
            &details,
            &scope.link,
            &scope.profiles,
        )? == activation.active,
        "PreconditionFailed",
        "NetworkManager trial connection changed before commit",
    )?;
    guarded_checkpoint(&bus, scope, checkpoint)?;
    before_effect()?;
    bus.verify_owner().map_err(uncertain)?;
    deadline(&bus, checkpoint).map_err(uncertain)?;
    bus.mutate("CheckpointDestroy", &["o", &checkpoint.path], "")
        .map_err(uncertain)?;
    let observed = observe(scope, checkpoint).map_err(uncertain)?;
    require(
        !observed.checkpoint_present && observed.active == activation.active,
        "OutcomeUnknown",
        "NetworkManager did not confirm the committed trial state",
    )?;
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, fs, os::unix::fs::MetadataExt};

    #[test]
    fn activation_poll_only_tolerates_empty_ip_on_the_bound_transition() {
        let baseline = "/org/freedesktop/NetworkManager/ActiveConnection/1";
        let candidate = "/org/freedesktop/NetworkManager/ActiveConnection/2";
        let unrelated = "/org/freedesktop/NetworkManager/ActiveConnection/3";
        let interface = "trial0";
        let details = |path, state, ip| {
            json!({
                "ActiveConnection":{"type":"o","data":path},
                "State":{"type":"u","data":state},
                "IpInterface":{"type":"s","data":ip},
            })
        };
        let check = |path, state, ip| {
            activation_ready(
                &details(path, state, ip),
                interface,
                baseline,
                candidate,
                || panic!("disconnected proof was unexpected"),
            )
        };
        assert!(!check(baseline, 110, "").unwrap());
        assert!(!check(candidate, 70, "").unwrap());
        assert!(check(candidate, 100, interface).unwrap());
        assert!(check(candidate, 100, "").is_err());
        assert!(check(candidate, 70, "other0").is_err());
        assert!(check(unrelated, 70, "").is_err());
        let gap = details("/", 30, "");
        assert!(!activation_ready(&gap, interface, baseline, candidate, || Ok(())).unwrap());
        assert!(
            activation_ready(&gap, interface, baseline, candidate, || Err(err(
                "OutcomeUnknown",
                "candidate was not pending"
            )))
            .is_err()
        );
    }

    #[test]
    #[ignore = "requires tests/network_manager_trial.py and its private NetworkManager namespaces"]
    fn real_network_manager_trial() -> Result<()> {
        let interface = "psios-nm0";
        let original = "1aad7685-9a7d-49ad-8132-0c9c56cfb34e";
        let candidate = "597da7b3-c8ba-4134-bfa7-57377f1ab670";
        assert_eq!(
            env::var("PSIOS_NETWORK_MANAGER_ADAPTER_FIXTURE").as_deref(),
            Ok("1")
        );
        assert!(rustix::process::geteuid().is_root());
        for kind in ["net", "mnt", "uts"] {
            let local = fs::metadata(format!("/proc/self/ns/{kind}"))?;
            let initial = fs::metadata(format!("/proc/1/ns/{kind}"))?;
            assert_ne!((local.dev(), local.ino()), (initial.dev(), initial.ino()));
        }
        let interfaces = super::super::kernel::observe_interfaces()?;
        let names: BTreeSet<_> = interfaces.iter().map(|link| link.ifname.as_str()).collect();
        assert_eq!(names, BTreeSet::from(["lo", interface]));

        let scope = snapshot(interface, [original, candidate])?;
        assert_eq!(scope.link.mac, "02:50:53:00:00:01");
        assert_eq!(scope.baseline.profile_uuid, original);
        let rejected = checkpoint(&scope, 12, |_| {
            Err(err("Fenced", "fixture refused dispatch"))
        });
        assert_eq!(rejected.unwrap_err().0.code, "Fenced");
        assert!(manager_paths(&Bus::open()?, "Checkpoints", CHECKPOINT_PATH, 128)?.is_empty());

        let trial = checkpoint(&scope, 12, |_| Ok(()))?;
        let overlap = checkpoint(&scope, 12, |_| {
            panic!("overlapping checkpoint reached dispatch")
        });
        assert_eq!(overlap.unwrap_err().0.code, "Conflict");
        assert!(checkpoint_present(&Bus::open()?, &trial)?);
        let activation = activate(&scope, &trial, candidate, || Ok(()))?;
        let observed = await_candidate(&scope, &trial, &activation)?;
        assert_eq!(observed.active, activation.active);
        assert!(observed.checkpoint_present);
        let wait_until = trial.deadline_boottime_ms + 8_000;
        loop {
            if BootClock::sample()?.millis >= trial.deadline_boottime_ms
                && let Ok(restored) = observe_recovery(&scope, &trial, Some(&activation))
                && !restored.checkpoint_present
                && restored.active.profile_uuid == original
            {
                break;
            }
            assert!(
                BootClock::sample()?.millis < wait_until,
                "device rollback did not restore exact original profile"
            );
            thread::sleep(Duration::from_millis(200));
        }

        let scope = snapshot(interface, [original, candidate])?;
        let trial = checkpoint(&scope, 12, |_| Ok(()))?;
        let activation = activate(&scope, &trial, candidate, || Ok(()))?;
        await_candidate(&scope, &trial, &activation)?;
        let mut invalid = trial.clone();
        invalid.path = "/".into();
        let mut invalid_activation = activation.clone();
        invalid_activation.checkpoint_path = "/".into();
        assert!(
            commit(&scope, &invalid, &invalid_activation, || panic!(
                "empty-path effect was admitted"
            ))
            .is_err()
        );
        let observed = commit(&scope, &trial, &activation, || Ok(()))?;
        assert_eq!(observed.active, activation.active);
        assert!(!observed.checkpoint_present);
        Ok(())
    }
}
