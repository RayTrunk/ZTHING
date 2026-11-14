//! Byte layouts of NTFS structures, read without `unsafe`. Malformed input
//! yields `None` instead of a panic.

use std::ops::Range;

/// The low 48 bits of a file reference are the MFT record number, the high 16 bits a sequence number.
const RECORD_MASK: u64 = 0xFFFF_FFFF_FFFF;

const RECORD_IN_USE: u16 = 0x01;
const RECORD_IS_DIRECTORY: u16 = 0x02;
const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;
/// 8.3 short names duplicate the long name of the same file.
const NAMESPACE_DOS: u8 = 2;
/// NTFS stores an update sequence number every 512 bytes, whatever the sector size.
const FIXUP_STRIDE: usize = 512;
const FILETIME_TICKS_PER_SECOND: u64 = 10_000_000;
const FILETIME_TO_UNIX_SECONDS: i64 = 11_644_473_600;

fn le<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    le(bytes, at).map(u16::from_le_bytes)
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    le(bytes, at).map(u32::from_le_bytes)
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    le(bytes, at).map(u64::from_le_bytes)
}

pub(crate) struct Layout {
    pub serial: u64,
    pub cluster: u64,
    pub record: usize,
    pub mft_offset: u64,
    pub mft_len: u64,
}

/// Reads `NTFS_VOLUME_DATA_BUFFER` from `FSCTL_GET_NTFS_VOLUME_DATA`.
pub(crate) fn layout(data: &[u8]) -> Option<Layout> {
    let cluster = u64::from(u32_at(data, 44)?);
    Some(Layout {
        serial: u64_at(data, 0)?,
        cluster,
        record: u32_at(data, 48)? as usize,
        mft_len: u64_at(data, 56)?,
        mft_offset: u64_at(data, 64)?.checked_mul(cluster)?,
    })
}

pub(crate) struct JournalData {
    pub id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
}

/// Reads `USN_JOURNAL_DATA_V0` from `FSCTL_QUERY_USN_JOURNAL`.
pub(crate) fn journal_data(data: &[u8]) -> Option<JournalData> {
    Some(JournalData {
        id: u64_at(data, 0)?,
        first_usn: u64_at(data, 8)? as i64,
        next_usn: u64_at(data, 16)? as i64,
    })
}

pub(crate) struct UsnChange<'a> {
    /// Full file reference including the sequence number, as `OpenFileById` needs it.
    pub file_reference: u64,
    pub record: u64,
    pub parent: u64,
    pub reason: u32,
    pub attributes: u32,
    /// UTF-16LE.
    pub name: &'a [u8],
}

/// The next USN to read and the `USN_RECORD_V2` entries of `FSCTL_READ_USN_JOURNAL` output.
pub(crate) fn usn_changes(buf: &[u8]) -> Option<(i64, impl Iterator<Item = UsnChange<'_>>)> {
    let next = u64_at(buf, 0)? as i64;
    let mut at = 8;
    let changes = std::iter::from_fn(move || {
        let len = u32_at(buf, at)? as usize;
        let record = buf.get(at..at.checked_add(len)?)?;
        at += len;
        let file_reference = u64_at(record, 8)?;
        let name_len = usize::from(u16_at(record, 56)?);
        let name_offset = usize::from(u16_at(record, 58)?);
        Some(UsnChange {
            file_reference,
            record: file_reference & RECORD_MASK,
            parent: u64_at(record, 16)? & RECORD_MASK,
            reason: u32_at(record, 40)?,
            attributes: u32_at(record, 52)?,
            name: record.get(name_offset..name_offset + name_len)?,
        })
    });
    Some((next, changes))
}

