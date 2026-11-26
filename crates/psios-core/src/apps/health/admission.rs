//! Bind the effective health choice to the approved application catalog operation.
use super::*;
use crate::db::find;
use rusqlite::OptionalExtension;

const EPOCH: &str = "oci-health-admission-epoch";

pub(crate) fn migrate(db: &Db<'_>) -> Result<()> {
    db.create(
        "OCIHealthAdmissionEpoch",
        EPOCH,
        policy::SYSTEM,
        json!({}),
        json!({"format":FORMAT}),
    )?;
    Ok(())
}

pub(super) fn epoch(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT seq FROM current WHERE id=?1 AND kind='OCIHealthAdmissionEpoch' AND rev=1 AND owner=?2 AND body->>'format'=?3",
        params![EPOCH,policy::SYSTEM,FORMAT], |row|row.get(0),
    ).optional()?.ok_or_else(|| err("RecoveryRequired", "OCI health admission epoch is missing"))
}

pub(super) fn modern(conn: &Connection, release: &Doc) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT seq>?3 FROM versions WHERE id=?1 AND rev=?2",
        params![release.id, release.rev, epoch(conn)?],
        |row| row.get(0),
    )?)
}

pub(in crate::apps) fn record(db: &Db<'_>, work: &Work, install: &Install) -> Result<Value> {
    let source = match &work.action {
        Action::InstallArtifact(input) => json!(input.reference),
        Action::StageArtifactRelease(input) => json!(input.reference),
        _ => Value::Null,
    };
    let permit = db.resolve(&work.effect.spec["permit_ref"])?;
    let decision = db.resolve(&permit.body["decision_ref"])?;
    let approval = db.of(&format!("use-{}", decision.id), "ApprovalUse")?;
    require(
        approval.body["operation_id"] == work.op.id && approval.body["validity_status"] == "Valid",
        "ApprovalInvalidated",
        "OCI health admission lacks its original approval",
    )?;
    Ok(
        json!({"operation_ref":work.op.reference(),"task_ref":work.task.reference(),"effect_ref":work.effect.reference(),
        "plan_ref":work.op.body["plan_ref"],"input_ref":work.task.spec["input_ref"],"approval_use_ref":approval.reference(),"run_ref":work.run.reference(),
        "image_id":install.image_id,"health_check":install.health_check,"source_ref":source}),
    )
}

fn same_choice(probe: Option<&Probe>, image: &Doc, declared: &Install) -> bool {
    declared.health_check.as_ref() == probe && image.body["config_digest"] == declared.image_id
}

fn signed_source(
    conn: &Connection,
    release: &Doc,
    spec: &Doc,
    plan: &Doc,
    operation: &Doc,
    binding: &Value,
    input: &Value,
) -> Result<Doc> {
    let source = resolve(conn, &binding["source_ref"])?;
    let attribution = &spec.body["provenance"];
    let tool = plan.spec["action"].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "OCI health admission has no catalog action",
        )
    })?;
    require(
        artifact_install::is_declaration(&source)
            && plan.body["preconditions"][1]["resource_ref"] == source.reference()
            && input["reference"] == source.reference()
            && artifact_install::recorded_release(conn, operation, tool, &source)?.reference()
                == release.reference(),
        "RecoveryRequired",
        "OCI health admission does not bind the approved source declaration",
    )?;
    if input["signature"].is_null() {
        require(
            attribution["source_trust"] == "UserSuppliedUnverified",
            "RecoveryRequired",
            "OCI health admission source trust is inconsistent",
        )?;
    } else {
        let proof = resolve(conn, &attribution["trust_evidence_or_admission_policy_ref"])?;
        require(
            attribution["source_trust"] == "PublisherVerified"
                && proof.kind == "OciPublisherVerification"
                && proof.spec["operation_id"] == binding["operation_ref"]["resource_id"]
                && proof.body["artifact_ref"] == source.reference()
                && proof.body["signature"] == input["signature"],
            "RecoveryRequired",
            "OCI health admission disagrees with the original signed declaration",
        )?;
    }
    Ok(source)
}

