//! Preserve source-reported holes when copying into a newly created regular file.
use super::{copy_hash_checked, hash_stream, same_source};
use crate::{Result, require};
use rustix::{
    fs::{SeekFrom, seek},
    io::Errno,
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek},
    os::unix::fs::MetadataExt,
};

enum Span {
    Data(u64),
    Hole(u64),
}

fn next_span(file: &File, offset: u64, end: u64) -> io::Result<Span> {
    match seek(file, SeekFrom::Data(offset)) {
        Ok(next) if next > offset => return Ok(Span::Hole(next.min(end))),
        Ok(next) if next == offset => {}
        Err(Errno::NXIO) => return Ok(Span::Hole(end)),
        Err(Errno::INVAL | Errno::NOTSUP) => return Ok(Span::Data(end)),
        Err(error) => return Err(error.into()),
        _ => {
            return Err(io::Error::other(
                "filesystem returned an invalid data offset",
            ));
        }
    }
    match seek(file, SeekFrom::Hole(offset)) {
        Ok(next) if next > offset => Ok(Span::Data(next.min(end))),
        Err(Errno::INVAL | Errno::NOTSUP | Errno::NXIO) => Ok(Span::Data(end)),
        Err(error) => Err(error.into()),
        _ => Err(io::Error::other(
            "filesystem returned an invalid hole offset",
        )),
    }
}

pub(super) fn copy_to_new_file(
    source: &mut File,
    output: &mut File,
    max: u64,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<(u64, String)> {
    let before = source.metadata()?;
    let size = before.len();
    require(size <= max, "QuotaExceeded", "backup byte limit")?;
    // Fully reserved files can report unwritten preallocation as holes.
    // Keep that reservation, including when their contents are all zero.
    if before.blocks() >= size.div_ceil(512) {
        return copy_hash_checked(source, output, max, check);
    }
    let mut hash = Sha256::new();
    let mut offset = 0;
    while offset < size {
        offset = match next_span(source, offset, size)? {
            Span::Data(end) => {
                let bytes = end - offset;
                source.seek(io::SeekFrom::Start(offset))?;
                output.seek(io::SeekFrom::Start(offset))?;
                let copied = hash_stream(&mut source.take(bytes), output, bytes, &mut hash, check)?;
                require(
                    copied == bytes,
                    "Conflict",
                    "source file shrank during copy",
                )?;
                end
            }
            Span::Hole(end) => {
                let bytes = end - offset;
                hash_stream(
                    &mut io::repeat(0).take(bytes),
                    &mut io::sink(),
                    bytes,
                    &mut hash,
                    check,
                )?;
                end
            }
        };
    }
    // Data and hole positions came from separate queries on this descriptor.
    // A concurrent writer invalidates the extent map as well as the digest.
    let after = source.metadata()?;
    require(
        same_source(&before, &after) && before.blocks() == after.blocks(),
        "Conflict",
        "source file changed during copy",
    )?;
    output.set_len(size)?;
    Ok((size, format!("sha256:{:x}", hash.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileExt;

    #[test]
    fn changing_data_inside_a_reported_hole_invalidates_the_copy() {
        let mut source = tempfile::tempfile().unwrap();
        let size = 256 * 1024;
        source.set_len(size).unwrap();
        let writer = source.try_clone().unwrap();
        let mut destination = tempfile::tempfile().unwrap();
        let mut changed = false;
        let error = copy_to_new_file(&mut source, &mut destination, size, &mut || {
            if !changed {
                writer.write_all_at(b"changed", size / 2)?;
                changed = true;
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.0.code, "Conflict");
    }
}