pub(crate) fn unix_from_filetime(filetime: u64) -> i64 {
    (filetime / FILETIME_TICKS_PER_SECOND) as i64 - FILETIME_TO_UNIX_SECONDS
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FileRecord<'a> {
    /// Set for extension records, whose attributes belong to the base record.
    pub base: Option<u64>,
    pub directory: bool,
    /// Windows file attributes from `$STANDARD_INFORMATION`.
    pub attributes: Option<u32>,
    /// Unix seconds (UTC).
    pub modified: Option<i64>,
    pub size: Option<u64>,
    /// Parent record number and UTF-16LE name of the first long `$FILE_NAME`.
    /// Further hard links of the same file are ignored.
    pub name: Option<(u64, &'a [u8])>,
}

/// Applies the fixups in place and reads the attributes ZTHING indexes.
/// Returns `None` for unused, torn or malformed records.
pub(crate) fn file_record(record: &mut [u8]) -> Option<FileRecord<'_>> {
    fixup(record)?;
    let record: &[u8] = record;
    let flags = u16_at(record, 22)?;
    if flags & RECORD_IN_USE == 0 {
        return None;
    }
    let base = u64_at(record, 32)? & RECORD_MASK;
    let mut file = FileRecord {
        base: (base != 0).then_some(base),
        directory: flags & RECORD_IS_DIRECTORY != 0,
        attributes: None,
        modified: None,
        size: None,
        name: None,
    };
    for (kind, attr) in attributes(record) {
        let non_resident = *attr.get(8)? != 0;
        let unnamed = *attr.get(9)? == 0;
        match kind {
            ATTR_STANDARD_INFORMATION if !non_resident => {
                let content = usize::from(u16_at(attr, 20)?);
                let filetime = u64_at(attr, content + 8)?;
                file.modified = Some(unix_from_filetime(filetime));
                file.attributes = Some(u32_at(attr, content + 32)?);
            }
            ATTR_FILE_NAME if !non_resident && file.name.is_none() => {
                let content = usize::from(u16_at(attr, 20)?);
                let len = usize::from(*attr.get(content + 64)?);
                if *attr.get(content + 65)? != NAMESPACE_DOS {
                    let parent = u64_at(attr, content)? & RECORD_MASK;
                    let name = attr.get(content + 66..content + 66 + 2 * len)?;
                    file.name = Some((parent, name));
                }
            }
            ATTR_DATA if unnamed && !non_resident => {
                file.size = Some(u64::from(u32_at(attr, 16)?));
            }
            // Only the extent starting at VCN 0 carries the stream size.
            ATTR_DATA if unnamed && u64_at(attr, 16)? == 0 => file.size = Some(u64_at(attr, 48)?),
            _ => {}
        }
    }
    Some(file)
}

/// Volume byte ranges of the $MFT, from the unnamed `$DATA` attribute of MFT record 0.
pub(crate) fn mft_extents(record: &mut [u8], cluster: u64) -> Option<Vec<Range<u64>>> {
    fixup(record)?;
    let (_, attr) = attributes(record).find(|&(kind, attr)| {
        kind == ATTR_DATA && attr.get(8) == Some(&1) && attr.get(9) == Some(&0)
    })?;
    let mut at = usize::from(u16_at(attr, 32)?);
    let mut lcn = 0i64;
    let mut extents = Vec::new();
    while let Some(&header) = attr.get(at).filter(|&&header| header != 0) {
        let len_size = usize::from(header & 0x0F);
        let offset_size = usize::from(header >> 4);
        // A run without an offset is sparse, which the $MFT never is.
        if offset_size == 0 {
            return None;
        }
        let clusters = le_var(attr.get(at + 1..at + 1 + len_size)?, false)?;
        let offset_at = at + 1 + len_size;
        lcn += le_var(attr.get(offset_at..offset_at + offset_size)?, true)?;
        let start = u64::try_from(lcn).ok()?.checked_mul(cluster)?;
        let len = u64::try_from(clusters).ok()?.checked_mul(cluster)?;
        extents.push(start..start + len);
        at = offset_at + offset_size;
    }
    Some(extents)
}

/// Reverts the update sequence: the last two bytes of every 512-byte stride
/// hold a check value on disk, and the original bytes are kept in an array.
fn fixup(record: &mut [u8]) -> Option<()> {
    if record.get(..4)? != b"FILE" {
        return None;
    }
    let array = usize::from(u16_at(record, 4)?);
    let count = usize::from(u16_at(record, 6)?);
    let check: [u8; 2] = le(record, array)?;
    for i in 1..count {
        let original: [u8; 2] = le(record, array + 2 * i)?;
        let end = i * FIXUP_STRIDE;
        let tail = record.get_mut(end - 2..end)?;
        // A mismatch means an interrupted write tore the record.
        if *tail != check {
            return None;
        }
        tail.copy_from_slice(&original);
    }
    Some(())
}

/// `(type, attribute bytes)` of every attribute in a fixed-up record.
fn attributes(record: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let mut at = u16_at(record, 20).map_or(record.len(), usize::from);
    std::iter::from_fn(move || {
        let kind = u32_at(record, at)?;
        let len = u32_at(record, at + 4)? as usize;
        if kind == ATTR_END || len == 0 {
            return None;
        }
        let attr = record.get(at..at.checked_add(len)?)?;
        at += len;
        Some((kind, attr))
    })
}