fn direct_result(
    conn: &Connection,
    operation: &Doc,
    task: &Doc,
    release: &Doc,
    staged: bool,
) -> Result<()> {
    let effect = resolve(conn, &operation.body["effect_refs"][0])?;
    let receipt = resolve(conn, &effect.body["evidence_refs"][0])?;
    let expected = if staged {
        release.reference()
    } else {
        let app = conn.query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=1",
            [release.body["application_id"].as_str()],
            row,
        )?;
        require(
            app.kind == "Application"
                && app.owner == release.owner
                && app.spec["release_ref"] == release.reference(),
            "RecoveryRequired",
            "OCI health admission is not linked to the installed application revision",
        )?;
        app.reference()
    };
    require(
        artifact::is_json(&receipt)
            && receipt.owner == operation.owner
            && receipt.spec["task_id"] == task.id
            && receipt.spec["operation_id"] == operation.id
            && receipt.spec["effect_id"] == effect.id
            && effect.kind == "Effect"
            && effect.body["phase"] == "EffectConfirmed"
            && effect.body["outcome_knowledge"] == "Known"
            && effect.body["evidence_refs"] == json!([receipt.reference()])
            && receipt.spec["digest"] == psios_api::sha256(&serde_json::to_vec(&expected)?),
        "RecoveryRequired",
        "OCI health admission is not linked to its catalog result",
    )
}

