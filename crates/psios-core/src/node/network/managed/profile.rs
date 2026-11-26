//! Only a pre-existing, file-backed Ethernet profile with one static IPv4 address is eligible.
use super::bus::{self, Bus};
use crate::{Result, err, require};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, net::Ipv4Addr, os::unix::fs::MetadataExt, path::Path};
use uuid::Uuid;

const SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const CONNECTION: &str = "org.freedesktop.NetworkManager.Settings.Connection";
pub(super) const PROFILE_PATH: &str = "/org/freedesktop/NetworkManager/Settings/";
const PROFILE_DIR: &str = "/etc/NetworkManager/system-connections";
pub(super) const ETHERNET: &str = "802-3-ethernet";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ipv4 {
    pub address: Ipv4Addr,
    pub prefix: u8,
}

impl Ipv4 {
    pub(crate) fn network(self) -> Ipv4Addr {
        let mask = u32::MAX
            .checked_shl(u32::from(32u8.saturating_sub(self.prefix)))
            .unwrap_or(0);
        Ipv4Addr::from(u32::from(self.address) & mask)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub uuid: String,
    pub path: String,
    pub ipv4: Ipv4,
    pub settings_digest: String,
}

fn unsupported() -> crate::Error {
    err(
        "UnsupportedCapability",
        "NetworkManager profile is outside the static isolated Ethernet contract",
    )
}

fn only(value: &Value, allowed: &[&str]) -> Result<()> {
    require(
        value
            .as_object()
            .is_some_and(|map| map.keys().all(|key| allowed.contains(&key.as_str()))),
        "UnsupportedCapability",
        "NetworkManager profile includes unsupported settings",
    )
}

fn must(map: &Value, name: &str, signature: &str, expected: &Value) -> Result<()> {
    require(
        bus::field(map, name, signature)? == expected,
        "UnsupportedCapability",
        "NetworkManager profile violates a required setting",
    )
}

fn optional(map: &Value, name: &str, signature: &str, expected: &Value) -> Result<()> {
    if map.get(name).is_some() {
        must(map, name, signature, expected)?;
    }
    Ok(())
}

fn valid_ipv4(settings: &Value) -> Result<Ipv4> {
    only(
        settings,
        &[
            "method",
            "address-data",
            "addresses",
            "ignore-auto-dns",
            "ignore-auto-routes",
            "never-default",
            "route-table",
            "dns",
            "dns-data",
            "dns-search",
            "dns-options",
            "gateway",
            "routes",
            "route-data",
            "routing-rules",
            "may-fail",
        ],
    )?;
    must(settings, "method", "s", &json!("manual"))?;
    must(settings, "never-default", "b", &json!(true))?;
    must(settings, "ignore-auto-dns", "b", &json!(true))?;
    must(settings, "route-table", "u", &json!(254))?;
    optional(settings, "ignore-auto-routes", "b", &json!(true))?;
    optional(settings, "may-fail", "b", &json!(true))?;
    for (name, signature) in [
        ("dns", "au"),
        ("dns-data", "as"),
        ("dns-search", "as"),
        ("dns-options", "as"),
        ("routes", "aau"),
        ("route-data", "aa{sv}"),
        ("routing-rules", "aa{sv}"),
    ] {
        optional(settings, name, signature, &json!([]))?;
    }
    optional(settings, "gateway", "s", &json!(""))?;
    let address = bus::field(settings, "address-data", "aa{sv}")?
        .as_array()
        .filter(|values| values.len() == 1)
        .and_then(|values| values.first())
        .ok_or_else(unsupported)?;
    only(address, &["address", "prefix"])?;
    let ip = bus::text(bus::field(address, "address", "s")?)?;
    let parsed: Ipv4Addr = ip.parse().map_err(|_| unsupported())?;
    let prefix = bus::uint(bus::field(address, "prefix", "u")?)?;
    require(
        (1..=30).contains(&prefix)
            && (1..224).contains(&parsed.octets()[0])
            && !parsed.is_loopback()
            && !parsed.is_link_local()
            && parsed.to_string() == ip,
        "UnsupportedCapability",
        "NetworkManager profile requires one canonical unicast IPv4 host address",
    )?;
    let address = Ipv4 {
        address: parsed,
        prefix: prefix as u8,
    };
    let ip = u32::from(parsed);
    let network = u32::from(address.network());
    let broadcast = network | (u32::MAX >> prefix);
    require(
        ip != network && ip != broadcast,
        "UnsupportedCapability",
        "NetworkManager profile specifies an IPv4 network or broadcast address",
    )?;
    if settings.get("addresses").is_some() {
        let legacy = bus::field(settings, "addresses", "aau")?;
        require(
            legacy == &json!([])
                || legacy == &json!([[u32::from_ne_bytes(parsed.octets()), prefix, 0]]),
            "UnsupportedCapability",
            "legacy NetworkManager IPv4 address data disagrees",
        )?;
    }
    Ok(address)
}

fn decoder(settings: &Value, interface: &str, id: &str) -> Result<Ipv4> {
    only(settings, &["connection", ETHERNET, "ipv4", "ipv6", "proxy"])?;
    let connection = &settings["connection"];
    only(
        connection,
        &[
            "id",
            "uuid",
            "type",
            "interface-name",
            "autoconnect",
            "permissions",
            "timestamp",
            "mdns",
            "llmnr",
            "autoconnect-priority",
        ],
    )?;
    must(connection, "uuid", "s", &json!(id))?;
    must(connection, "interface-name", "s", &json!(interface))?;
    must(connection, "type", "s", &json!(ETHERNET))?;
    must(connection, "autoconnect", "b", &json!(false))?;
    must(connection, "mdns", "i", &json!(0))?;
    must(connection, "llmnr", "i", &json!(0))?;
    optional(connection, "permissions", "as", &json!([]))?;
    optional(connection, "autoconnect-priority", "i", &json!(0))?;
    let name = bus::text(bus::field(connection, "id", "s")?)?;
    require(
        !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control),
        "UnsupportedCapability",
        "NetworkManager profile has an unsafe name",
    )?;
    if connection.get("timestamp").is_some() {
        require(
            bus::field(connection, "timestamp", "t")?.is_u64(),
            "DependencyUnavailable",
            "invalid NetworkManager profile timestamp",
        )?;
    }
    if let Some(wired) = settings.get(ETHERNET) {
        only(
            wired,
            &[
                "auto-negotiate",
                "mac-address-blacklist",
                "mac-address-denylist",
                "s390-options",
            ],
        )?;
        optional(wired, "auto-negotiate", "b", &json!(false))?;
        optional(wired, "mac-address-blacklist", "as", &json!([]))?;
        optional(wired, "mac-address-denylist", "as", &json!([]))?;
        optional(wired, "s390-options", "a{ss}", &json!({}))?;
    }
    if let Some(proxy) = settings.get("proxy") {
        only(proxy, &["method", "browser-only"])?;
        optional(proxy, "method", "i", &json!(0))?;
        optional(proxy, "browser-only", "b", &json!(false))?;
    }
    let ipv6 = &settings["ipv6"];
    only(
        ipv6,
        &[
            "method",
            "addr-gen-mode",
            "addresses",
            "address-data",
            "routes",
            "route-data",
            "dns",
            "dns-data",
            "dns-search",
        ],
    )?;
    must(ipv6, "method", "s", &json!("disabled"))?;
    if ipv6.get("addr-gen-mode").is_some() {
        require(
            bus::field(ipv6, "addr-gen-mode", "i")?
                .as_i64()
                .is_some_and(|value| (0..=3).contains(&value)),
            "UnsupportedCapability",
            "invalid NetworkManager IPv6 mode",
        )?;
    }
    for (name, signature) in [
        ("addresses", "a(ayuay)"),
        ("address-data", "aa{sv}"),
        ("routes", "a(ayuayu)"),
        ("route-data", "aa{sv}"),
        ("dns", "aay"),
        ("dns-data", "as"),
        ("dns-search", "as"),
    ] {
        optional(ipv6, name, signature, &json!([]))?;
    }
    valid_ipv4(&settings["ipv4"])
}

