use crate::{Result, require};
use serde::{Deserialize, Serialize};

const PAGE_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Cursor {
    snapshot_digest: String,
    offset: usize,
}

pub(super) fn validate(cursor: Option<&Cursor>, limit: usize) -> Result<()> {
    require(
        (1..=128).contains(&limit),
        "InvalidArgument",
        "network page limit must be 1 to 128",
    )?;
    if let Some(cursor) = cursor {
        require(
            cursor.offset > 0
                && cursor
                    .snapshot_digest
                    .strip_prefix("sha256:")
                    .is_some_and(|s| {
                        s.len() == 64
                            && s.bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    }),
            "InvalidArgument",
            "invalid network page cursor",
        )?;
    }
    Ok(())
}

pub(super) fn page<'a, T: Serialize>(
    items: &'a [T],
    cursor: Option<&Cursor>,
    limit: usize,
    digest: &str,
) -> Result<(&'a [T], Option<Cursor>)> {
    let offset = if let Some(cursor) = cursor {
        require(
            cursor.snapshot_digest == digest,
            "ResyncRequired",
            "network observation changed or cursor scope differs",
        )?;
        require(
            cursor.offset < items.len(),
            "InvalidArgument",
            "network cursor is past the end",
        )?;
        cursor.offset
    } else {
        0
    };
    let mut size = 0;
    let mut count = 0;
    for entry in items[offset..].iter().take(limit) {
        let len = serde_json::to_vec(entry)?.len() + 1;
        if size + len > PAGE_BYTES {
            require(
                count > 0,
                "QuotaExceeded",
                "one network entry exceeds the result page byte limit",
            )?;
            break;
        }
        size += len;
        count += 1;
    }
    let end = offset + count;
    let next = (end < items.len()).then(|| Cursor {
        snapshot_digest: digest.into(),
        offset: end,
    });
    Ok((&items[offset..end], next))
}
