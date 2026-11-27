//! Check decoded layer bytes before allowing a digest-addressed gzip layer to load.
use super::{HashOutput, Result, check_inner_layer, invalid, require};
use flate2::bufread::GzDecoder;
use rustix::fs::{MemfdFlags, memfd_create};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek},
};

pub(super) fn verify_layer(input: impl Read, limit: u64, diff_id: &str) -> Result<u64> {
    let mut layer = File::from(
        memfd_create("psios-oci-gzip-layer", MemfdFlags::CLOEXEC).map_err(io::Error::from)?,
    );
    let mut decoder = GzDecoder::new(BufReader::new(input));
    let mut output = HashOutput::new(&mut layer);
    let size = io::copy(&mut decoder.by_ref().take(limit + 1), &mut output).map_err(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
        ) {
            invalid("OCI gzip layer is malformed")
        } else {
            error.into()
        }
    })?;
    require(
        size <= limit,
        "QuotaExceeded",
        "inflated OCI layer chain exceeds the byte limit",
    )?;
    require(
        decoder.into_inner().fill_buf()?.is_empty(),
        "PreconditionFailed",
        "OCI gzip layer contains trailing bytes or multiple members",
    )?;
    require(
        output.finish() == diff_id,
        "PreconditionFailed",
        "OCI gzip layer disagrees with its inspected DiffID",
    )?;
    layer.rewind()?;
    check_inner_layer(&mut layer, size)?;
    Ok(size)
}
