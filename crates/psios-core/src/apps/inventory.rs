//! Application-scoped release history over the catalog's commit order.
use crate::{
    Result,
    db::{Db, Doc, row},
    decode,
    events::ScopedSnapshot,
    label, policy, require,
};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = super::artifact_install::STAGE_FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("inventory.sql");
pub(crate) const TOOL: &str = "application.releases";

const RELEASE_PAGE: &str = "SELECT r.*, EXISTS(SELECT 1 FROM versions d INDEXED BY deployment_release_lookup
           WHERE d.kind='Deployment' AND d.rev=1 AND d.spec->>'application_id'=?1
           AND d.spec->'release_ref'->>'resource_id'=r.id AND d.seq<=?4) AS had_deployment
         FROM versions r WHERE r.kind='ApplicationRelease' AND r.rev=1 AND r.body->>'application_id'=?1
           AND r.owner=?2 AND r.seq>?3 AND r.seq<=?4 ORDER BY r.seq LIMIT ?5";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub application_id: String,
    cursor: Option<String>,
    limit: Option<u32>,
}

impl Request {
    pub(crate) fn validate(&self) -> Result<()> {
        label(&self.application_id)?;
        require(
            (1..=64).contains(&self.limit.unwrap_or(32)),
            "InvalidArgument",
            "release page limit must be 1..64",
        )
    }
}

pub(crate) fn list(db: &Db<'_>, actor: &str, input: &Value) -> Result<Value> {
    let input: Request = decode(input)?;
    input.validate()?;
    policy::can_target(db, actor, TOOL, &input.application_id)?;
    let app = db.of(&input.application_id, "Application")?;
    policy::owned(actor, &app)?;
    page(db, actor, &input, &app)
}

pub(crate) fn page(db: &Db<'_>, actor: &str, input: &Request, app: &Doc) -> Result<Value> {
    let scope = format!("application.releases/{}", app.id);
    let snapshot = ScopedSnapshot::open(db, actor, &scope, input.cursor.as_deref())?;
    let application =
        db.c.query_row(
            "SELECT * FROM versions WHERE id=?1 AND seq<=?2 ORDER BY seq DESC LIMIT 1",
            params![app.id, snapshot.high],
            row,
        )
        .optional()?
        .ok_or_else(|| {
            crate::err(
                "CursorExpired",
                "application is unavailable in the release snapshot",
            )
        })?;
    require(
        application.kind == "Application" && application.owner == app.owner,
        "RecoveryRequired",
        "release snapshot disagrees with application ownership",
    )?;
    let deployed = application.body["lifecycle"] != "Uninstalled";
    let selected = deployed.then(|| &application.spec["release_ref"]);
    let deployment = deployed.then(|| &application.spec["deployment_ref"]);
    let limit = input.limit.unwrap_or(32);
    let mut query = db.c.prepare_cached(RELEASE_PAGE)?;
    let mut releases = query
        .query_map(
            params![
                app.id,
                app.owner,
                snapshot.position,
                snapshot.high,
                limit + 1
            ],
            |r| {
                Ok((
                    r.get::<_, i64>("seq")?,
                    row(r)?,
                    r.get::<_, bool>("had_deployment")?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let complete = releases.len() <= limit as usize;
    releases.truncate(limit as usize);
    let next = if complete {
        None
    } else {
        releases
            .last()
            .map(|(position, _, _)| snapshot.sign(db, actor, &scope, *position))
            .transpose()?
    };

    let mut items = Vec::with_capacity(releases.len());
    for (_, release, had_deployment) in releases {
        let retired = db.c.prepare_cached(
           "SELECT * FROM versions WHERE id=?1 AND kind='ApplicationReleaseRetirement' AND rev=1 AND seq<=?2",
        )?.query_row(params![super::release_retirement::receipt_id(&release.id),snapshot.high],row).optional()?;
        let source =
            db.c.prepare_cached(
                "SELECT body->'source_artifact_ref' FROM versions WHERE rev=1
             AND kind IN ('ApplicationArtifactInstallation','ApplicationArtifactReleaseStage')
             AND spec->>'release_id'=?1 AND owner=?2 AND seq<=?3",
            )?
            .query_row(params![release.id, release.owner, snapshot.high], |r| {
                r.get::<_, Value>(0)
            })
            .optional()?;
        let reference = release.reference();
        let state = if retired.is_some() {
            "Retired"
        } else if selected == Some(&reference) {
            "Selected"
        } else if had_deployment {
            "Unselected"
        } else {
            "Staged"
        };
        items.push(
            json!({"release_ref":reference,"release_version":release.body["release_version"],
            "package_format":release.body["package_format"],"state":state,
            "source_artifact_ref":source,"retirement_ref":retired.as_ref().map(Doc::reference)}),
        );
    }
    Ok(
        json!({"application_ref":application.reference(),"application_lifecycle":application.body["lifecycle"],
        "selected_release_ref":selected,"deployment_ref":deployment,"items":items,"complete":complete,"next_cursor":next}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, StatementStatus};

    #[test]
    fn release_page_seeks_past_other_applications_and_deployments() {
        let db = Connection::open_in_memory().unwrap();
        for sql in [
            include_str!("../state.sql"),
            include_str!("../events/queries.sql"),
            include_str!("../events/snapshots.sql"),
            super::super::release_retirement::SCHEMA,
            SCHEMA,
        ] {
            db.execute_batch(sql).unwrap();
        }
        db.execute_batch(
            "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<12000),
             fixtures AS (
               SELECT 'release-'||i AS id,'ApplicationRelease' AS kind,'{}' AS spec,
                 json_object('application_id','outside') AS body FROM n
               UNION ALL SELECT 'deployment-'||i,'Deployment',
                 json_object('application_id','outside','release_ref',json_object('resource_id','release-'||i)),'{}' FROM n
               UNION ALL SELECT 'wanted-'||i,'ApplicationRelease','{}',json_object('application_id','mine') FROM n WHERE i<=8
               UNION ALL SELECT 'selected','Deployment',
                 json_object('application_id','mine','release_ref',json_object('resource_id','wanted-2')),'{}'
             ) INSERT INTO versions(id,rev,gen,kind,owner,spec,body,created,stamp,event_id,correlation,incarnation)
               SELECT id,1,1,kind,'owner',spec,body,'now','now',id,'test','local' FROM fixtures;",
        ).unwrap();
        let mut page = db.prepare(RELEASE_PAGE).unwrap();
        let rows = page
            .query_map(params!["mine", "owner", 0, 24009, 4], |r| {
                Ok((
                    r.get::<_, String>("id")?,
                    r.get::<_, bool>("had_deployment")?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows.iter()
                .map(|(id, deployed)| (id.as_str(), *deployed))
                .collect::<Vec<_>>(),
            [
                ("wanted-1", false),
                ("wanted-2", true),
                ("wanted-3", false),
                ("wanted-4", false)
            ]
        );
        let steps = page.get_status(StatementStatus::VmStep);
        assert!(steps < 400, "page took {steps} SQLite VM operations");
    }
}
