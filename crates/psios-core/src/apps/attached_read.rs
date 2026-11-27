//! Share a managed OCI attachment only while Linux proves the original mount is read-only.

use super::{
    Engine, launch_spec, mount_observation, selected_volume_access, verify_static_isolation,
};
use crate::{
    Result,
    db::{Db, Doc},
    err,
    host::{Install, VolumeAccess, Work},
    require,
};
use serde_json::Value;

pub(crate) const FORMAT: u32 = crate::artifact::ingress::windows::RELEASE_FORMAT + 1;

pub(crate) struct AttachedRead {
    application: Doc,
    launch: Install,
    mount: Value,
}

pub(crate) fn prepare(db: &Db<'_>, holder: &str, volume: &Doc) -> Result<Option<AttachedRead>> {
    let Some(application) = db.find(holder)?.filter(|app| {
        app.kind == "Application"
            && app.body["lifecycle"] == "Running"
            && app.body["runtime_state"] == "Running"
    }) else {
        return Ok(None);
    };
    if !selected_volume_access(db.c, &application)?.read_only() {
        return Ok(None);
    }
    let references = volume.body["attachment_refs"].as_array().map(Vec::as_slice);
    let Some([reference]) = references else {
        return Err(err(
            "RecoveryRequired",
            "read-only volume attachment reference is missing",
        ));
    };
    let attachment = db.resolve(reference)?;
    require(
        attachment.kind == "VolumeAttachment"
            && attachment.owner == application.owner
            && db.get(&attachment.id)?.reference() == *reference
            && attachment.body["lifecycle"] == "Attached"
            && attachment.body["volume_id"] == volume.id
            && attachment.body["workload_id"] == application.id
            && attachment.body["node_id"] == db.meta("authority")?
            && attachment.body["requested_access"] == "ReadOnly"
            && attachment.body["effective_access"] == "ReadOnly"
            && application.spec["volume_id"] == volume.id
            && attachment.body["data_incarnation"] == volume.body["data_incarnation"]
            && application.spec["data_incarnation"] == volume.body["data_incarnation"],
        "RecoveryRequired",
        "read-only volume attachment custody changed",
    )?;

    let instance = db.resolve(&application.body["instance_ref"])?;
    let observation = &instance.body["observation"];
    let mount = &observation[mount_observation::FIELD];
    let recorded_volume = db.resolve(&mount["volume_ref"])?;
    require(
        instance.kind == "RuntimeInstance"
            && instance.spec["application_id"] == application.id
            && instance.spec["deployment_ref"] == application.spec["deployment_ref"]
            && instance.spec["start_operation_id"] == application.body["start_operation_id"]
            && instance.spec["container_id"] == application.body["container_id"]
            && instance.spec["engine_id"] == application.body["engine_id"]
            && observation["container_id"] == instance.spec["container_id"]
            && observation["engine_id"] == instance.spec["engine_id"]
            && recorded_volume.kind == "Volume"
            && recorded_volume.id == volume.id
            && recorded_volume.spec["device"] == volume.spec["device"]
            && recorded_volume.spec["inode"] == volume.spec["inode"]
            && recorded_volume.body["data_incarnation"] == volume.body["data_incarnation"]
            && mount_observation::matches(&recorded_volume, observation, VolumeAccess::ReadOnly),
        "RecoveryRequired",
        "original read-only mount receipt no longer matches the attached volume",
    )?;
    let launch = launch_spec(db, &application, false)?;
    Ok(Some(AttachedRead {
        application,
        launch,
        mount: mount.clone(),
    }))
}

impl AttachedRead {
    pub(crate) fn verify(&self, work: &Work) -> Result<()> {
        let application = &self.application;
        let container = application.body["container_id"]
            .as_str()
            .ok_or_else(|| err("RecoveryRequired", "read-only attachment has no container"))?;
        let operation = application.body["start_operation_id"]
            .as_str()
            .ok_or_else(|| {
                err(
                    "RecoveryRequired",
                    "read-only attachment has no start operation",
                )
            })?;
        let engine = application.body["engine_id"]
            .as_str()
            .ok_or_else(|| err("RecoveryRequired", "read-only attachment has no engine"))?;
        let volume = work.volume.as_ref().ok_or_else(|| {
            err(
                "RecoveryRequired",
                "read-only attachment has no approved volume",
            )
        })?;
        let adapter = Engine(&work.cfg);
        let user = format!(
            "{}:{}",
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw()
        );
        let inspect = || {
            let current = adapter.managed_application(application, container, operation)?;
            verify_static_isolation(work.data.as_deref(), &current, &self.launch, &user)?;
            Ok(current["State"].clone())
        };
        let live = mount_observation::observe(
            volume,
            VolumeAccess::ReadOnly,
            engine,
            container,
            &inspect()?,
            inspect,
        )?;
        let mut expected = self.mount.clone();
        expected["volume_ref"] = volume.reference();
        expected["observed_at"] = live["observed_at"].clone();
        require(
            live == expected,
            "PreconditionFailed",
            "read-only attachment invocation or Linux mount changed",
        )
    }
}