fn root_keyfile(filename: &str) -> Result<()> {
    let path = Path::new(filename);
    let rejected = || {
        err(
            "UnsupportedCapability",
            "NetworkManager profile must be a private root keyfile",
        )
    };
    require(
        path.parent() == Some(Path::new(PROFILE_DIR))
            && path.extension().is_some_and(|ext| ext == "nmconnection")
            && fs::canonicalize(path).is_ok_and(|canonical| canonical == path),
        "UnsupportedCapability",
        "NetworkManager profile is not a system keyfile",
    )?;
    let metadata = fs::symlink_metadata(path).map_err(|_| rejected())?;
    require(
        metadata.is_file()
            && metadata.uid() == 0
            && metadata.gid() == 0
            && metadata.nlink() == 1
            && matches!(metadata.mode() & 0o7777, 0o400 | 0o600),
        "UnsupportedCapability",
        "NetworkManager profile must be a private root keyfile",
    )?;
    for directory in ["/etc/NetworkManager", PROFILE_DIR] {
        let metadata = fs::symlink_metadata(directory).map_err(|_| rejected())?;
        require(
            metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
            "UnsupportedCapability",
            "NetworkManager profile parent is not root-controlled",
        )?;
    }
    Ok(())
}

pub(super) fn read(bus: &Bus, interface: &str, uuid: &str) -> Result<Profile> {
    require(
        Uuid::parse_str(uuid).is_ok_and(|value| !value.is_nil() && value.to_string() == uuid),
        "InvalidArgument",
        "invalid canonical NetworkManager profile UUID",
    )?;
    let path = bus.read(
        SETTINGS_PATH,
        SETTINGS,
        "GetConnectionByUuid",
        &["s", uuid],
        "o",
    )?;
    let path = bus::numbered_path(&path, PROFILE_PATH)?;
    let property = bus.get_all(path, CONNECTION)?;
    require(
        bus::field(&property, "Unsaved", "b")? == false
            && bus::field(&property, "Flags", "u")? == 0,
        "UnsupportedCapability",
        "NetworkManager profile is not an unchanged persistent profile",
    )?;
    let filename = bus::text(bus::field(&property, "Filename", "s")?)?;
    root_keyfile(filename)?;
    let mut settings = bus.read(path, CONNECTION, "GetSettings", &[], "a{sa{sv}}")?;
    let ipv4 = decoder(&settings, interface, uuid)?;
    settings["connection"]
        .as_object_mut()
        .expect("validated settings")
        .remove("timestamp");
    let settings_digest = psios_api::sha256(&psios_api::canonical(&settings));
    Ok(Profile {
        uuid: uuid.to_owned(),
        path: path.to_owned(),
        ipv4,
        settings_digest,
    })
}

