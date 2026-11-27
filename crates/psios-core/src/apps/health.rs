//! OCI health checks inherited only from an explicitly approved local image.
use super::*;
use crate::db::{resolve, row};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Serialize;
mod admission;
pub(crate) use admission::migrate;
pub(super) use admission::record;

pub(crate) const FORMAT: u32 = crate::events::scopes::FORMAT + 1;
pub(crate) const STARTUP_FORMAT: u32 = host::READ_PUBLICATION_FORMAT + 1;
const DEFAULT: &str = "oci-health";
const KIND: &str = "OCIImageHealthCheck";
const PROFILE: &str = "DockerImageExecV1";
const NS_PER_MS: u64 = 1_000_000;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Probe {
    argv: Vec<String>,
    interval_ms: u32,
    timeout_ms: u32,
    retries: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_period_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_interval_ms: Option<u32>,
}

impl Probe {
    pub(super) fn validate(&self) -> Result<()> {
        require(
            (1..=16).contains(&self.argv.len())
                && self.argv[0].starts_with('/')
                && self.argv[0]
                    .split('/')
                    .skip(1)
                    .all(|part| !matches!(part, "" | "." | ".."))
                && self
                    .argv
                    .iter()
                    .all(|part| part.len() <= 512 && !part.contains('\0'))
                && self.argv.iter().map(String::len).sum::<usize>() <= 2048
                && (1_000..=60_000).contains(&self.interval_ms)
                && (100..=5_000).contains(&self.timeout_ms)
                && self.timeout_ms <= self.interval_ms
                && (1..=5).contains(&self.retries),
            "InvalidArgument",
            "OCI probe requires an absolute executable, bounded argv, interval, timeout and retries",
        )?;
        let startup_allowed = match (self.start_period_ms, self.start_interval_ms) {
            (None, None) => true,
            (Some(period), Some(interval)) => {
                period <= 120_000
                    && (1_000..=60_000).contains(&interval)
                    && self.timeout_ms <= interval
                    && interval < period
            }
            _ => false,
        };
        require(
            startup_allowed,
            "InvalidArgument",
            "OCI startup health requires a grace period and a bounded startup interval shorter than that period",
        )
    }

    fn matches_config(&self, value: &Value) -> bool {
        if !value.as_object().is_some_and(|fields| {
            fields.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "Test" | "Interval" | "Timeout" | "Retries" | "StartPeriod" | "StartInterval"
                )
            })
        }) {
            return false;
        }
        let Some(test) = value["Test"].as_array() else {
            return false;
        };
        let Some((mode, argv)) = test.split_first() else {
            return false;
        };
        mode == "CMD"
            && argv.len() == self.argv.len()
            && argv
                .iter()
                .zip(&self.argv)
                .all(|(accepted, expected)| accepted == expected)
            && value["Interval"].as_u64() == Some(u64::from(self.interval_ms) * NS_PER_MS)
            && value["Timeout"].as_u64() == Some(u64::from(self.timeout_ms) * NS_PER_MS)
            && value["Retries"].as_u64() == Some(u64::from(self.retries))
            && [
                ("StartPeriod", self.start_period_ms),
                ("StartInterval", self.start_interval_ms),
            ]
            .into_iter()
            .all(|(key, duration)| match duration {
                Some(ms) => value[key].as_u64() == Some(u64::from(ms) * NS_PER_MS),
                None => value.get(key).is_none_or(|value| value.as_u64() == Some(0)),
            })
    }

    pub(super) fn require_image(&self, image: &Value) -> Result<()> {
        require(
            self.matches_config(&image["Config"]["Healthcheck"]),
            "UnsupportedCapability",
            "local image does not contain the exact approved bounded exec-form health check",
        )
    }
}

pub(super) fn verify_runtime(expected: Option<&Probe>, container: &Value) -> Result<()> {
    let config = &container["Config"]["Healthcheck"];
    let matches = if let Some(expected) = expected {
        let host = &container["HostConfig"];
        let uid = rustix::process::geteuid().as_raw();
        let gid = rustix::process::getegid().as_raw();
        let bindings = &host["PortBindings"];
        let networks = &container["NetworkSettings"]["Networks"];
        expected.matches_config(config)
            && uid != 0
            && container["Config"]["User"] == format!("{uid}:{gid}")
            && container["Config"]["WorkingDir"] == "/"
            && host["NetworkMode"] == "none"
            && host["Privileged"] == false
            && host["ReadonlyRootfs"] == true
            && host["PublishAllPorts"] == false
            && (bindings.is_null() || bindings.as_object().is_some_and(serde_json::Map::is_empty))
            && (networks.is_null()
                || networks
                    .as_object()
                    .is_some_and(|all| all.keys().all(|name| name == "none")))
    } else {
        config.is_null()
            || config["Test"]
                .as_array()
                .is_some_and(|test| test.is_empty() || test == &[json!("NONE")])
    };
    require(
        matches,
        "PreconditionFailed",
        "OCI effective health configuration differs from the authorized release",
    )
}

