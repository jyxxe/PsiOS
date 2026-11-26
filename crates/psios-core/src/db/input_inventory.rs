//! Bounded literal-reference inventory of immutable legacy Task inputs.
use crate::{Error, Result, decode, err, require};
use psios_api::{ResourceRef, canonical, sha256};
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, limits::Limit, types::ValueRef};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};

const MAX_INPUTS: usize = 16;
const MAX_SCHEMA_ENTRIES: usize = 512;
const MAX_TABLES: usize = 192;
const MAX_COLUMNS: usize = 128;
const MAX_ROWS: u64 = 262_144;
const MAX_CELLS: u64 = 4_194_304;
const MAX_NAME_BYTES: usize = 256;
const MAX_ROW_BYTES: u64 = 8 * 1024 * 1024;
const PREFIX: &str = "input-";
const ID_BYTES: usize = PREFIX.len() + 64;
const MANIFEST: &str = "TaskInputManifest";

#[derive(Serialize)]
struct Match {
    table: String,
    rows: u64,
}

#[derive(Serialize)]
struct Input {
    reference: ResourceRef,
    kind: String,
    owner_principal_id: String,
    stored_body_bytes: u64,
    references: Vec<Match>,
}

struct Table {
    name: String,
    columns: usize,
}

#[derive(Default, PartialEq, Eq, Serialize)]
struct Scan {
    tables: usize,
    rows: u64,
    bytes: u64,
    cells: u64,
}

fn lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sqlite_error(error: rusqlite::Error) -> Error {
    match error {
        rusqlite::Error::FromSqlConversionFailure(..)
        | rusqlite::Error::IntegralValueOutOfRange(..)
        | rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::InvalidColumnName(..)
        | rusqlite::Error::InvalidColumnIndex(..) => err(
            "RecoveryRequired",
            "inventory encountered invalid SQLite row data",
        ),
        _ => match error.sqlite_error_code() {
            Some(ErrorCode::TooBig) => err(
                "QuotaExceeded",
                "inventory SQLite row or value exceeds its byte limit",
            ),
            Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => {
                err("RecoveryRequired", "inventory SQLite data is invalid")
            }
            _ => err(
                "StorageUnavailable",
                "inventory could not read the SQLite copy",
            ),
        },
    }
}

fn selection(references: &Value) -> Result<Vec<(&Value, ResourceRef)>> {
    let refs = references
        .as_array()
        .filter(|refs| (1..=MAX_INPUTS).contains(&refs.len()))
        .ok_or_else(|| {
            err(
                "InvalidArgument",
                "provide 1 to 16 exact legacy input references",
            )
        })?;
    let mut seen = BTreeSet::new();
    let mut result = Vec::with_capacity(refs.len());
    for value in refs {
        let reference: ResourceRef = decode(value)?;
        require(
            reference.version == "r1"
                && reference
                    .resource_id
                    .strip_prefix(PREFIX)
                    .is_some_and(lower_sha256)
                && reference
                    .digest
                    .strip_prefix("sha256:")
                    .is_some_and(lower_sha256)
                && seen.insert(reference.resource_id.clone()),
            "InvalidArgument",
            "inventory requires distinct legacy input r1 references with SHA-256 digests",
        )?;
        result.push((value, reference));
    }
    Ok(result)
}