/// Little-endian integer of 1 to 8 bytes, sign-extended if `signed`.
fn le_var(bytes: &[u8], signed: bool) -> Option<i64> {
    let last = *bytes.last()?;
    if bytes.len() > 8 {
        return None;
    }
    let mut full = if signed && last & 0x80 != 0 {
        [0xFF; 8]
    } else {
        [0; 8]
    };
    full[..bytes.len()].copy_from_slice(bytes);
    Some(i64::from_le_bytes(full))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-03-15 12:00 UTC.
    const NOON: i64 = 1_773_576_000;
    const CHECK: [u8; 2] = [0xCD, 0xAB];

    fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
        bytes[at..at + value.len()].copy_from_slice(value);
    }

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// A 1024-byte FILE record as NTFS writes it to disk, with fixups applied.
    fn record(flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut rec = vec![0u8; 1024];
        put(&mut rec, 0, b"FILE");
        put(&mut rec, 4, &48u16.to_le_bytes());
        put(&mut rec, 6, &3u16.to_le_bytes());
        put(&mut rec, 20, &56u16.to_le_bytes());
        put(&mut rec, 22, &flags.to_le_bytes());
        put(&mut rec, 32, &base.to_le_bytes());
        let mut at = 56;
        for attr in attrs {
            put(&mut rec, at, attr);
            at += attr.len();
        }
        put(&mut rec, at, &ATTR_END.to_le_bytes());
        put(&mut rec, 48, &CHECK);
        for i in 1..3 {
            let end = i * 512;
            let original = [rec[end - 2], rec[end - 1]];
            put(&mut rec, 48 + 2 * i, &original);
            put(&mut rec, end - 2, &CHECK);
        }
        rec
    }

    fn resident(kind: u32, content: &[u8]) -> Vec<u8> {
        let mut attr = vec![0u8; (24 + content.len()).next_multiple_of(8)];
        let len = attr.len() as u32;
        put(&mut attr, 0, &kind.to_le_bytes());
        put(&mut attr, 4, &len.to_le_bytes());
        put(&mut attr, 16, &(content.len() as u32).to_le_bytes());
        put(&mut attr, 20, &24u16.to_le_bytes());
        put(&mut attr, 24, content);
        attr
    }

    fn standard_information(unix: i64, attributes: u32) -> Vec<u8> {
        let filetime = (unix + FILETIME_TO_UNIX_SECONDS) as u64 * FILETIME_TICKS_PER_SECOND;
        let mut content = [0u8; 48];
        put(&mut content, 8, &filetime.to_le_bytes());
        put(&mut content, 32, &attributes.to_le_bytes());
        resident(ATTR_STANDARD_INFORMATION, &content)
    }

    fn file_name(parent_reference: u64, namespace: u8, name: &str) -> Vec<u8> {
        let name = utf16(name);
        let mut content = vec![0u8; 66 + name.len()];
        put(&mut content, 0, &parent_reference.to_le_bytes());
        content[64] = (name.len() / 2) as u8;
        content[65] = namespace;
        put(&mut content, 66, &name);
        resident(ATTR_FILE_NAME, &content)
    }

    fn non_resident_data(start_vcn: u64, size: u64, runs: &[u8]) -> Vec<u8> {
        let mut attr = vec![0u8; (64 + runs.len()).next_multiple_of(8)];
        let len = attr.len() as u32;
        put(&mut attr, 0, &ATTR_DATA.to_le_bytes());
        put(&mut attr, 4, &len.to_le_bytes());
        attr[8] = 1;
        put(&mut attr, 16, &start_vcn.to_le_bytes());
        put(&mut attr, 32, &64u16.to_le_bytes());
        put(&mut attr, 48, &size.to_le_bytes());
        put(&mut attr, 64, runs);
        attr
    }

    #[test]
    fn base_record_with_resident_data_and_fixups() {
        // 400 data bytes span the first sector tail, which the fixup must restore.
        let data = vec![0x11; 400];
        let mut rec = record(
            RECORD_IN_USE,
            0,
            &[
                standard_information(NOON, 0x20),
                file_name(0x0005_0000_0000_0005, NAMESPACE_DOS, "LANGER~1.TXT"),
                file_name(0x0005_0000_0000_0005, 1, "Langer Name.txt"),
                resident(ATTR_DATA, &data),
            ],
        );
        assert_eq!(&rec[510..512], &CHECK);
        let long_name = utf16("Langer Name.txt");
        assert_eq!(
            file_record(&mut rec),
            Some(FileRecord {
                base: None,
                directory: false,
                attributes: Some(0x20),
                modified: Some(NOON),
                size: Some(400),
                name: Some((5, long_name.as_slice())),
            })
        );
        assert_eq!(&rec[510..512], &[0x11, 0x11]);
    }

    #[test]
    fn directory_flag_comes_from_the_record_header() {
        let mut rec = record(
            RECORD_IN_USE | RECORD_IS_DIRECTORY,
            0,
            &[file_name(5, 1, "Ordner")],
        );
        assert!(file_record(&mut rec).unwrap().directory);
    }

    #[test]
    fn extension_record_reports_its_base_and_first_extent_size() {
        let base_reference = 0x0003_0000_0000_002A;
        let mut rec = record(
            RECORD_IN_USE,
            base_reference,
            &[non_resident_data(0, 7 << 30, &[0])],
        );
        let file = file_record(&mut rec).unwrap();
        assert_eq!((file.base, file.size), (Some(42), Some(7 << 30)));

        let mut later_extent = record(
            RECORD_IN_USE,
            base_reference,
            &[non_resident_data(9, 1, &[0])],
        );
        assert_eq!(file_record(&mut later_extent).unwrap().size, None);
    }

    #[test]
    fn unusable_records_are_skipped() {
        let mut unused = record(0, 0, &[standard_information(NOON, 0)]);
        assert_eq!(file_record(&mut unused), None);

        let mut torn = record(RECORD_IN_USE, 0, &[standard_information(NOON, 0)]);
        torn[1022] ^= 0xFF;
        assert_eq!(file_record(&mut torn), None);

        let mut garbage = vec![0xEE; 1024];
        assert_eq!(file_record(&mut garbage), None);
    }

    #[test]
    fn usn_changes_follow_the_next_usn() {
        let name = utf16("Ä.txt");
        let mut buf = 4711u64.to_le_bytes().to_vec();
        for (frn, reason) in [
            (0x0002_0000_0000_0030u64, 0x100u32),
            (0x0001_0000_0000_0031, 0x200),
        ] {
            let mut rec = vec![0u8; (60 + name.len()).next_multiple_of(8)];
            let len = rec.len() as u32;
            put(&mut rec, 0, &len.to_le_bytes());
            put(&mut rec, 8, &frn.to_le_bytes());
            put(&mut rec, 16, &0x0005_0000_0000_0005u64.to_le_bytes());
            put(&mut rec, 40, &reason.to_le_bytes());
            put(&mut rec, 52, &0x20u32.to_le_bytes());
            put(&mut rec, 56, &(name.len() as u16).to_le_bytes());
            put(&mut rec, 58, &60u16.to_le_bytes());
            put(&mut rec, 60, &name);
            buf.extend_from_slice(&rec);
        }
        let (next, changes) = usn_changes(&buf).unwrap();
        let changes: Vec<_> = changes.collect();
        assert_eq!(next, 4711);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].file_reference, 0x0002_0000_0000_0030);
        assert_eq!((changes[0].record, changes[0].parent), (0x30, 5));
        assert_eq!((changes[0].reason, changes[1].reason), (0x100, 0x200));
        assert_eq!(changes[0].attributes, 0x20);
        assert_eq!(changes[0].name, name.as_slice());
    }

    #[test]
    fn mft_extents_follow_relative_run_offsets() {
        // 0x40 clusters at LCN 0x0C0000, then 0x20 clusters 0x100 clusters earlier.
        let runs = [0x31, 0x40, 0x00, 0x00, 0x0C, 0x21, 0x20, 0x00, 0xFF, 0x00];
        let mut rec = record(
            RECORD_IN_USE,
            0,
            &[non_resident_data(0, 0x60 * 4096, &runs)],
        );
        let first = 0x0C_0000 * 4096;
        let second = (0x0C_0000 - 0x100) * 4096;
        assert_eq!(
            mft_extents(&mut rec, 4096),
            Some(vec![
                first..first + 0x40 * 4096,
                second..second + 0x20 * 4096
            ])
        );
    }
}
