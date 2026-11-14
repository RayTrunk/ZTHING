//! Reads NTFS volumes into a [`zthing_core::Index`] by parsing the raw $MFT.
//! Opening a volume needs administrator rights.

mod parse;
mod volume;

use std::collections::{HashMap, HashSet};
use std::fs;

use windows::Win32::System::Ioctl::FSCTL_GET_NTFS_VOLUME_DATA;
use zthing_core::{ATTRIBUTE_DIRECTORY, Index, IndexError};

use volume::Volume;

pub use volume::fixed_drives;

/// MFT record of the root folder on every NTFS volume.
const ROOT_RECORD: usize = 5;
/// Records below 16 are NTFS metadata files such as $MFT, $LogFile and $Extend.
const FIRST_USER_RECORD: usize = 16;
const NO_ID: u32 = u32::MAX;
/// A multiple of every NTFS cluster size, large enough to keep the per-read overhead small.
const READ_SIZE: usize = 4 << 20;
/// Covers every common sector size.
const SECTOR_ALIGNMENT: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum NtfsError {
    #[error("{drive}: is {filesystem}, not NTFS")]
    NotNtfs { drive: char, filesystem: String },
    #[error("{drive}: {action}: {source}")]
    Windows {
        drive: char,
        action: &'static str,
        source: windows::core::Error,
    },
    #[error("{drive}: cannot read the $MFT: {reason}")]
    Mft { drive: char, reason: &'static str },
    #[error(transparent)]
    Index(#[from] IndexError),
}

/// Index id per MFT record number of one volume.
pub struct Records {
    ids: Vec<u32>,
}

impl Records {
    pub fn get(&self, record: u64) -> Option<u32> {
        let id = *self.ids.get(usize::try_from(record).ok()?)?;
        (id != NO_ID).then_some(id)
    }

    pub fn heap_size(&self) -> usize {
        self.ids.capacity() * size_of::<u32>()
    }
}

pub struct NtfsVolume {
    drive: char,
    volume: Volume,
}

impl NtfsVolume {
    pub fn open(drive: char) -> Result<Self, NtfsError> {
        let filesystem = volume::filesystem(drive).map_err(|source| NtfsError::Windows {
            drive,
            action: "reading the file system",
            source,
        })?;
        if filesystem != "NTFS" {
            return Err(NtfsError::NotNtfs { drive, filesystem });
        }
        let volume = Volume::open(drive).map_err(|source| NtfsError::Windows {
            drive,
            action: "opening the volume",
            source,
        })?;
        Ok(Self { drive, volume })
    }

    /// Adds a root entry such as `C:` and every file and folder below it,
    /// with attributes, size and modification date.
    pub fn read(&self, index: &mut Index) -> Result<Records, NtfsError> {
        let mut data = [0u8; 128];
        let len = self
            .volume
            .ioctl(FSCTL_GET_NTFS_VOLUME_DATA, &(), &mut data)
            .map_err(|source| self.error("reading the volume layout", source))?;
        let layout =
            parse::layout(&data[..len]).ok_or_else(|| self.mft_error("volume data too short"))?;
        // Whole records per cluster keep every read aligned to record boundaries.
        if layout.record == 0 || layout.cluster % layout.record as u64 != 0 {
            return Err(self.mft_error("clusters are smaller than file records"));
        }

        let cluster = layout.cluster as usize;
        let mut storage = Vec::new();
        let buf = aligned(&mut storage, READ_SIZE.max(cluster));
        self.read_at(layout.mft_offset, &mut buf[..cluster])?;
        let extents = parse::mft_extents(&mut buf[..layout.record], layout.cluster)
            .ok_or_else(|| self.mft_error("record 0 has no readable $DATA runs"))?;
        if extents.iter().map(|e| e.end - e.start).sum::<u64>() < layout.mft_len {
            return Err(self.mft_error("its runs continue in an attribute list"));
        }

        let root = index.push(&format!("{}:", self.drive), None, ATTRIBUTE_DIRECTORY, 0, 0)?;
        let record_count = (layout.mft_len / layout.record as u64) as usize;
        let mut ids = vec![NO_ID; record_count.max(FIRST_USER_RECORD)];
        ids[ROOT_RECORD] = root;
        let mut parents = Vec::new();
        // Sizes from extension records that precede their base record.
        let mut early_sizes = HashMap::new();
        let mut name = String::new();
        let mut record = 0usize;
        let mut remaining = layout.mft_len;
        for extent in extents {
            let mut offset = extent.start;
            while offset < extent.end && remaining > 0 {
                let len = (extent.end - offset)
                    .min(READ_SIZE as u64)
                    .min(remaining.next_multiple_of(layout.cluster))
                    as usize;
                if self.read_at(offset, &mut buf[..len])? != len {
                    return Err(self.mft_error("short read"));
                }
                let valid = (len as u64).min(remaining) as usize;
                for chunk in buf[..valid].chunks_exact_mut(layout.record) {
                    let current = record;
                    record += 1;
                    let Some(file) = parse::file_record(chunk) else {
                        continue;
                    };
                    let target = file.base.map_or(current, |base| base as usize);
                    if target < FIRST_USER_RECORD || target >= ids.len() {
                        continue;
                    }
                    let id = match (ids[target], file.name) {
                        (NO_ID, Some((parent, utf16))) => {
                            name.clear();
                            name.extend(
                                char::decode_utf16(
                                    utf16
                                        .chunks_exact(2)
                                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
                                )
                                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)),
                            );
                            let id = index.push(&name, None, 0, 0, 0)?;
                            ids[target] = id;
                            parents.push((id, parent));
                            if let Some(size) = early_sizes.remove(&target) {
                                index.set_size(id, size);
                            }
                            id
                        }
                        (NO_ID, None) => {
                            if let Some(size) = file.size {
                                early_sizes.insert(target, size);
                            }
                            continue;
                        }
                        (id, _) => id,
                    };
                    if file.base.is_none() {
                        let directory = if file.directory {
                            ATTRIBUTE_DIRECTORY
                        } else {
                            0
                        };
                        index.set_attributes(id, file.attributes.unwrap_or(0) | directory);
                    }
                    if let Some(size) = file.size {
                        index.set_size(id, size);
                    }
                    if let Some(modified) = file.modified {
                        index.set_modified(id, modified);
                    }
                }
                offset += len as u64;
                remaining -= valid as u64;
            }
        }

        let records = Records { ids };
        // Some internal files, such as $RmMetadata, name the root as parent in
        // their $FILE_NAME but are missing from the root's directory listing.
        // Without a listing every root child is kept.
        let root_listing: Option<HashSet<String>> =
            fs::read_dir(format!(r"{}:\", self.drive)).ok().map(|dir| {
                dir.filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            });
        link_parents(index, &records, root, &parents, root_listing.as_ref());
        Ok(records)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, NtfsError> {
        self.volume
            .read_at(offset, buf)
            .map_err(|source| self.error("reading the $MFT", source))
    }

    fn error(&self, action: &'static str, source: windows::core::Error) -> NtfsError {
        NtfsError::Windows {
            drive: self.drive,
            action,
            source,
        }
    }

    fn mft_error(&self, reason: &'static str) -> NtfsError {
        NtfsError::Mft {
            drive: self.drive,
            reason,
        }
    }
}

/// Sets the parent of every entry pushed after `root` and removes entries whose
/// parent chain does not reach `root`: NTFS metadata below $Extend, records
/// orphaned while the volume was in use, cycles in damaged records, and root
/// children absent from `root_listing`.
fn link_parents(
    index: &mut Index,
    records: &Records,
    root: u32,
    parents: &[(u32, u64)],
    root_listing: Option<&HashSet<String>>,
) {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Unknown,
        Visiting,
        Reachable,
        Unreachable,
    }
    let first = root as usize;
    let mut parent_of = vec![NO_ID; index.len() - first];
    for &(id, parent) in parents {
        parent_of[id as usize - first] = records.get(parent).unwrap_or(NO_ID);
    }
    let mut state = vec![State::Unknown; parent_of.len()];
    state[0] = State::Reachable;
    let mut chain = Vec::new();
    for start in 0..state.len() {
        let mut at = start;
        let reachable = loop {
            match state[at] {
                State::Reachable => break true,
                State::Unreachable | State::Visiting => break false,
                State::Unknown => {
                    state[at] = State::Visiting;
                    chain.push(at);
                    match parent_of[at] {
                        NO_ID => break false,
                        parent
                            if parent == root
                                && root_listing.is_some_and(|names| {
                                    !names.contains(index.name((first + at) as u32))
                                }) =>
                        {
                            break false;
                        }
                        parent => at = parent as usize - first,
                    }
                }
            }
        };
        let verdict = if reachable {
            State::Reachable
        } else {
            State::Unreachable
        };
        for at in chain.drain(..) {
            state[at] = verdict;
        }
    }
    for (offset, (&parent, &state)) in parent_of.iter().zip(&state).enumerate().skip(1) {
        let id = (first + offset) as u32;
        if state == State::Reachable {
            index.set_parent(id, Some(parent));
        } else {
            index.remove(id);
        }
    }
}

/// Raw volume reads bypass the cache, which can require sector-aligned memory.
fn aligned(storage: &mut Vec<u8>, len: usize) -> &mut [u8] {
    *storage = vec![0; len + SECTOR_ALIGNMENT];
    let start = storage.as_ptr().align_offset(SECTOR_ALIGNMENT);
    &mut storage[start..start + len]
}