fn inputs(
    conn: &Connection,
    selected: Vec<(&Value, ResourceRef)>,
    max_bytes: u64,
) -> Result<Vec<Input>> {
    let mut candidate_bytes = 0u64;
    let mut result = Vec::with_capacity(selected.len());
    let mut lengths = conn
        .prepare("SELECT octet_length(body) FROM main.versions WHERE id=?1 AND rev=1")
        .map_err(sqlite_error)?;
    let mut versions = conn
        .prepare("SELECT * FROM main.versions WHERE id=?1 AND rev=1")
        .map_err(sqlite_error)?;
    for (value, reference) in selected {
        let size: i64 = lengths
            .query_row([&reference.resource_id], |row| row.get(0))
            .optional()
            .map_err(sqlite_error)?
            .ok_or_else(|| err("PreconditionFailed", "resource version unavailable"))?;
        let size = u64::try_from(size)
            .map_err(|_| err("RecoveryRequired", "invalid stored input body size"))?;
        candidate_bytes = within(candidate_bytes, size, max_bytes, "Text/Blob byte")?;
        let doc = versions
            .query_row([&reference.resource_id], super::row)
            .optional()
            .map_err(sqlite_error)?
            .ok_or_else(|| {
                err(
                    "RecoveryRequired",
                    "inventory input disappeared from the copy",
                )
            })?;
        require(
            doc.reference() == *value,
            "PreconditionFailed",
            "reference digest mismatch",
        )?;
        let material = match doc.kind.as_str() {
            "Input" => json!([doc.owner, doc.body]),
            MANIFEST => json!([doc.owner, doc.kind, doc.body]),
            _ => {
                return Err(err(
                    "RecoveryRequired",
                    "legacy input ID has the wrong resource kind",
                ));
            }
        };
        let id_digest = sha256(&canonical(&material));
        require(
            doc.rev == 1
                && doc.generation == 1
                && doc.spec == json!({})
                && reference.resource_id.strip_prefix(PREFIX) == id_digest.strip_prefix("sha256:"),
            "RecoveryRequired",
            "legacy input content does not match its original identity",
        )?;
        result.push(Input {
            reference,
            kind: doc.kind,
            owner_principal_id: doc.owner,
            stored_body_bytes: size,
            references: Vec::new(),
        });
    }
    Ok(result)
}

