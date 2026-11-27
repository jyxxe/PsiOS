//! Paged binary data operations relative to a checked volume directory descriptor.
use super::*;
use crate::host::Target;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::io::{Seek, SeekFrom};
const CHUNK: usize = 96 * 1024;
const FILE_MAX: u64 = 256 * 1024 * 1024;
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
    pub limit: Option<usize>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadChunk {
    pub target: Target,
    pub path: String,
    pub offset: u64,
    pub limit: usize,
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
            )
        }
        Action::MakeDir(p) | Action::RemoveEntry(p) => path(&p.path, false),
        Action::Chunk(p) => {
            path(&p.path, false)?;
            require(
                (1..=CHUNK).contains(&p.limit),
                "InvalidArgument",
                "invalid read chunk size",
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
fn list(root: &File, p: &Page) -> Result<Value> {
    let dir = if p.path.is_empty() {
        root.try_clone()?
    } else {
        child(root, &p.path, OFlags::RDONLY | OFlags::DIRECTORY)?
    };
    let limit = p.limit.unwrap_or(32);
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
        if matches!(name, "." | "..") || p.after.as_deref().is_some_and(|a| name <= a) {
            continue;
        }
        selected.insert(name.to_owned());
        if selected.len() > limit + 1 {
            selected.pop_last();
        }
    }
    let more = selected.len() > limit;
    if more {
        selected.pop_last();
    }
    let cursor = more.then(|| selected.last().unwrap().clone());
    let mut entries = Vec::new();
    for name in selected {
        let st =
            statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
        let kind = match FileType::from_raw_mode(st.st_mode) {
            FileType::RegularFile => "file",
            FileType::Directory => "directory",
            _ => "unsupported",
        };
        entries.push(json!({"name":name,"kind":kind,"bytes":st.st_size.to_string()}));
    }
    Ok(
        json!({"entries":entries,"next_after":cursor,"volume_version":p.target.expected_resource_version}),
    )
}
fn read(root: &File, p: &ReadChunk) -> Result<Value> {
    let mut f = child(root, &p.path, OFlags::RDONLY)?;
    let m = f.metadata()?;
    regular(&m)?;
    require(p.offset <= m.len(), "PreconditionFailed", "read beyond end")?;
    f.seek(SeekFrom::Start(p.offset))?;
    let mut b = Vec::new();
    f.take(p.limit as u64).read_to_end(&mut b)?;
    let end = p.offset + b.len() as u64;
    Ok(
        json!({"path":p.path,"offset":p.offset,"next_offset":end,"total_bytes":m.len(),"eof":end==m.len(),
              "data_base64":STANDARD.encode(&b),"digest":sha256(&b)}),
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
