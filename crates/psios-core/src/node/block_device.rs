//! A named, boot-local scope for a verified whole block device.
pub(crate) mod reconcile;
pub(crate) mod retirement;

use super::block::{self, Fence, Presence};
use crate::{
    ById, Result,
    db::{Db, Doc, row},
    decode, err,
    host::{Action, Target, Work},
    now, policy, require,
};
use psios_api::{canonical, sha256};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::apps::mount_observation::FORMAT + 1;
pub(crate) const TRACK: &str = "node.track_block_device";
pub(crate) const OBSERVE: &str = "device.block_layout";
pub(crate) const KIND: &str = "BlockDevice";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    major_minor: String,
    expected: Fence,
}

impl Enrollment {
    pub(crate) fn validate(&self) -> Result<()> {
        require(
            block::device_number(&self.major_minor).is_some()
                && self.expected.valid()
                && self.expected.attachment_digest.is_some(),
            "InvalidArgument",
            "track a whole disk using an exact trackable layout fence",
        )
    }

    fn attachment(&self, node_id: String) -> Result<Identity> {
        let attachment_digest = self.expected.attachment_digest.clone().ok_or_else(|| {
            err(
                "RecoveryRequired",
                "block enrollment has no attachment identity",
            )
        })?;
        Ok(Identity {
            node_id,
            boot_id: self.expected.boot_id.clone(),
            major_minor: self.major_minor.clone(),
            disk_sequence: self.expected.disk_sequence.clone(),
            attachment_digest,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    node_id: String,
    boot_id: String,
    major_minor: String,
    disk_sequence: String,
    attachment_digest: String,
}

impl Identity {
    fn id(&self) -> String {
        let digest = sha256(&canonical(&json!(["psios/boot-block-device/1", self])));
        format!("block-device-{}", &digest[7..])
    }

    fn matches(&self, fence: &Fence) -> bool {
        self.boot_id == fence.boot_id
            && self.disk_sequence == fence.disk_sequence
            && fence.attachment_digest.as_deref() == Some(self.attachment_digest.as_str())
    }
}

fn identity(device: &Doc, node_id: &str) -> Result<Identity> {
    let identity: Identity = decode(&device.spec)
        .map_err(|_| err("RecoveryRequired", "invalid tracked block identity"))?;
    require(
        device.kind == KIND
            && device.owner == policy::OWNER
            && identity.node_id == node_id
            && device.id == identity.id()
            && block::device_number(&identity.major_minor).is_some()
            && block::valid_boot(&identity.boot_id)
            && block::valid_sequence(&identity.disk_sequence)
            && block::valid_digest(&identity.attachment_digest),
        "RecoveryRequired",
        "tracked block device identity disagrees with its original enrollment",
    )?;
    Ok(identity)
}

fn enrolled(db: &Db<'_>, expected: &Identity) -> Result<Option<Doc>> {
    let existing = db.find(&expected.id())?;
    if let Some(device) = &existing {
        require(
            identity(device, &expected.node_id)? == *expected,
            "RecoveryRequired",
            "tracked block device ID already belongs to another attachment",
        )?;
    }
    Ok(existing)
}

pub(crate) fn enrollment_target(db: &Db<'_>, actor: &str, request: &Enrollment) -> Result<Doc> {
    policy::admin(actor)?;
    let node = db.of(&db.meta("authority")?, "Node")?;
    let expected = request.attachment(node.id.clone())?;
    require(
        enrolled(db, &expected)?.is_none_or(|device| device.body["lifecycle"] != "Retired"),
        "PreconditionFailed",
        "block attachment enrollment has been retired",
    )?;
    Ok(node)
}

pub(crate) fn target(db: &Db<'_>, actor: &str, query: &Target) -> Result<Doc> {
    crate::label(&query.id)?;
    policy::can_target(db, actor, OBSERVE, &query.id)?;
    let device = db.of(&query.id, KIND)?;
    device.cas(&query.expected_resource_version)?;
    identity(&device, &db.meta("authority")?)?;
    require(
        device.body["lifecycle"] != "Retired",
        "PreconditionFailed",
        "block attachment enrollment has been retired",
    )?;
    Ok(device)
}

pub(crate) fn get(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let request: ById = decode(request)?;
    crate::label(&request.id)?;
    policy::can_target(db, actor, OBSERVE, &request.id)?;
    let device = db.of(&request.id, KIND)?;
    let node = db.of(&db.meta("authority")?, "Node")?;
    let expected = identity(&device, &node.id)?;
    Ok(view(&device, &expected, &node))
}

fn view(device: &Doc, expected: &Identity, node: &Doc) -> Value {
    json!({"reference": device.reference(), "state": device.body,
           "on_current_boot": node.body["boot_id"] == expected.boot_id})
}

fn present(sample: block::Sample) -> Value {
    json!({"lifecycle":"Present", "fence":sample.fence, "layout":sample.layout,
           "mount_namespace_inode":sample.mount_namespace_inode, "observed_at":sample.observed_at})
}

pub(super) fn enroll(request: &Enrollment) -> Result<Value> {
    let sample = block::sample(&request.major_minor, true)?;
    require(
        sample.fence == request.expected,
        "PreconditionFailed",
        "block attachment or layout changed since the enrollment observation",
    )?;
    Ok(present(sample))
}

fn unobserved(lifecycle: &str, timestamp: &str) -> Value {
    json!({"lifecycle":lifecycle,"observed_at":timestamp})
}

pub(super) fn observe(work: &Work) -> Result<Value> {
    let node_id = work.target.spec["node_id"].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "tracked block device has no node identity",
        )
    })?;
    let expected = identity(&work.target, node_id)?;
    let previous = &work.target.body;
    if let Some(lifecycle @ ("Replaced" | "BootEnded")) = previous["lifecycle"].as_str() {
        let timestamp = previous["observed_at"].as_str().ok_or_else(|| {
            err(
                "RecoveryRequired",
                "terminal block device lacks an observation time",
            )
        })?;
        return Ok(unobserved(lifecycle, timestamp));
    }
    sample(&expected)
}