fn quoted(name: &str) -> Result<String> {
    require(
        !name.is_empty() && !name.contains('\0') && name.len() <= MAX_NAME_BYTES,
        "UnsupportedCapability",
        "inventory does not support this SQLite table identifier",
    )?;
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

fn tables(conn: &Connection) -> Result<Vec<Table>> {
    let mut stmt = conn
        .prepare("PRAGMA main.table_list")
        .map_err(sqlite_error)?;
    let mut rows = stmt.query([]).map_err(sqlite_error)?;
    let mut result = Vec::new();
    let mut entries = 0;
    while let Some(row) = rows.next().map_err(sqlite_error)? {
        entries += 1;
        require(
            entries <= MAX_SCHEMA_ENTRIES,
            "QuotaExceeded",
            "inventory schema entry limit exceeded",
        )?;
        let schema: String = row.get("schema").map_err(sqlite_error)?;
        let name: String = row.get("name").map_err(sqlite_error)?;
        let kind: String = row.get("type").map_err(sqlite_error)?;
        require(
            schema == "main",
            "RecoveryRequired",
            "inventory schema listing escaped main",
        )?;
        match kind.as_str() {
            "view" => continue,
            "table"
                if matches!(
                    name.as_str(),
                    "sqlite_schema"
                        | "sqlite_sequence"
                        | "sqlite_stat1"
                        | "sqlite_stat2"
                        | "sqlite_stat3"
                        | "sqlite_stat4"
                ) =>
            {
                continue;
            }
            "table" if !name.starts_with("sqlite_") => {}
            _ => {
                return Err(err(
                    "UnsupportedCapability",
                    "inventory requires ordinary SQLite tables and known internal objects",
                ));
            }
        }
        let columns: i64 = row.get("ncol").map_err(sqlite_error)?;
        let columns = usize::try_from(columns)
            .map_err(|_| err("RecoveryRequired", "invalid inventory column count"))?;
        require(
            (1..=MAX_COLUMNS).contains(&columns) && result.len() < MAX_TABLES,
            "QuotaExceeded",
            "inventory table or column limit exceeded",
        )?;
        result.push(Table { name, columns });
    }
    result.sort_unstable_by(|first, second| first.name.cmp(&second.name));
    require(
        result.iter().any(|table| table.name == "versions"),
        "RecoveryRequired",
        "inventory has no versions table",
    )?;
    Ok(result)
}

fn within(current: u64, additional: u64, limit: u64, subject: &str) -> Result<u64> {
    current
        .checked_add(additional)
        .filter(|total| *total <= limit)
        .ok_or_else(|| {
            err(
                "QuotaExceeded",
                format!("inventory {subject} scan limit exceeded"),
            )
        })
}

fn literal_hits(mut text: &[u8], ids: &HashMap<&[u8], u16>) -> u16 {
    let mut hits = 0;
    while let Some(offset) = text.iter().position(|byte| *byte == b'i') {
        text = &text[offset..];
        if text.len() < ID_BYTES {
            break;
        }
        if text.starts_with(PREFIX.as_bytes())
            && let Some(bit) = ids.get(&text[..ID_BYTES])
        {
            hits |= *bit;
        }
        text = &text[1..];
    }
    hits
}

fn own_hit(row: &Row<'_>, columns: (usize, usize), ids: &HashMap<&[u8], u16>) -> Result<u16> {
    let id = row.get_ref(columns.0).map_err(sqlite_error)?;
    let revision = row.get_ref(columns.1).map_err(sqlite_error)?;
    let (ValueRef::Text(id), ValueRef::Integer(revision)) = (id, revision) else {
        return Err(err(
            "RecoveryRequired",
            "versions has an invalid resource identity",
        ));
    };
    Ok(if revision == 1 {
        ids.get(id).copied().unwrap_or(0)
    } else {
        0
    })
}

fn row_sizes(conn: &Connection, table: &Table, scan: &mut Scan, max_bytes: u64) -> Result<()> {
    let name = quoted(&table.name)?;
    let mut info = conn
        .prepare(&format!("PRAGMA main.table_xinfo({name})"))
        .map_err(sqlite_error)?;
    let mut columns = info.query([]).map_err(sqlite_error)?;
    let mut sizes = Vec::with_capacity(table.columns);
    while let Some(column) = columns.next().map_err(sqlite_error)? {
        require(
            sizes.len() < table.columns,
            "RecoveryRequired",
            "inventory table shape changed",
        )?;
        require(
            column.get::<_, i64>("hidden").map_err(sqlite_error)? == 0,
            "UnsupportedCapability",
            "inventory does not support SQLite hidden or generated columns",
        )?;
        let column: String = column.get("name").map_err(sqlite_error)?;
        let column = quoted(&column)?;
        sizes.push(format!(
            "CASE typeof({column}) WHEN 'text' THEN octet_length({column}) \
             WHEN 'blob' THEN octet_length({column}) ELSE 0 END"
        ));
    }
    require(
        sizes.len() == table.columns,
        "RecoveryRequired",
        "inventory column count changed",
    )?;
    // SQLite can read typeof() and octet_length() directly from the stored
    // record metadata. A per-cell SQLite length cap alone does not cap the
    // combined size of the columns returned for a row.
    let sql = format!("SELECT {} FROM main.{name}", sizes.join(" + "));
    let mut lengths = conn.prepare(&sql).map_err(sqlite_error)?;
    let mut rows = lengths.query([]).map_err(sqlite_error)?;
    while let Some(row) = rows.next().map_err(sqlite_error)? {
        scan.rows = within(scan.rows, 1, MAX_ROWS, "row")?;
        scan.cells = within(scan.cells, table.columns as u64, MAX_CELLS, "cell")?;
        let size: i64 = row.get(0).map_err(sqlite_error)?;
        let size = u64::try_from(size)
            .map_err(|_| err("RecoveryRequired", "invalid inventory row byte count"))?;
        within(0, size, MAX_ROW_BYTES, "row byte")?;
        scan.bytes = within(scan.bytes, size, max_bytes, "Text/Blob byte")?;
    }
    scan.tables += 1;
    Ok(())
}

fn table_hits(
    conn: &Connection,
    table: &Table,
    ids: &HashMap<&[u8], u16>,
    scan: &mut Scan,
    max_bytes: u64,
) -> Result<[u64; MAX_INPUTS]> {
    let name = quoted(&table.name)?;
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM main.{name}"))
        .map_err(sqlite_error)?;
    require(
        stmt.column_count() == table.columns,
        "RecoveryRequired",
        "inventory row projection disagrees with its schema",
    )?;
    let version_identity = if table.name == "versions" {
        Some((
            stmt.column_index("id").map_err(sqlite_error)?,
            stmt.column_index("rev").map_err(sqlite_error)?,
        ))
    } else {
        None
    };
    let mut rows = stmt.query([]).map_err(sqlite_error)?;
    let mut hits_per_input = [0u64; MAX_INPUTS];
    while let Some(row) = rows.next().map_err(sqlite_error)? {
        scan.rows = within(scan.rows, 1, MAX_ROWS, "row")?;
        scan.cells = within(scan.cells, table.columns as u64, MAX_CELLS, "cell")?;
        let mut hits = 0;
        for index in 0..table.columns {
            if let ValueRef::Text(bytes) | ValueRef::Blob(bytes) =
                row.get_ref(index).map_err(sqlite_error)?
            {
                scan.bytes = within(scan.bytes, bytes.len() as u64, max_bytes, "Text/Blob byte")?;
                hits |= literal_hits(bytes, ids);
            }
        }
        if let Some(columns) = version_identity {
            hits &= !own_hit(row, columns, ids)?;
        }
        while hits != 0 {
            let index = hits.trailing_zeros() as usize;
            hits_per_input[index] += 1;
            hits &= hits - 1;
        }
    }
    scan.tables += 1;
    Ok(hits_per_input)
}

pub(super) fn scan(conn: &Connection, references: &Value, max_scanned_bytes: u64) -> Result<Value> {
    require(
        max_scanned_bytes > 0,
        "InvalidArgument",
        "inventory scan byte limit must be positive",
    )?;
    let selected = selection(references)?;
    // SQLite enforces the per-value limit before returning payloads. The
    // metadata pass also bounds each combined row and the cumulative bytes.
    let limit = Limit::SQLITE_LIMIT_LENGTH;
    let prior = conn.limit(limit).map_err(sqlite_error)?;
    let row_bytes = max_scanned_bytes.min(MAX_ROW_BYTES) as i32;
    conn.set_limit(limit, prior.min(row_bytes))
        .map_err(sqlite_error)?;
    let result = scan_bounded(conn, selected, max_scanned_bytes);
    conn.set_limit(limit, prior).map_err(sqlite_error)?;
    result
}

fn scan_bounded(
    conn: &Connection,
    selected: Vec<(&Value, ResourceRef)>,
    max_scanned_bytes: u64,
) -> Result<Value> {
    let encoding: String = conn
        .pragma_query_value(Some("main"), "encoding", |row| row.get(0))
        .map_err(sqlite_error)?;
    require(
        encoding == "UTF-8",
        "UnsupportedCapability",
        "inventory requires SQLite UTF-8 storage",
    )?;
    let tables = tables(conn)?;
    let mut expected = Scan::default();
    for table in &tables {
        row_sizes(conn, table, &mut expected, max_scanned_bytes)?;
    }
    let mut inputs = inputs(conn, selected, max_scanned_bytes)?;
    let mut scan = Scan::default();
    // Keep only one per-table count per candidate, regardless of occurrences,
    // columns or the number of historical rows that mention the same input.
    let identifiers: Vec<_> = inputs
        .iter()
        .map(|input| input.reference.resource_id.clone())
        .collect();
    let ids: HashMap<_, _> = identifiers
        .iter()
        .enumerate()
        .map(|(index, id)| (id.as_bytes(), 1u16 << index))
        .collect();
    for table in tables {
        let counts = table_hits(conn, &table, &ids, &mut scan, max_scanned_bytes)?;
        for (input, rows) in inputs.iter_mut().zip(counts) {
            if rows != 0 {
                input.references.push(Match {
                    table: table.name.clone(),
                    rows,
                });
            }
        }
    }
    require(
        scan == expected,
        "RecoveryRequired",
        "inventory copy changed between metadata and payload reads",
    )?;
    Ok(json!({"inputs":inputs,"scan":scan}))
}
