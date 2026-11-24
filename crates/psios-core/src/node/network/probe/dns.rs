use super::{dial, transport_error};
use crate::config::DnsProbeConfig;
use crate::{Result, err, require};
use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{DNSClass, Name, RData, RecordType},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashSet},
    io,
    net::IpAddr,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_REPLY_BYTES: usize = 8 * 1024;
const MAX_ALIASES: usize = 16;

pub(super) fn name(value: &str) -> Result<Name> {
    let domain = value.strip_suffix('.').unwrap_or_default();
    require(
        !domain.is_empty()
            && domain.len() <= 253
            && domain.split('.').all(|label| {
                label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                    && label
                        .as_bytes()
                        .last()
                        .is_some_and(u8::is_ascii_alphanumeric)
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            }),
        "InvalidArgument",
        "network probe DNS name must be an absolute ASCII hostname",
    )?;
    Name::from_ascii(value).map_err(|_| err("InvalidArgument", "invalid network probe DNS name"))
}

async fn exchange(cfg: &DnsProbeConfig, message: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut stream = dial(cfg.resolver).await?;
    stream.write_u16(message.len() as u16).await?;
    stream.write_all(message).await?;
    let size = usize::from(stream.read_u16().await?);
    if !(12..=MAX_REPLY_BYTES).contains(&size) {
        return Ok(None);
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    Ok(Some(bytes))
}

fn addresses(
    response: &Message,
    query: &Query,
    ipv4: bool,
) -> Option<(Vec<String>, BTreeSet<IpAddr>)> {
    let mut aliases = Vec::new();
    let mut seen = HashSet::new();
    let mut current = query.name();
    loop {
        if !seen.insert(current) || aliases.len() > MAX_ALIASES {
            return None;
        }
        let mut alias = None;
        let mut found = BTreeSet::new();
        for answer in response
            .answers
            .iter()
            .filter(|r| r.dns_class == DNSClass::IN && &r.name == current)
        {
            match &answer.data {
                RData::CNAME(name) => {
                    if alias.is_some_and(|known| known != &name.0) {
                        return None;
                    }
                    alias = Some(&name.0);
                }
                RData::A(record) if ipv4 => {
                    found.insert(IpAddr::V4(record.0));
                }
                RData::AAAA(record) if !ipv4 => {
                    found.insert(IpAddr::V6(record.0));
                }
                _ => {}
            }
        }
        let Some(next) = alias else {
            return Some((aliases, found));
        };
        if !found.is_empty() {
            return None;
        }
        aliases.push(next.to_ascii());
        current = next;
    }
}

fn verify(bytes: &[u8], id: u16, query: &Query, endpoint: IpAddr) -> Value {
    let invalid = || json!({"status":"InvalidResponse"});
    let Ok(response) = Message::from_vec(bytes) else {
        return invalid();
    };
    let metadata = response.metadata;
    if metadata.id != id
        || metadata.message_type != MessageType::Response
        || metadata.op_code != OpCode::Query
        || metadata.truncation
        || response.queries.as_slice() != std::slice::from_ref(query)
    {
        return invalid();
    }
    let code = u16::from(metadata.response_code);
    if metadata.response_code != ResponseCode::NoError {
        return json!({"status":"ResponseError","response_code":code});
    }
    let Some((aliases, addresses)) = addresses(&response, query, endpoint.is_ipv4()) else {
        return json!({"status":"InvalidResponse","response_code":code});
    };
    let status = if addresses.is_empty() {
        "NoAddress"
    } else if addresses.len() == 1 && addresses.contains(&endpoint) {
        "Verified"
    } else {
        "AddressMismatch"
    };
    json!({"status":status,"response_code":code,"addresses":addresses,"aliases":aliases})
}

pub(super) async fn observe(cfg: &DnsProbeConfig, endpoint: IpAddr) -> Value {
    let Ok(name) = name(&cfg.name) else {
        return json!({"status":"InvalidConfiguration"});
    };
    let record_type = if endpoint.is_ipv4() {
        RecordType::A
    } else {
        RecordType::AAAA
    };
    let query = Query::query(name, record_type);
    let mut request = Message::query();
    request.metadata.recursion_desired = true;
    request.add_query(query.clone());
    let Ok(wire) = request.to_vec() else {
        return json!({"status":"InvalidConfiguration"});
    };
    match exchange(cfg, &wire).await {
        Ok(Some(reply)) => verify(&reply, request.metadata.id, &query, endpoint),
        Ok(None) => json!({"status":"InvalidResponse"}),
        Err(error) => transport_error(&error),
    }
}
