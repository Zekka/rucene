use core::store::{BufferedChecksumIndexInput, ChecksumIndexInput};
use core::store::{DataInput, DataOutput, IndexInput};

use core::util::string_util::id2str;
use core::util::string_util::ID_LENGTH;
use error::ErrorKind::{CorruptIndex, IllegalArgument};
use error::Result;
use std::io::Read;

pub const CODEC_MAGIC: i32 = 0x3FD7_6C17;
pub const FOOTER_MAGIC: i32 = !CODEC_MAGIC;

pub fn write_header<T: DataOutput + ?Sized>(out: &mut T, codec: &str, version: i32) -> Result<()> {
    let clen = codec.len();
    if clen >= 128 {
        bail!(IllegalArgument(format!(
            "codec must be simple ASCII less than 128 characters, got {}[length={}]",
            codec, clen,
        )));
    }
    out.write_int(CODEC_MAGIC)?;
    out.write_string(codec)?;
    out.write_int(version)
}

pub fn write_index_header<T: DataOutput + ?Sized>(
    out: &mut T,
    codec: &str,
    version: i32,
    id: &[u8],
    suffix: &str,
) -> Result<()> {
    if id.len() != ID_LENGTH {
        bail!(IllegalArgument(format!("Invalid id: {:?}", id)));
    }
    write_header(out, codec, version)?;
    out.write_bytes(id, 0, id.len())?;
    let slen = suffix.len() as usize;

    if slen >= 256 {
        bail!(IllegalArgument(format!(
            "suffix must be simple ASCII less than 256 characters, got {}[length={}]",
            suffix, slen
        )));
    }
    out.write_byte(slen as u8)?;
    out.write_bytes(&suffix.as_bytes(), 0, slen)
}

fn header_length(codec: &str) -> usize {
    9 + codec.len()
}

pub fn index_header_length(codec: &str, suffix: &str) -> usize {
    header_length(codec) + ID_LENGTH + 1 + suffix.len()
}

pub fn check_header<T: DataInput + ?Sized>(
    data_input: &mut T,
    codec: &str,
    min_ver: i32,
    max_ver: i32,
) -> Result<i32> {
    let actual_header = data_input.read_int()?;
    if actual_header != CODEC_MAGIC {
        bail!(CorruptIndex(format!(
            "codec header mismatch: actual=0x{:X}, expected=0x{:X}",
            actual_header, CODEC_MAGIC
        )));
    }
    check_header_no_magic(data_input, codec, min_ver, max_ver)
}

pub fn check_header_no_magic<T: DataInput + ?Sized>(
    data_input: &mut T,
    codec: &str,
    min_ver: i32,
    max_ver: i32,
) -> Result<i32> {
    let actual_codec = data_input.read_string()?;
    if actual_codec != codec {
        bail!(CorruptIndex(format!(
            "codec mismatch: actual={}, expected={}",
            actual_codec, codec
        )));
    }
    let actual_ver = data_input.read_int()?;
    if actual_ver < min_ver || actual_ver > max_ver {
        bail!(CorruptIndex(format!(
            "index format either too new or too old: {} <= {} <= {} doesn't hold",
            min_ver, actual_ver, max_ver
        )));
    }
    Ok(actual_ver)
}

pub fn check_index_header<T: DataInput + ?Sized>(
    data_input: &mut T,
    codec: &str,
    min_ver: i32,
    max_ver: i32,
    expected_id: &[u8],
    expected_suffix: &str,
) -> Result<i32> {
    let version = check_header(data_input, codec, min_ver, max_ver)?;
    check_index_header_id(data_input, expected_id)?;
    check_index_header_suffix(data_input, expected_suffix)?;
    Ok(version)
}

fn check_index_header_id<T: DataInput + ?Sized>(
    data_input: &mut T,
    expected_id: &[u8],
) -> Result<()> {
    let mut actual_id = [0u8; ID_LENGTH];
    data_input.read_bytes(&mut actual_id, 0, ID_LENGTH)?;
    if actual_id != expected_id {
        bail!(CorruptIndex(format!(
            "file mismatch, expected_id={}, got={}",
            id2str(expected_id),
            id2str(&actual_id)
        )));
    }
    Ok(())
}