fn sample(expected: &Identity) -> Result<Value> {
    if crate::linux_clock::boot_id()? != expected.boot_id {
        return Ok(unobserved("BootEnded", &now()));
    }
    match block::presence(&expected.major_minor, &expected.disk_sequence)? {
        Presence::Absent => Ok(unobserved("Absent", &now())),
        Presence::OtherSequence => Ok(unobserved("Replaced", &now())),
        Presence::SameSequence => {
            let sample = block::sample(&expected.major_minor, true)?;
            if expected.matches(&sample.fence) {
                Ok(present(sample))
            } else {
                Ok(unobserved("Replaced", &sample.observed_at))
            }
        }
    }
}

fn verified_fence(receipt: &Value, major_minor: &str) -> Result<Fence> {
    let fence: Fence = decode(&receipt["fence"]).map_err(|_| {
        err(
            "RecoveryRequired",
            "block observation lacks a valid layout fence",
        )
    })?;
    require(
        receipt["lifecycle"] == "Present"
            && fence.valid()
            && fence.attachment_digest.is_some()
            && receipt["layout"]["major_minor"] == major_minor
            && receipt["layout"]["disk_sequence"] == fence.disk_sequence
            && sha256(&canonical(&receipt["layout"])) == fence.layout_digest,
        "RecoveryRequired",
        "tracked block result disagrees with the sampled layout",
    )?;
    Ok(fence)
}

fn state(identity: &Identity, receipt: &Value, generation: i64) -> Value {
    json!({"identity":identity, "lifecycle":receipt["lifecycle"], "observed_at":receipt["observed_at"],
           "layout_digest":receipt["fence"]["layout_digest"], "device_generation":generation.to_string()})
}

fn changes_state(existing: &Doc, receipt: &Value) -> bool {
    existing.body["lifecycle"] != receipt["lifecycle"]
        || existing.body["layout_digest"] != receipt["fence"]["layout_digest"]
}

fn watched(body: &Value) -> bool {
    matches!(body["lifecycle"].as_str(), Some("Present" | "Absent"))
}

fn update(db: &Db<'_>, existing: &Doc, expected: &Identity, receipt: &Value) -> Result<Doc> {
    if !changes_state(existing, receipt) {
        return Ok(existing.clone());
    }
    require(
        watched(&existing.body),
        "PreconditionFailed",
        "block attachment has reached its terminal identity state",
    )?;
    db.status(existing, state(expected, receipt, existing.rev + 1))
}