pub(super) fn approved_source(
    conn: &Connection,
    release: &Doc,
    spec: &Doc,
    image: &Doc,
    probe: Option<&Probe>,
) -> Result<Option<Doc>> {
    let binding = &image.body["health_admission"];
    let operation = resolve(conn, &binding["operation_ref"])?;
    let task = resolve(conn, &binding["task_ref"])?;
    let effect = resolve(conn, &binding["effect_ref"])?;
    let run = resolve(conn, &binding["run_ref"])?;
    let plan = resolve(conn, &binding["plan_ref"])?;
    let input = resolve(conn, &binding["input_ref"])?;
    let permit = resolve(conn, &effect.spec["permit_ref"])?;
    let proof = resolve(conn, &permit.body["auth_proof"]["record_ref"])?;
    let decision = resolve(conn, &permit.body["decision_ref"])?;
    let used = resolve(conn, &binding["approval_use_ref"])?;
    let request = resolve(conn, &decision.body["request_ref"])?;
    let current = find(conn, &operation.id)?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "OCI health admission operation is missing",
        )
    })?;
    let tool = task.spec["task_type"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "OCI health admission has no tool"))?;
    let indexed: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM approval_bindings WHERE request=?1 AND task=?2 AND decision=?3 AND operation=?4)",
        params![request.id,task.id,decision.id,operation.id],|row|row.get(0))?;
    require(
        indexed
            && binding.get("health_check").is_some()
            && binding["health_check"] == json!(probe)
            && binding["image_id"] == image.body["config_digest"]
            && image.kind == "OCIImage"
            && image.rev == 1
            && operation.kind == "Operation"
            && task.kind == "Task"
            && task.owner == operation.owner
            && task.spec["input_ref"] == input.reference()
            && operation.spec["task_id"] == task.id
            && operation.body["outcome_knowledge"] == "InFlight"
            && task.body["operation_refs"] == json!([operation.reference()])
            && run.kind == "Run"
            && run.owner == task.owner
            && run.body["task_id"] == task.id
            && run.body["operation_refs"] == task.body["operation_refs"]
            && run.body["outcome_knowledge"] == "InFlight"
            && task.body["current_run_id"] == run.id
            && operation.body["effect_refs"] == json!([effect.reference()])
            && operation.body["plan_ref"] == plan.reference()
            && effect.kind == "Effect"
            && effect.owner == operation.owner
            && effect.body["action"] == tool
            && effect.body["operation_id"] == operation.id
            && effect.body["resource_ref"] == plan.body["allowed_effects"][0]["resource_ref"]
            && current.kind == "Operation"
            && current.spec == operation.spec
            && current.owner == operation.owner
            && current.body["plan_ref"] == plan.reference()
            && current.body["outcome_knowledge"] == "Known"
            && current.body["effect_refs"][0]["resource_id"] == effect.id
            && plan.kind == "OperationPlan"
            && plan.owner == operation.owner
            && plan.body["purpose_ref"] == input.reference()
            && plan.body["allowed_effects"][0]["action"] == tool
            && permit.kind == "ExecutionPermit"
            && permit.rev == 1
            && permit.owner == operation.owner
            && permit.body["operation_id"] == operation.id
            && permit.body["effect_id"] == effect.id
            && permit.body["plan_digest"] == plan.body["plan_digest"]
            && permit.body["preconditions"] == plan.body["preconditions"]
            && permit.body["authorization_ref"] == task.spec["delegation_ref"]
            && permit.body["recovery_incarnation"] == run.body["recovery_incarnation"]
            && proof.kind == "PermitBinding"
            && proof.rev == 1
            && proof.owner == operation.owner
            && proof.body["permit_id"] == permit.id
            && proof.body["operation_id"] == operation.id
            && proof.body["effect_id"] == effect.id
            && proof.body["plan_digest"] == plan.body["plan_digest"]
            && proof.body["claim_id"] == run.body["claim_id"]
            && proof.body["claim_epoch"] == run.body["claim_epoch"]
            && decision.kind == "ApprovalDecision"
            && decision.rev == 1
            && decision.owner == operation.owner
            && decision.body["decision"] == "Approved"
            && decision.body["approver_principal_id"] == policy::OWNER
            && decision.body["plan_digest"] == plan.body["plan_digest"]
            && decision.body["preconditions"] == plan.body["preconditions"]
            && request.kind == "ApprovalRequest"
            && request.rev == 1
            && request.owner == operation.owner
            && request.spec["task_id"] == task.id
            && request.body["status"] == "Pending"
            && request.body["plan_ref"] == plan.reference()
            && request.body["plan_digest"] == plan.body["plan_digest"]
            && used.kind == "ApprovalUse"
            && used.rev == 2
            && used.owner == operation.owner
            && used.id == format!("use-{}", decision.id)
            && used.body["plan_digest"] == plan.body["plan_digest"]
            && used.body["decision_ref"] == decision.reference()
            && used.body["binding_status"] == "Bound"
            && used.body["validity_status"] == "Valid"
            && used.body["operation_id"] == operation.id,
        "RecoveryRequired",
        "OCI release health choice disagrees with its original approved admission",
    )?;

    let input = crate::broker::dependencies::tool_input(&input)?;
    let target = &plan.body["allowed_effects"][0]["resource_ref"]["resource_id"];
    let app = &release.body["application_id"];
    match Action::parse(tool, input)? {
        Action::Install(install)
            if app == &json!(format!("app-{}", operation.id))
                && same_choice(probe, image, &install)
                && binding["source_ref"].is_null() =>
        {
            direct_result(conn, &current, &task, release, false)?;
            Ok(None)
        }
        Action::Stage {
            release: install, ..
        } if app == target
            && same_choice(probe, image, &install)
            && binding["source_ref"].is_null() =>
        {
            direct_result(conn, &current, &task, release, true)?;
            Ok(None)
        }
        Action::InstallArtifact(_) if app == &json!(format!("app-{}", operation.id)) => {
            signed_source(conn, release, spec, &plan, &current, binding, input).map(Some)
        }
        Action::StageArtifactRelease(_) if app == target => {
            signed_source(conn, release, spec, &plan, &current, binding, input).map(Some)
        }
        _ => Err(err(
            "RecoveryRequired",
            "OCI release health choice does not match the approved input",
        )),
    }
}

pub(super) fn check_descriptor(
    probe: Option<&Probe>,
    image: &Doc,
    descriptor: &artifact_install::Descriptor,
) -> Result<()> {
    require(
        same_choice(probe, image, &descriptor.install),
        "RecoveryRequired",
        "OCI health choice differs from its retained declaration",
    )
}

pub(super) fn verify_retained(
    conn: &Connection,
    release: &Doc,
    image: &Doc,
    probe: Option<&Probe>,
    source: &Doc,
) -> Result<()> {
    match artifact::retention::require_content(conn, source) {
        Ok(()) => check_descriptor(
            probe,
            image,
            &artifact_install::read_descriptor(conn, source)?,
        ),
        Err(error) if error.0.code == "ResultEvicted" => {
            let retired = find(conn, &release_retirement::receipt_id(&release.id))?;
            require(
                retired.is_some_and(|record| {
                    record.kind == "ApplicationReleaseRetirement"
                        && record.owner == release.owner
                        && record.body["release_ref"] == release.reference()
                }),
                "RecoveryRequired",
                "live OCI health declaration is no longer retained",
            )
        }
        Err(error) => Err(error),
    }
}