pub fn check_index_header_suffix<T: DataInput + ?Sized>(
    data_input: &mut T,
    expected_suffix: &str,
) -> Result<()> {
    let suffix_len = data_input.read_byte()? as usize;
    let mut suffix_bytes = vec![0u8; suffix_len];
    data_input.read_bytes(&mut suffix_bytes, 0, suffix_len)?;
    let suffix = ::std::str::from_utf8(&suffix_bytes)?;
    if suffix != expected_suffix {
        bail!(CorruptIndex(format!(
            "file mismatch, expected suffix={}, got={}",
            expected_suffix, suffix
        )));
    }
    Ok(())
}

#[inline(always)]
pub fn footer_length() -> usize {
    16
}

pub fn validate_footer<T: IndexInput + ?Sized>(input: &mut T) -> Result<()> {
    let remaining = input.len() as i64 - input.file_pointer();
    let expected = footer_length() as i64;

    if remaining < expected {
        bail!(CorruptIndex(format!(
            "misplaced codec footer (file truncated?): remaining={}, expected={}",
            remaining, expected
        )))
    } else if remaining > expected {
        bail!(CorruptIndex(format!(
            "misplaced codec footer (file extended?): remaining={}, expected={}",
            remaining, expected
        )))
    } else {
        let magic = input.read_int()?;
        if magic != FOOTER_MAGIC {
            bail!(CorruptIndex(format!(
                "codec footer mismatch: actual={} vs expected={}",
                magic, FOOTER_MAGIC
            )));
        }
        let algorithm_id = input.read_int()?;
        if algorithm_id != 0 {
            bail!(CorruptIndex(format!(
                "codec footer mismatch: unknown algorithm_id: {}",
                algorithm_id
            )));
        }
        Ok(())
    }
}

pub fn check_footer<T: ChecksumIndexInput + ?Sized>(input: &mut T) -> Result<i64> {
    validate_footer(input)?;
    let actual_checksum: i64 = input.checksum();
    let expected_checksum: i64 = read_crc(input)?;
    if actual_checksum != expected_checksum {
        bail!(CorruptIndex(format!(
            "checksum failed (hardware problems?): expected=0x{:X}, actual=0x{:X}",
            expected_checksum, actual_checksum
        )));
    }
    Ok(actual_checksum)
}

fn read_crc<T: IndexInput + ?Sized>(input: &mut T) -> Result<i64> {
    let val = input.read_long()?;
    if (val as u64 & 0xFFFF_FFFF_0000_0000) != 0 {
        bail!(CorruptIndex(format!("Illegal CRC-32 checksum: {}", val)));
    }
    Ok(val)
}

pub fn retrieve_checksum<T: IndexInput + ?Sized>(input: &mut T) -> Result<i64> {
    let length = input.len();
    let footer_length = footer_length() as u64;
    if length < footer_length {
        bail!(CorruptIndex(format!(
            "misplaced codec footer (file truncated?): length={}, but footer_length={}",
            length, footer_length
        )));
    }
    input.seek((length - footer_length) as i64)?;
    validate_footer(input)?;

    read_crc(input)
}

// TODO: duplicates to refactor
pub fn check_checksum<T: IndexInput + ?Sized>(input: &mut T, actual_checksum: i64) -> Result<()> {
    let expected_checksum: i64 = read_crc(input)?;
    if actual_checksum != expected_checksum {
        bail!(CorruptIndex(format!(
            "checksum failed (hardware problems?): expected=0x{:X}, actual=0x{:X}",
            expected_checksum, actual_checksum
        )));
    }
    Ok(())
}

pub fn checksum_entire_file<T: IndexInput + ?Sized>(input: &T) -> Result<i64> {
    let mut index = input.clone()?;
    index.seek(0)?;
    let mut checksum = BufferedChecksumIndexInput::new(index);
    let mut len = checksum.len();
    let mut pos = checksum.file_pointer() as u64;
    if len < footer_length() as u64 {
        bail!(CorruptIndex(format!(
            "misplaced codec footer (file truncated?): length={} but footerLength=={}",
            checksum.len(),
            footer_length()
        )));
    }
    const BUFSIZ: u64 = 1024 * 64;
    let mut buffer = [0u8; BUFSIZ as usize];
    len -= footer_length() as u64;

    while pos < len {
        let size = if len - pos < BUFSIZ {
            len - pos
        } else {
            BUFSIZ
        };
        pos += checksum.read(&mut buffer[0..size as usize])? as u64;
    }

    validate_footer(&mut checksum)?;
    let actual = checksum.checksum();
    check_checksum(&mut checksum, actual)?;
    Ok(actual)
}