use super::{lease, state};
use crate::{ById, Result, db::Db, db::Doc, decode, err, policy, require};
use psios_api::{
    ModelCallOutcome, ModelCallUsage, ModelLeaseTimeUsage, ModelTokenUsage,
    ProviderReportedModelUsage, ResourceRef, SessionModelUsage,
};
use serde_json::{Value, json};

struct Turn {
    number: u32,
    lease_id: String,
    responded: bool,
    usage_ref: Option<Value>,
    inference: Option<String>,
    budget: Option<String>,
    reserved_ms: Option<u32>,
    settled_ms: Option<u32>,
}

impl Turn {
    fn outcome(&self, db: &Db<'_>, session: &Doc) -> Result<ModelCallOutcome> {
        use ModelCallOutcome::*;
        match (self.responded, self.inference.as_deref()) {
            (true, None | Some("Responded")) => Ok(Responded),
            (false, Some("NotStarted")) => Ok(NotStarted),
            (false, Some("Unknown")) => Ok(Unknown),
            (false, None | Some("Pending")) => {
                let active = state(session) == "ModelInFlight"
                    && session.body["current_lease"] == self.lease_id
                    && lease::still_authorized(db, session, &self.lease_id)?;
                Ok(if active { Active } else { Unknown })
            }
            _ => Err(err(
                "RecoveryRequired",
                "model usage has conflicting outcomes",
            )),
        }
    }

    fn tokens(&self, db: &Db<'_>, session: &Doc) -> Result<Option<ModelTokenUsage>> {
        let Some(reference) = &self.usage_ref else {
            return Ok(None);
        };
        let usage = db.resolve(reference)?;
        require(
            self.responded
                && usage.kind == "ModelUsage"
                && usage.owner == session.owner
                && usage.body["session_id"] == session.id
                && usage.body["call_id"] == self.lease_id
                && usage.body["model_ref"] == session.spec["model_ref"]
                && usage.body["budget_ref"] == session.body["budget_ref"],
            "RecoveryRequired",
            "model token receipt disagrees with its session or lease",
        )?;
        usage
            .body
            .get("reported_tokens")
            .map(|tokens| {
                require(
                    usage.body["measurement_source"] == "ProviderReported",
                    "RecoveryRequired",
                    "model token receipt has an invalid measurement source",
                )?;
                serde_json::from_value(tokens.clone()).map_err(|_| {
                    err(
                        "RecoveryRequired",
                        "model token receipt has invalid quantities",
                    )
                })
            })
            .transpose()
    }

    fn charge(
        &self,
        outcome: ModelCallOutcome,
        budget: &str,
        total: &mut ModelLeaseTimeUsage,
    ) -> Result<Option<u32>> {
        use ModelCallOutcome::*;
        if self.budget.is_none() && self.reserved_ms.is_none() && self.settled_ms.is_none() {
            total.unmetered_calls += 1;
            return Ok(None);
        }
        require(
            self.budget.as_deref() == Some(budget),
            "RecoveryRequired",
            "model lease charge has a different budget",
        )?;
        let charge = match (outcome, self.reserved_ms, self.settled_ms) {
            (Active, Some(reserved), None) => {
                total.active_reserved_ms += u64::from(reserved);
                reserved
            }
            (Unknown, Some(reserved), None) => {
                total.retained_unknown_ms += u64::from(reserved);
                reserved
            }
            (Responded | NotStarted, Some(reserved), Some(settled)) if settled <= reserved => {
                total.settled_ms += u64::from(settled);
                settled
            }
            _ => {
                return Err(err(
                    "RecoveryRequired",
                    "model lease charge disagrees with its outcome",
                ));
            }
        };
        Ok(Some(charge))
    }
}

fn inherited_turns(db: &Db<'_>, session: &Doc) -> Result<u64> {
    let Some(reference) = session.spec.get("predecessor_ref") else {
        return Ok(0);
    };
    let previous = db.resolve(reference)?;
    require(
        previous.kind == "Session"
            && previous.owner == session.owner
            && previous.body["budget_ref"] == session.body["budget_ref"],
        "RecoveryRequired",
        "session model handoff has a different owner or budget",
    )?;
    previous.body["turn"]
        .as_u64()
        .filter(|turn| *turn <= 32)
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "session model handoff has no valid original turn",
            )
        })
}

pub(crate) fn get(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let request: ById = decode(request)?;
    let session = db.of(&request.id, "Session")?;
    policy::owned(actor, &session)?;
    policy::can(db, actor, "task.read")?;
    let budget: ResourceRef =
        serde_json::from_value(session.body["budget_ref"].clone()).map_err(|_| {
            err(
                "RecoveryRequired",
                "session is missing its exact budget reference",
            )
        })?;
    let mut rows = db.c.prepare(
        "SELECT t.turn,t.lease,t.receipt IS NOT NULL,t.receipt->'usage_ref',i.status,
                l.budget,l.reserved_ms,l.settled_ms
         FROM session_turns t LEFT JOIN model_lease_times l USING(lease)
         LEFT JOIN model_inference_leases i USING(lease)
         WHERE t.session=?1 ORDER BY t.turn LIMIT 33",
    )?;
    let turns = rows
        .query_map([&session.id], |row| {
            Ok(Turn {
                number: row.get(0)?,
                lease_id: row.get(1)?,
                responded: row.get(2)?,
                usage_ref: row.get(3)?,
                inference: row.get(4)?,
                budget: row.get(5)?,
                reserved_ms: row.get(6)?,
                settled_ms: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let inherited = inherited_turns(db, &session)?;
    let last = inherited + turns.len() as u64;
    require(
        last <= 32
            && session.body["turn"] == last
            && turns
                .iter()
                .enumerate()
                .all(|(offset, turn)| u64::from(turn.number) == inherited + offset as u64 + 1),
        "RecoveryRequired",
        "session turn sequence disagrees with the model ledger",
    )?;
    let mut summary = SessionModelUsage {
        session_ref: decode(&session.reference())?,
        budget_ref: budget,
        charged_calls: turns.len() as u32,
        lease_time: ModelLeaseTimeUsage::default(),
        provider_reported: ProviderReportedModelUsage::default(),
        calls: Vec::with_capacity(turns.len()),
    };
    for turn in turns {
        let outcome = turn.outcome(db, &session)?;
        let charged_lease_ms = turn.charge(
            outcome,
            &summary.budget_ref.resource_id,
            &mut summary.lease_time,
        )?;
        let tokens = turn.tokens(db, &session)?;
        if let Some(tokens) = tokens {
            summary.provider_reported.reported_calls += 1;
            summary.provider_reported.input_tokens += u64::from(tokens.input_tokens);
            summary.provider_reported.output_tokens += u64::from(tokens.output_tokens);
        }
        summary.calls.push(ModelCallUsage {
            turn: turn.number,
            lease_id: turn.lease_id,
            outcome,
            charged_lease_ms,
            provider_reported_tokens: tokens,
        });
    }
    Ok(json!(summary))
}
