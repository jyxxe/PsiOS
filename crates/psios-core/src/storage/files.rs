//! Paged binary data operations relative to a checked volume directory descriptor.
use super::*;
use crate::host::Target;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::io::{Seek, SeekFrom};
const CHUNK: usize = 96 * 1024;
const FILE_MAX: u64 = 256 * 1024 * 1024;
const MIN_PAGE_BYTES: usize = 4 * 1024;
const MAX_PAGE_BYTES: usize = 128 * 1024;
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathInput {
    pub target: Target,
    pub path: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub target: Target,
    #[serde(default)]
    pub path: String,
    pub after: Option<String>,
    pub expected_directory_revision: Option<String>,
    pub limit: Option<usize>,
    pub max_result_bytes: Option<usize>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadChunk {
    pub target: Target,
    pub path: String,
    pub offset: u64,
    pub limit: usize,
    pub expected_file_revision: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Append {
    pub target: Target,
    pub path: String,
    pub offset: u64,
    pub data_base64: String,
}
impl Append {
    pub(super) fn bytes(&self) -> Result<Vec<u8>> {
        require(
            !self.data_base64.is_empty() && self.data_base64.len() <= CHUNK.div_ceil(3) * 4,
            "QuotaExceeded",
            "chunk outside size limit",
        )?;
        let bytes = STANDARD
            .decode(&self.data_base64)
            .map_err(|_| err("InvalidArgument", "invalid base64 chunk"))?;
        require(
            self.offset
                .checked_add(bytes.len() as u64)
                .is_some_and(|end| end <= FILE_MAX),
            "QuotaExceeded",
            "file exceeds local volume profile",
        )?;
        Ok(bytes)
    }
}
pub(super) fn path(s: &str, root: bool) -> Result<()> {
    if root && s.is_empty() {
        return Ok(());
    }
    require(
        s.len() <= 2048 && s.split('/').count() <= 32,
        "InvalidArgument",
        "volume path exceeds profile",
    )?;
    for component in s.split('/') {
        filename(component)?;
    }
    Ok(())
}
pub fn validate(a: &Action) -> Result<()> {
    match a {
        Action::Files(p) => {
            path(&p.path, true)?;
            require(
                p.expected_directory_revision
                    .as_deref()
                    .is_none_or(psios_api::valid_volume_entry_revision),
                "InvalidArgument",
                "invalid volume directory revision",
            )?;
            if let Some(after) = &p.after {
                // The cursor is a name from readdir and is used only for ordering.
                require(
                    !matches!(after.as_str(), "" | "." | "..") && !after.contains(['/', '\0']),
                    "InvalidArgument",
                    "invalid directory page cursor",
                )?;
            }
            require(
                (1..=128).contains(&p.limit.unwrap_or(32)),
                "InvalidArgument",
                "invalid directory page size",
            )?;
            require(
                p.max_result_bytes
                    .is_none_or(|limit| (MIN_PAGE_BYTES..=MAX_PAGE_BYTES).contains(&limit)),
                "InvalidArgument",
                "invalid directory result byte limit",
            )
        }
        Action::MakeDir(p) | Action::RemoveEntry(p) => path(&p.path, false),
        Action::Chunk(p) => {
            path(&p.path, false)?;
            require(
                (1..=psios_api::MAX_VOLUME_FILE_READ_CHUNK_BYTES).contains(&p.limit),
                "InvalidArgument",
                "invalid read chunk size",
            )?;
            require(
                p.expected_file_revision
                    .as_deref()
                    .is_none_or(psios_api::VolumeFileChunkQuery::valid_file_revision),
                "InvalidArgument",
                "invalid volume file revision",
            )
        }
        Action::Append(p) => {
            path(&p.path, false)?;
            p.bytes()?;
            Ok(())
        }
        _ => Ok(()),
    }
}
pub(super) fn parent(root: &File, path: &str) -> Result<(File, String)> {
    let (dir, leaf) = path.rsplit_once('/').unwrap_or(("", path));
    let dir = if dir.is_empty() {
        root.try_clone()?
    } else {
        child(root, dir, OFlags::RDONLY | OFlags::DIRECTORY)?
    };
    Ok((dir, leaf.to_owned()))
}
fn open_directory(root: &File, path: &str) -> Result<File> {
    if path.is_empty() {
        Ok(root.try_clone()?)
    } else {
        child(root, path, OFlags::RDONLY | OFlags::DIRECTORY)
    }
}

fn list(root: &File, p: &Page) -> Result<Value> {
    let dir = open_directory(root, &p.path)?;
    let directory_before = dir.metadata()?;
    let limit = p.limit.unwrap_or(32);
    let mut membership = Sha256::new();
    membership.update(canonical(&json!([
        "psios/volume-directory-revision/1",
        p.target.id,
        p.target.expected_resource_version,
        p.path,
        directory_before.dev(),
        directory_before.ino()
    ])));
    let mut selected = std::collections::BTreeSet::<String>::new();
    for (i, entry) in Dir::read_from(&dir)
        .map_err(std::io::Error::from)?
        .enumerate()
    {
        require(i < 65538, "QuotaExceeded", "directory scan limit")?;
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| err("UnsupportedCapability", "non-UTF8 filename"))?;
        if matches!(name, "." | "..") {
            continue;
        }
        // Include names on both sides of the cursor. Linux names cannot contain NUL.
        membership.update([0]);
        membership.update(name.as_bytes());
        if p.after.as_deref().is_some_and(|a| name <= a) {
            continue;
        }
        selected.insert(name.to_owned());
        if selected.len() > limit + 1 {
            selected.pop_last();
        }
    }
    let revision = format!("sha256:{:x}", membership.finalize());
    require(
        p.expected_directory_revision
            .as_ref()
            .is_none_or(|expected| *expected == revision),
        "Conflict",
        "volume directory changed between pages",
    )?;
    let mut more = selected.len() > limit;
    let mut entries = Vec::new();
    let mut last = None;
    let mut page = json!({
        "entries": [], "next_after": null, "volume_version": p.target.expected_resource_version,
        "directory_revision": revision,
    });
    let mut encoded = p.max_result_bytes.map(|_| page.to_string().len());
    for name in selected.into_iter().take(limit) {
        // Do not collect metadata through a mount or follow a link out of the volume.
        let meta = child(&dir, name.as_str(), OFlags::PATH)?.metadata()?;
        let (kind, bytes) = if meta.is_file() && meta.nlink() == 1 {
            ("file", meta.len())
        } else if meta.is_dir() {
            ("directory", meta.len())
        } else {
            ("unsupported", 0)
        };
        let entry = json!({"name":name,"kind":kind,"bytes":bytes.to_string()});
        if let (Some(encoded), Some(cap)) = (&mut encoded, p.max_result_bytes) {
            let entry_bytes = entry.to_string().len() + 1;
            // Reserve an escaped cursor for this entry in case the next row does not fit.
            if *encoded + entry_bytes + entry["name"].to_string().len() > cap {
                require(
                    !entries.is_empty(),
                    "QuotaExceeded",
                    "directory entry exceeds page byte limit",
                )?;
                more = true;
                break;
            }
            *encoded += entry_bytes;
        }
        last = Some(name);
        entries.push(entry);
    }
    let current = open_directory(root, &p.path)?;
    require(
        same_source(&directory_before, &current.metadata()?),
        "Conflict",
        "directory changed while the page was being read",
    )?;
    page["entries"] = Value::Array(entries);
    if more {
        page["next_after"] = json!(last);
    }
    Ok(page)
}
fn read(root: &File, p: &ReadChunk) -> Result<Value> {
    let mut f = child(root, &p.path, OFlags::RDONLY)?;
    let m = f.metadata()?;
    regular(&m)?;
    let revision = sha256(&canonical(&json!([
        "psios/volume-file-revision/1",
        p.path,
        m.dev(),
        m.ino(),
        m.mode(),
        m.uid(),
        m.gid(),
        m.nlink(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec()
    ])));
    require(
        p.expected_file_revision
            .as_ref()
            .is_none_or(|expected| *expected == revision),
        "Conflict",
        "volume file revision changed",
    )?;
    require(p.offset <= m.len(), "PreconditionFailed", "read beyond end")?;
    f.seek(SeekFrom::Start(p.offset))?;
    let mut b = Vec::new();
    (&mut f).take(p.limit as u64).read_to_end(&mut b)?;
    let current = child(root, &p.path, OFlags::PATH)?;
    require(
        same_source(&m, &f.metadata()?) && same_source(&m, &current.metadata()?),
        "Conflict",
        "volume file changed during chunk read",
    )?;
    let end = p.offset + b.len() as u64;
    Ok(
        json!({"path":p.path,"offset":p.offset,"next_offset":end,"total_bytes":m.len(),"eof":end==m.len(),
              "data_base64":STANDARD.encode(&b),"digest":sha256(&b),
              "volume_version":p.target.expected_resource_version,"file_revision":revision}),
    )
}
fn append(config: &Config, root: &File, p: &Append) -> Result<Value> {
    let bytes = p.bytes()?;
    let (dir, name) = parent(root, &p.path)?;
    let mut f = if p.offset == 0 {
        config.check_managed_space(&dir, std::iter::once(bytes.len() as u64), 1)?;
        let fd = openat2(
            &dir,
            name.as_str(),
            OFlags::WRONLY | OFlags::CLOEXEC | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(|e| {
            if e == rustix::io::Errno::EXIST {
                err("Conflict", "destination already exists")
            } else {
                std::io::Error::from(e).into()
            }
        })?;
        File::from(fd)
    } else {
        let f = child(&dir, &name, OFlags::WRONLY | OFlags::APPEND)?;
        let m = f.metadata()?;
        regular(&m)?;
        require(
            m.len() == p.offset,
            "Conflict",
            "append offset does not match current length",
        )?;
        config.check_managed_space(&f, std::iter::once(bytes.len() as u64), 0)?;
        f
    };
    f.write_all(&bytes).map_err(unknown)?;
    f.sync_all().map_err(unknown)?;
    dir.sync_all().map_err(unknown)?;
    Ok(
        json!({"path":p.path,"offset":p.offset,"next_offset":p.offset+bytes.len() as u64,"digest":sha256(&bytes)}),
    )
}
pub fn perform(w: &Work) -> Result<Value> {
    let root = File::open(
        w.data
            .as_ref()
            .ok_or_else(|| err("PreconditionFailed", "volume not mounted"))?,
    )?;
    registered_root(&w.target, &root.metadata()?)?;
    match &w.action {
        Action::Files(p) => list(&root, p),
        Action::Chunk(p) => read(&root, p),
        Action::Append(p) => append(&w.cfg, &root, p),
        _ => Err(err("UnsupportedCapability", "unknown file operation")),
    }
}