pub(super) fn pin(
    db: &Db<'_>,
    release_spec: &Doc,
    image: &Doc,
    expected: Option<&Probe>,
) -> Result<Value> {
    let Some(expected) = expected else {
        return Ok(db.get(DEFAULT)?.reference());
    };
    require(
        image.body["inspection"]["config_digest"] == image.body["config_digest"]
            && image.body["inspection"]["health_check"] == json!(expected),
        "RecoveryRequired",
        "OCI image admission did not verify the approved health check",
    )?;
    Ok(db.create(KIND,&format!("oci-health-{}",release_spec.id),&release_spec.owner,json!({}),json!({
        "profile":PROFILE,"release_spec_ref":release_spec.reference(),"image_ref":image.reference(),
        "config_digest":image.body["config_digest"],"probe":expected,
    }))?.reference())
}

fn profile(conn: &Connection, release: &Doc, spec: &Doc, image: &Doc) -> Result<Option<Probe>> {
    let profile = resolve(conn, &release.body["health_check_ref"])?;
    if profile.id == DEFAULT {
        require(
            profile.kind == "HealthCheck"
                && profile.rev == 1
                && image.body["inspection"].get("health_check").is_none(),
            "RecoveryRequired",
            "disabled OCI health policy disagrees with the image admission",
        )?;
        return Ok(None);
    }
    let probe: Probe = decode(&profile.body["probe"])?;
    probe.validate()?;
    require(
        profile.id == format!("oci-health-{}", spec.id)
            && profile.kind == KIND
            && profile.rev == 1
            && profile.owner == release.owner
            && profile.body["profile"] == PROFILE
            && profile.body["release_spec_ref"] == spec.reference()
            && profile.body["image_ref"] == image.reference()
            && profile.body["config_digest"] == image.body["config_digest"]
            && image.kind == "OCIImage"
            && spec.kind == "ApplicationReleaseSpec"
            && image.body["inspection"]["config_digest"] == image.body["config_digest"]
            && image.body["inspection"]["health_check"] == json!(probe),
        "RecoveryRequired",
        "OCI health check profile disagrees with its release or admitted image",
    )?;
    Ok(Some(probe))
}

fn approved(
    conn: &Connection,
    release: &Doc,
    spec: &Doc,
    image: &Doc,
) -> Result<(Option<Probe>, Option<Doc>)> {
    let probe = profile(conn, release, spec, image)?;
    if admission::modern(conn, release)? {
        let source = admission::approved_source(conn, release, spec, image, probe.as_ref())?;
        return Ok((probe, source));
    }
    require(
        probe.is_none() && image.body.get("health_admission").is_none(),
        "RecoveryRequired",
        "legacy OCI release claims an unsupported health profile",
    )?;
    Ok((None, None))
}

pub(super) fn release(
    conn: &Connection,
    release: &Doc,
    spec: &Doc,
    image: &Doc,
) -> Result<Option<Probe>> {
    Ok(approved(conn, release, spec, image)?.0)
}

pub(super) fn check_retained_choice(
    conn: &Connection,
    release: &Doc,
    descriptor: &artifact_install::Descriptor,
) -> Result<()> {
    let spec = resolve(conn, &release.body["release_spec_ref"])?;
    let image = resolve(conn, &release.body["artifact_ref"])?;
    let probe = self::release(conn, release, &spec, &image)?;
    admission::check_descriptor(probe.as_ref(), &image, descriptor)
}

pub(crate) fn selected(db: &Db<'_>, app: &Doc) -> Result<Option<Probe>> {
    let selected = db.resolve(&app.spec["release_ref"])?;
    let spec = db.resolve(&selected.body["release_spec_ref"])?;
    let image = db.resolve(&selected.body["artifact_ref"])?;
    require(
        selected.kind == "ApplicationRelease"
            && selected.body["application_id"] == app.id
            && image.body["config_digest"] == app.spec["image_id"],
        "RecoveryRequired",
        "OCI health observation is outside the selected image",
    )?;
    release(db.c, &selected, &spec, &image)
}

