//! Verify one bounded compressed OCI layer before preserving its original encoded bytes.
use super::{HashOutput, Result, check_inner_layer, invalid, require};
use flate2::bufread::GzDecoder;
use rustix::fs::{MemfdFlags, memfd_create};
use std::{
    fs::File,
    io::{self, BufRead, BufReader, Cursor, Read, Seek},
};

const ZSTD_WINDOW_LOG_MAX: u32 = 23;
const ZSTD_MAGIC: [u8; 4] = *b"\x28\xb5\x2f\xfd";

pub(super) fn gzip(input: impl Read, limit: u64, diff_id: &str) -> Result<u64> {
    let mut decoder = GzDecoder::new(BufReader::new(input));
    let size = verify_decoded(&mut decoder, limit, diff_id)?;
    require(
        decoder.into_inner().fill_buf()?.is_empty(),
        "PreconditionFailed",
        "OCI gzip layer contains trailing bytes or multiple members",
    )?;
    Ok(size)
}

pub(super) fn zstd(mut input: impl Read, limit: u64, diff_id: &str) -> Result<u64> {
    let mut magic = [0; ZSTD_MAGIC.len()];
    input.read_exact(&mut magic).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            invalid("OCI zstd layer has a truncated frame header")
        } else {
            error.into()
        }
    })?;
    require(
        magic == ZSTD_MAGIC,
        "PreconditionFailed",
        "OCI zstd layer must begin with a standard frame",
    )?;
    let mut decoder =
        zstd::stream::read::Decoder::new(Cursor::new(magic).chain(input))?.single_frame();
    decoder.window_log_max(ZSTD_WINDOW_LOG_MAX)?;
    let size = verify_decoded(&mut decoder, limit, diff_id)?;
    require(
        decoder.finish().fill_buf()?.is_empty(),
        "PreconditionFailed",
        "OCI zstd layer contains trailing bytes or multiple frames",
    )?;
    Ok(size)
}

fn verify_decoded(decoder: &mut impl Read, limit: u64, diff_id: &str) -> Result<u64> {
    let mut layer = File::from(
        memfd_create("psios-oci-decoded-layer", MemfdFlags::CLOEXEC).map_err(io::Error::from)?,
    );
    let mut output = HashOutput::new(&mut layer);
    let size = io::copy(&mut decoder.take(limit + 1), &mut output).map_err(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::InvalidInput
                | io::ErrorKind::InvalidData
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::Other
        ) {
            invalid("compressed OCI layer is malformed or exceeds the decoder window")
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
        output.finish() == diff_id,
        "PreconditionFailed",
        "decoded OCI layer disagrees with its inspected DiffID",
    )?;
    layer.rewind()?;
    check_inner_layer(&mut layer, size)?;
    Ok(size)
}
