//! Bounded Linux user attributes for a backup or a verified detached volume.
use crate::{Result, err, require};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustix::fs::{self, XattrFlags};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs::File};

pub(crate) const FORMAT: u32 = crate::apps::restore_archive::ZSTD_FORMAT + 1;
pub(super) const MAX_TREE_BYTES: u64 = 1024 * 1024;
const MAX_ATTRIBUTES: usize = 16;
const MAX_NAME_BYTES: usize = 255;
const MAX_VALUE_BYTES: usize = 16 * 1024;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Attribute {
    name: String,
    value_base64: String,
}

#[derive(Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub(super) struct Attributes(Vec<Attribute>);

fn valid_name(name: &str) -> bool {
    name.len() <= MAX_NAME_BYTES
        && name
            .strip_prefix("user.")
            .is_some_and(|suffix| !suffix.is_empty())
        && !name.chars().any(char::is_control)
}

impl Attribute {
    fn value(&self) -> Result<Vec<u8>> {
        require(
            valid_name(&self.name) && self.value_base64.len() <= 4 * MAX_VALUE_BYTES.div_ceil(3),
            "PreconditionFailed",
            "invalid backup user attribute name or value length",
        )?;
        let value = STANDARD
            .decode(&self.value_base64)
            .map_err(|_| err("PreconditionFailed", "invalid backup user attribute data"))?;
        require(
            value.len() <= MAX_VALUE_BYTES && STANDARD.encode(&value) == self.value_base64,
            "PreconditionFailed",
            "backup user attribute data is outside the canonical profile",
        )?;
        Ok(value)
    }
}

impl Attributes {
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn read(file: &File, failure: &str) -> Result<Self> {
        let list_size = fs::flistxattr_query(file)
            .map_err(|_| err(failure, "cannot list Linux extended attributes"))?;
        if list_size == 0 {
            return Ok(Self::default());
        }
        require(
            list_size <= MAX_ATTRIBUTES * (MAX_NAME_BYTES + 1),
            "QuotaExceeded",
            "backup extended attribute list exceeds its limit",
        )?;
        let mut raw = vec![0; list_size];
        let actual = fs::flistxattr(file, raw.as_mut_slice())
            .map_err(|_| err(failure, "extended attribute names changed during backup"))?;
        require(
            actual == list_size && raw.last() == Some(&0),
            failure,
            "extended attribute names changed during backup",
        )?;
        let names = std::str::from_utf8(&raw[..list_size - 1])
            .map_err(|_| err(failure, "backup requires UTF-8 extended attribute names"))?;
        let mut attributes = Vec::new();
        for name in names.split('\0') {
            require(
                valid_name(name),
                failure,
                "backup only supports user extended attributes",
            )?;
            require(
                attributes.len() < MAX_ATTRIBUTES,
                "QuotaExceeded",
                "backup extended attribute count exceeds its limit",
            )?;
            let size = fs::fgetxattr_query(file, name)
                .map_err(|_| err(failure, "extended attribute changed during backup"))?;
            require(
                size <= MAX_VALUE_BYTES,
                "QuotaExceeded",
                "backup extended attribute value exceeds its limit",
            )?;
            let mut data = vec![0; size.max(1)];
            let actual = fs::fgetxattr(file, name, data.as_mut_slice())
                .map_err(|_| err(failure, "extended attribute changed during backup"))?;
            require(
                actual == size,
                failure,
                "extended attribute changed during backup",
            )?;
            attributes.push(Attribute {
                name: name.into(),
                value_base64: STANDARD.encode(&data[..size]),
            });
        }
        attributes.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let attributes = Self(attributes);
        attributes.validate()?;
        Ok(attributes)
    }

    pub(super) fn validate(&self) -> Result<()> {
        require(
            self.0.len() <= MAX_ATTRIBUTES
                && self.0.windows(2).all(|pair| pair[0].name < pair[1].name),
            "PreconditionFailed",
            "backup user attributes must be unique and sorted",
        )?;
        for attribute in &self.0 {
            attribute.value()?;
        }
        Ok(())
    }

    fn encoded_bound(&self) -> u64 {
        self.0
            .iter()
            .map(|attribute| (2 * attribute.name.len() + attribute.value_base64.len() + 48) as u64)
            .sum()
    }

    pub(super) fn install(&self, file: &File) -> Result<()> {
        require(
            fs::flistxattr_query(file).is_ok_and(|size| size == 0),
            "PreconditionFailed",
            "backup attribute destination already contains extended attributes",
        )?;
        for attribute in &self.0 {
            fs::fsetxattr(
                file,
                attribute.name.as_str(),
                &attribute.value()?,
                XattrFlags::CREATE,
            )
            .map_err(std::io::Error::from)?;
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Tree {
    pub root: Attributes,
    members: Vec<Member>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Member {
    path: String,
    attributes: Attributes,
}

impl Tree {
    pub(super) fn new(root: Attributes) -> Self {
        Self {
            root,
            members: Vec::new(),
        }
    }

    pub(super) fn add(&mut self, path: String, attributes: Attributes) {
        if !attributes.is_empty() {
            self.members.push(Member { path, attributes });
        }
    }

    pub(super) fn sort(&mut self) {
        self.members.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    }

    pub(super) fn at(&self, path: &str) -> Option<&Attributes> {
        self.members
            .binary_search_by(|member| member.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.members[index].attributes)
    }

    pub(super) fn install_member(&self, path: &str, file: &File) -> Result<()> {
        self.at(path)
            .unwrap_or(&Attributes::default())
            .install(file)
    }

    pub(super) fn digest(&self) -> Result<String> {
        Ok(psios_api::sha256(&psios_api::canonical(
            &serde_json::to_value(self)?,
        )))
    }

    pub(super) fn member_bound(path: &str, attributes: &Attributes) -> u64 {
        if attributes.is_empty() {
            0
        } else {
            2 * path.len() as u64 + attributes.encoded_bound() + 48
        }
    }

    pub(super) fn root_bound(&self) -> u64 {
        self.root.encoded_bound() + 64
    }

    pub(super) fn validate(&self, paths: &BTreeMap<&str, bool>) -> Result<u64> {
        require(
            self.members.len() <= paths.len()
                && self
                    .members
                    .windows(2)
                    .all(|pair| pair[0].path < pair[1].path),
            "PreconditionFailed",
            "backup attribute paths must be unique and sorted",
        )?;
        let mut bytes = self.root_bound();
        self.root.validate()?;
        for member in &self.members {
            require(
                paths.contains_key(member.path.as_str()) && !member.attributes.is_empty(),
                "PreconditionFailed",
                "backup user attributes refer to an invalid member",
            )?;
            bytes += Self::member_bound(&member.path, &member.attributes);
            require(
                bytes <= MAX_TREE_BYTES,
                "QuotaExceeded",
                "backup user attribute metadata exceeds its limit",
            )?;
            member.attributes.validate()?;
        }
        Ok(bytes)
    }
}