pub(super) fn status(report: &mut Value, expected: Option<&Probe>, state: &Value) {
    let (health, readiness) = classify(expected, state, Utc::now());
    report["health"] = json!(health);
    report["readiness"] = json!(readiness);
    if expected.is_some() {
        report["health_probe_profile"] = json!(PROFILE);
    }
}

pub(super) fn projection(observation: &Value, recovered: bool) -> (&str, &str) {
    let running = observation["process_running"] == true;
    if recovered && observation["health_probe_profile"] == PROFILE {
        return ("Unknown", if running { "Unknown" } else { "NotReady" });
    }
    (
        observation["health"].as_str().unwrap_or("Unknown"),
        observation["readiness"]
            .as_str()
            .unwrap_or(if running { "Unknown" } else { "NotReady" }),
    )
}

fn classify(
    expected: Option<&Probe>,
    state: &Value,
    now: DateTime<Utc>,
) -> (&'static str, &'static str) {
    if state["Running"] != true || state["Status"] != "running" {
        return ("Unknown", "NotReady");
    }
    let Some(expected) = expected else {
        return ("Unknown", "Unknown");
    };
    let health = &state["Health"];
    if health["Status"] == "starting" {
        return ("Unknown", "NotReady");
    }
    let stamp = |field: &Value| {
        field
            .as_str()
            .filter(|text| text.len() <= 40)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            .map(|time| time.to_utc())
    };
    let last = health["Log"].as_array().and_then(|items| items.last());
    let fresh = last.is_some_and(|last| {
        let (Some(invocation), Some(start), Some(end)) = (
            stamp(&state["StartedAt"]),
            stamp(&last["Start"]),
            stamp(&last["End"]),
        ) else {
            return false;
        };
        let limit = chrono::Duration::milliseconds(i64::from(
            expected.interval_ms + expected.timeout_ms + 35_000,
        ));
        invocation <= start && start <= end && end <= now && now - end <= limit
    });
    if !fresh {
        return ("Unknown", "Unknown");
    }
    match health["Status"].as_str() {
        Some("healthy") => ("Healthy", "Ready"),
        Some("unhealthy")
            if health["FailingStreak"]
                .as_u64()
                .is_some_and(|failures| failures >= u64::from(expected.retries)) =>
        {
            ("Unhealthy", "NotReady")
        }
        _ => ("Unknown", "Unknown"),
    }
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut count = 0i64;
    let mut assigned = 0i64;
    let epoch = admission::epoch(conn)?;
    let mut releases = conn.prepare(
        "SELECT * FROM versions WHERE kind='ApplicationRelease' AND rev=1
        AND (seq>?1 OR body->'health_check_ref'->>'resource_id'<>?2)",
    )?;
    for release_doc in releases.query_map(params![epoch, DEFAULT], row)? {
        let release_doc = release_doc?;
        let spec = resolve(conn, &release_doc.body["release_spec_ref"])?;
        let image = resolve(conn, &release_doc.body["artifact_ref"])?;
        let (probe, source) = approved(conn, &release_doc, &spec, &image)?;
        if let Some(source) = source {
            admission::verify_retained(conn, &release_doc, &image, probe.as_ref(), &source)?;
        }
        assigned += i64::from(probe.is_some());
        count += 1;
    }
    let (profiles, images): (i64, i64) = conn.query_row(
        "SELECT count(*) FILTER (WHERE kind=?1),count(*) FILTER (WHERE kind='OCIImage' AND body->>'health_admission' IS NOT NULL)
         FROM versions WHERE kind IN (?1,'OCIImage')",[KIND],|r|Ok((r.get(0)?,r.get(1)?)))?;
    require(
        profiles == assigned && images == count,
        "RecoveryRequired",
        "OCI health journal has missing or orphaned release bindings",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Probe {
        Probe {
            argv: vec!["/probe".into(), "--ready".into()],
            interval_ms: 1000,
            timeout_ms: 200,
            retries: 2,
            start_period_ms: None,
            start_interval_ms: None,
        }
    }

    #[test]
    fn oci_health_requires_exact_exec_form_and_explicit_image_timings() {
        let probe = profile();
        probe.validate().unwrap();
        let accepted = json!({"Test":["CMD","/probe","--ready"],"Interval":1_000_000_000,"Timeout":200_000_000,"Retries":2});
        assert!(probe.matches_config(&accepted));
        for (field, value) in [
            ("Test", json!(["CMD-SHELL", "/probe --ready"])),
            ("Test", json!(["CMD", "/probe", "--different"])),
            ("Interval", json!(0)),
            ("Timeout", json!(0)),
            ("Retries", json!(0)),
            ("StartPeriod", json!(1)),
            ("StartInterval", json!(1)),
            ("UnknownField", json!(0)),
        ] {
            let mut changed = accepted.clone();
            changed[field] = value;
            assert!(!probe.matches_config(&changed), "{field}");
        }
        let mut unbounded = probe;
        unbounded.timeout_ms = 2000;
        assert_eq!(unbounded.validate().unwrap_err().0.code, "InvalidArgument");
    }

    #[test]
    fn startup_health_requires_exact_explicit_engine_timings() {
        let mut probe = profile();
        assert!(json!(probe).get("start_period_ms").is_none());
        assert!(json!(probe).get("start_interval_ms").is_none());
        probe.start_period_ms = Some(12_000);
        assert_eq!(probe.validate().unwrap_err().0.code, "InvalidArgument");
        probe.start_interval_ms = Some(1_000);
        probe.validate().unwrap();
        let engine = json!({"Test":["CMD","/probe","--ready"],"Interval":1_000_000_000,
            "Timeout":200_000_000,"Retries":2,"StartPeriod":12_000_000_000u64,"StartInterval":1_000_000_000});
        assert!(probe.matches_config(&engine));
        assert!(!profile().matches_config(&engine));
        for field in ["StartPeriod", "StartInterval"] {
            let mut missing = engine.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(!probe.matches_config(&missing), "{field}");
        }
        for (period, interval) in [
            (120_001, 1_000),
            (2_000, 999),
            (1_000, 1_000),
            (65_000, 60_001),
        ] {
            probe.start_period_ms = Some(period);
            probe.start_interval_ms = Some(interval);
            assert_eq!(probe.validate().unwrap_err().0.code, "InvalidArgument");
        }
        probe.start_period_ms = Some(12_000);
        probe.start_interval_ms = Some(1_000);
        probe.interval_ms = 2_000;
        probe.timeout_ms = 1_500;
        assert_eq!(probe.validate().unwrap_err().0.code, "InvalidArgument");
    }

    #[test]
    fn oci_health_does_not_trust_a_stale_invocation_or_return_probe_output() {
        let probe = profile();
        let mut state = json!({"Running":true,"Status":"running","StartedAt":"2026-11-25T12:00:00Z",
            "Health":{"Status":"healthy","FailingStreak":0,"Log":[{"Start":"2026-11-25T12:00:01Z",
                "End":"2026-11-25T12:00:02Z","Output":"private"}]}});
        let observed = DateTime::parse_from_rfc3339("2026-11-25T12:00:03Z")
            .unwrap()
            .to_utc();
        assert_eq!(
            classify(Some(&probe), &state, observed),
            ("Healthy", "Ready")
        );
        assert_eq!(classify(None, &state, observed), ("Unknown", "Unknown"));
        state["Health"]["Status"] = json!("unhealthy");
        state["Health"]["FailingStreak"] = json!(2);
        assert_eq!(
            classify(Some(&probe), &state, observed),
            ("Unhealthy", "NotReady")
        );
        state["StartedAt"] = json!("2026-11-25T12:00:03Z");
        assert_eq!(
            classify(Some(&probe), &state, observed),
            ("Unknown", "Unknown")
        );
        state["Health"]["Status"] = json!("starting");
        assert_eq!(
            classify(Some(&probe), &state, observed),
            ("Unknown", "NotReady")
        );
        let mut result = json!({});
        status(&mut result, Some(&probe), &state);
        assert_eq!(
            result,
            json!({"health":"Unknown","readiness":"NotReady","health_probe_profile":PROFILE})
        );
        result["process_running"] = json!(true);
        result["health"] = json!("Healthy");
        result["readiness"] = json!("Ready");
        assert_eq!(projection(&result, false), ("Healthy", "Ready"));
        assert_eq!(projection(&result, true), ("Unknown", "Unknown"));
    }
}