pub(super) fn verifies_applied(
    bus: &Bus,
    device: &str,
    interface: &str,
    profile: &Profile,
) -> Result<()> {
    let settings = bus.applied(device)?;
    // Applied settings can omit file metadata and the activation timestamp.
    require(
        decoder(&settings, interface, &profile.uuid)? == profile.ipv4,
        "PreconditionFailed",
        "active NetworkManager settings differ from the approved static profile",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_manual_profile_excludes_profile_writes_and_route_widening() {
        let v = |signature, data| json!({"type":signature,"data":data});
        let uuid = "1aad7685-9a7d-49ad-8132-0c9c56cfb34e";
        let mut settings = json!({
            "connection": { "id":v("s",json!("psios-original")), "uuid":v("s",json!(uuid)),
                "type":v("s",json!(ETHERNET)), "interface-name":v("s",json!("psios-nm0")),
                "autoconnect":v("b",json!(false)), "mdns":v("i",json!(0)), "llmnr":v("i",json!(0)) },
            (ETHERNET):{}, "proxy":{},
            "ipv4": { "method":v("s",json!("manual")), "never-default":v("b",json!(true)),
                "ignore-auto-dns":v("b",json!(true)), "route-table":v("u",json!(254)),
                "address-data":v("aa{sv}",json!([{"address":v("s",json!("192.0.2.10")),"prefix":v("u",json!(24))}])) },
            "ipv6": { "method":v("s",json!("disabled")) },
        });
        let ipv4 = decoder(&settings, "psios-nm0", uuid).unwrap();
        assert_eq!(ipv4.address, Ipv4Addr::new(192, 0, 2, 10));
        assert_eq!(ipv4.network(), Ipv4Addr::new(192, 0, 2, 0));
        assert!(decoder(&settings, "different0", uuid).is_err());
        for (group, setting, value) in [
            ("connection", "autoconnect", v("b", json!(true))),
            ("connection", "controller", v("s", json!("elsewhere"))),
            ("ipv4", "route-table", v("u", json!(42))),
            ("ipv4", "gateway", v("s", json!("192.0.2.1"))),
            ("ipv4", "dns-data", v("as", json!(["192.0.2.53"]))),
            ("ipv6", "method", v("s", json!("auto"))),
        ] {
            let mut changed = settings.clone();
            changed[group][setting] = value;
            assert!(
                decoder(&changed, "psios-nm0", uuid).is_err(),
                "{group}.{setting}"
            );
        }
        settings["ipv4"]["addresses"] = v("aau", json!([[0, 24, 0]]));
        assert!(decoder(&settings, "psios-nm0", uuid).is_err());
    }
}
