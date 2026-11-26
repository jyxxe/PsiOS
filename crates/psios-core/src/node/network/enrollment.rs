//! Root-configured ownership of one spare link for local configuration trials.
use crate::{
    ById, Result,
    config::NetworkLinkConfig,
    db::{Db, Doc},
    decode, policy, require,
};
use serde_json::{Value, json};

const KIND: &str = "NetworkLink";

fn resource_id(link: &NetworkLinkConfig) -> String {
    format!("network-link-{}", link.mac.replace(':', ""))
}

fn held(db: &Db<'_>, id: &str) -> Result<bool> {
    Ok(db.c.query_row(
        "SELECT EXISTS (SELECT 1 FROM holds WHERE volume=?1)",
        [id],
        |row| row.get(0),
    )?)
}

fn matches_config(db: &Db<'_>, resource: &Doc) -> bool {
    resource.kind == KIND
        && resource.owner == policy::SYSTEM
        && resource.body["lifecycle"] == "Registered"
        && db
            .cfg
            .network_link
            .as_ref()
            .is_some_and(|link| resource.id == resource_id(link) && resource.spec == json!(link))
}

pub(crate) fn require_configured(db: &Db<'_>, resource: &Doc) -> Result<()> {
    require(
        matches_config(db, resource),
        "PreconditionFailed",
        "network link does not match the current root enrollment",
    )
}

pub(crate) fn get(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let query: ById = decode(params)?;
    policy::can_target(db, actor, "network.trial", &query.id)?;
    let link = db.of(&query.id, KIND)?;
    Ok(json!({
        "reference": link.reference(),
        "configuration": link.spec,
        "state": link.body,
        "configured": matches_config(db, &link),
    }))
}

pub(crate) fn sync(db: &Db<'_>) -> Result<()> {
    let wanted = db.cfg.network_link.as_ref().map(resource_id);
    let enrolled =
        db.c.prepare("SELECT id FROM current WHERE kind=?1 AND body->>'lifecycle'='Registered'")?
            .query_map([KIND], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in enrolled.iter().filter(|id| Some(*id) != wanted.as_ref()) {
        // Keep the exact original revision readable until a trial is settled.
        // Its definition no longer authorizes effects after configuration changes.
        if held(db, id)? {
            return Ok(());
        }
        let previous = db.of(id, KIND)?;
        let mut body = previous.body.clone();
        body["lifecycle"] = json!("Unregistered");
        db.status(&previous, body)?;
    }
    let Some((configured, id)) = db.cfg.network_link.as_ref().zip(wanted.as_deref()) else {
        return Ok(());
    };
    let spec = serde_json::to_value(configured)?;
    if let Some(previous) = db.find(id)? {
        require(
            previous.kind == KIND && previous.owner == policy::SYSTEM,
            "RecoveryRequired",
            "network link ID is occupied by another resource",
        )?;
        if !matches_config(db, &previous) && !held(db, id)? {
            db.update(&previous, spec, json!({"lifecycle": "Registered"}))?;
        }
    } else {
        db.create(
            KIND,
            id,
            policy::SYSTEM,
            spec,
            json!({"lifecycle": "Registered"}),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Store};

    const LINK: &str = "network-link-025053000001";

    #[test]
    fn enrollments_keep_the_exact_link_revision_while_a_trial_holds_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config::local(&directory.path().join("core"));
        config.network_link = Some(NetworkLinkConfig {
            interface: "test-spare".into(),
            mac: "02:50:53:00:00:01".into(),
            original_uuid: "1aad7685-9a7d-49ad-8132-0c9c56cfb34e".into(),
            candidate_uuid: "597da7b3-c8ba-4134-bfa7-57377f1ab670".into(),
            confirmation_seconds: 40,
        });
        Store::init(&config).unwrap();
        let mut store = Store::open(config.clone()).unwrap();
        let before = store
            .transaction("pin-network-link", |db| {
                let link = db.of(LINK, KIND)?;
                require_configured(db, &link)?;
                db.c.execute(
                    "INSERT INTO holds(volume,holder) VALUES(?1,'trial-operation')",
                    [LINK],
                )?;
                Ok(link)
            })
            .unwrap();
        drop(store);

        config.network_link.as_mut().unwrap().interface = "other-spare".into();
        let mut store = Store::open(config).unwrap();
        store
            .transaction("honor-network-hold", |db| {
                let link = db.of(LINK, KIND)?;
                assert_eq!(link.reference(), before.reference());
                assert_eq!(
                    require_configured(db, &link).unwrap_err().0.code,
                    "PreconditionFailed"
                );
                db.c.execute(
                    "DELETE FROM holds WHERE volume=?1 AND holder='trial-operation'",
                    [LINK],
                )?;
                sync(db)?;
                let current = db.of(LINK, KIND)?;
                assert_eq!(current.spec["interface"], "other-spare");
                assert_eq!(current.generation, before.generation + 1);
                require_configured(db, &current)
            })
            .unwrap();
    }
}