pub(crate) fn publish(db: &Db<'_>, work: &Work, receipt: &Value) -> Result<Value> {
    require(
        receipt["observed_at"]
            .as_str()
            .is_some_and(|at| chrono::DateTime::parse_from_rfc3339(at).is_ok()),
        "RecoveryRequired",
        "block observation lacks a timestamp",
    )?;
    let node_id = db.meta("authority")?;
    let device = match &work.action {
        Action::TrackBlockDevice(request) => {
            require(
                work.target.id == node_id,
                "Fenced",
                "block enrollment belongs to another node",
            )?;
            let fence = verified_fence(receipt, &request.major_minor)?;
            require(
                fence == request.expected,
                "Fenced",
                "block enrollment fence changed",
            )?;
            let expected = request.attachment(node_id)?;
            if let Some(existing) = enrolled(db, &expected)? {
                update(db, &existing, &expected, receipt)?
            } else {
                db.cfg.check_admission_space()?;
                db.create(
                    KIND,
                    &expected.id(),
                    policy::OWNER,
                    json!(expected),
                    state(&expected, receipt, 1),
                )?
            }
        }
        Action::ObserveBlockDevice(_) => {
            let expected = identity(&work.target, &node_id)?;
            match receipt["lifecycle"].as_str() {
                Some("Present") => require(
                    expected.matches(&verified_fence(receipt, &expected.major_minor)?),
                    "Fenced",
                    "scoped block observation belongs to another attachment",
                )?,
                Some("Absent" | "Replaced" | "BootEnded") => require(
                    receipt["layout"].is_null() && receipt["fence"].is_null(),
                    "RecoveryRequired",
                    "unavailable attachment returned a block layout",
                )?,
                _ => {
                    return Err(err(
                        "RecoveryRequired",
                        "invalid block device observation state",
                    ));
                }
            }
            update(db, &work.target, &expected, receipt)?
        }
        _ => return Err(err("Fenced", "block observation has no matching action")),
    };
    let mut result = json!({"device_ref":device.reference(), "identity":device.spec,
        "lifecycle":receipt["lifecycle"], "observed_at":receipt["observed_at"]});
    if receipt["lifecycle"] == "Present" && matches!(work.action, Action::ObserveBlockDevice(_)) {
        result["fence"] = receipt["fence"].clone();
        result["layout"] = receipt["layout"].clone();
        result["mount_namespace_inode"] = receipt["mount_namespace_inode"].clone();
    }
    Ok(result)
}

pub(crate) fn verify(conn: &Connection, format: u32) -> Result<()> {
    let node_id: String =
        conn.query_row("SELECT value FROM meta WHERE key='authority'", [], |r| {
            r.get(0)
        })?;
    let mut query = conn.prepare("SELECT * FROM versions WHERE kind=?1 ORDER BY id,rev")?;
    let mut rows = query.query([KIND])?;
    let mut previous: Option<Doc> = None;
    while let Some(r) = rows.next()? {
        let device = row(r)?;
        let expected = identity(&device, &node_id)?;
        let lifecycle = device.body["lifecycle"].as_str();
        require(
            device.body["identity"] == json!(expected)
                && device.body["device_generation"] == device.rev.to_string().as_str()
                && device.body["observed_at"]
                    .as_str()
                    .is_some_and(|at| chrono::DateTime::parse_from_rfc3339(at).is_ok())
                && match lifecycle {
                    Some("Present") => device.body["layout_digest"]
                        .as_str()
                        .is_some_and(block::valid_digest),
                    Some("Absent" | "Replaced" | "BootEnded") => {
                        device.body["layout_digest"].is_null()
                    }
                    Some("Retired") => {
                        format >= retirement::FORMAT && device.body["layout_digest"].is_null()
                    }
                    _ => false,
                },
            "RecoveryRequired",
            "invalid tracked block device state",
        )?;
        if let Some(prior) = previous.as_ref().filter(|prior| prior.id == device.id) {
            require(
                prior.rev + 1 == device.rev && prior.spec == device.spec && watched(&prior.body),
                "RecoveryRequired",
                "tracked block device history violates its original attachment",
            )?;
            if lifecycle == Some("Retired") {
                retirement::verify_transition(prior, &device, &expected)?;
            }
        } else {
            require(
                device.rev == 1 && lifecycle == Some("Present"),
                "RecoveryRequired",
                "tracked block device has no original present observation",
            )?;
        }
        previous = Some(device);
    }
    if format >= retirement::FORMAT {
        retirement::verify_projection(conn)?;
    }
    Ok(())
}
