//! Reads NTFS volumes into a [`zthing_core::Index`] by parsing the raw $MFT and
//! keeps the index current through the USN journal. Needs administrator rights.

mod indexer;
mod parse;
mod volume;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::Range;

use windows::Win32::Foundation::{
    ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
};
use windows::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
    READ_USN_JOURNAL_DATA_V0,
};
use zthing_core::{ATTRIBUTE_DIRECTORY, Index, IndexError};

use parse::{FileRecord, JournalData, Layout, UsnChange};
use volume::Volume;

pub use indexer::{Indexer, OpenReport};
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
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
const USN_REASON_SECURITY_CHANGE: u32 = 0x0000_0800;
const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
/// Access group = (volume number + 1) << 24 | NTFS security id.
const GROUP_SHIFT: u32 = 24;
const OVERWRITTEN: &str = "journal entries since the last update were overwritten";

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
    /// The journal cannot be replayed, so the volume has to be read again.
    #[error("{drive}: {reason}")]
    Journal { drive: char, reason: &'static str },
    #[error(transparent)]
    Index(#[from] IndexError),
}

/// Everything needed to keep one volume's entries current. Saved with the index.
pub struct VolumeState {
    drive: char,
    serial: u64,
    /// `None` if the volume has no active USN journal.
    journal: Option<Journal>,
    root: u32,
    /// Index id per MFT record number.
    ids: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Journal {
    id: u64,
    next_usn: i64,
}

impl VolumeState {
    pub fn id(&self, record: u64) -> Option<u32> {
        let id = *self.ids.get(usize::try_from(record).ok()?)?;
        (id != NO_ID).then_some(id)
    }

    pub fn heap_size(&self) -> usize {
        self.ids.capacity() * size_of::<u32>()
    }

    fn forget(&mut self, index: &mut Index, record: usize) {
        if let Some(id) = self.ids.get_mut(record).filter(|id| **id != NO_ID) {
            index.remove(*id);
            *id = NO_ID;
        }
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

    pub fn drive(&self) -> char {
        self.drive
    }

    /// Adds a root entry such as `C:` and every file and folder below it,
    /// with attributes, size, modification date and access group. `volume_number`
    /// keeps the access groups of different volumes apart.
    pub fn read(&self, index: &mut Index, volume_number: u8) -> Result<VolumeState, NtfsError> {
        let group_base = (u32::from(volume_number) + 1) << GROUP_SHIFT;
        // Queried before the $MFT is read, so replaying from this position also
        // catches the changes made while the read runs.
        let journal = self.journal()?.map(|journal| Journal {
            id: journal.id,
            next_usn: journal.next_usn,
        });
        let layout = self.layout()?;
        let mut storage = Vec::new();
        let buf = aligned(&mut storage, READ_SIZE.max(layout.cluster as usize));
        let extents = self.mft_extents(&layout, buf)?;
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
        for extent in &extents {
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
                            decode_name(utf16, &mut name);
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
                    apply_metadata(index, id, &file, group_base);
                }
                offset += len as u64;
                remaining -= valid as u64;
            }
        }

        let state = VolumeState {
            drive: self.drive,
            serial: layout.serial,
            journal,
            root,
            ids,
        };
        // Some internal files, such as $RmMetadata, name the root as parent in
        // their $FILE_NAME but are missing from the root's directory listing.
        // Without a listing every root child is kept.
        let root_listing: Option<HashSet<String>> =
            fs::read_dir(format!(r"{}:\", self.drive)).ok().map(|dir| {
                dir.filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            });
        link_parents(index, &state, &parents, root_listing.as_ref());
        Ok(state)
    }

    /// Applies the journal entries written since the last call and returns how
    /// many there were. A volume without journal reports none.
    /// [`NtfsError::Journal`] means the volume has to be read again.
    pub fn catch_up(&self, index: &mut Index, state: &mut VolumeState) -> Result<usize, NtfsError> {
        let Some(saved) = state.journal else {
            return Ok(0);
        };
        let current = self
            .journal()?
            .ok_or_else(|| self.journal_error("the USN journal was deleted"))?;
        if current.id != saved.id {
            return Err(self.journal_error("the USN journal was recreated"));
        }
        if saved.next_usn < current.first_usn {
            return Err(self.journal_error(OVERWRITTEN));
        }

        // Record number -> file reference of entries whose size and date must be read.
        let mut refresh = HashMap::new();
        let mut name = String::new();
        let mut count = 0;
        let mut input = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: saved.next_usn,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: saved.id,
        };
        let mut buf = vec![0u8; 64 << 10];
        loop {
            let len = match self.volume.ioctl(FSCTL_READ_USN_JOURNAL, &input, &mut buf) {
                Ok(len) => len,
                Err(e) if e.code() == ERROR_JOURNAL_ENTRY_DELETED.to_hresult() => {
                    return Err(self.journal_error(OVERWRITTEN));
                }
                Err(source) => return Err(self.error("reading the USN journal", source)),
            };
            let (next, changes) = parse::usn_changes(&buf[..len])
                .ok_or_else(|| self.journal_error("unexpected USN journal output"))?;
            for change in changes {
                count += 1;
                self.apply_change(index, state, &change, &mut name, &mut refresh)?;
            }
            if next == input.StartUsn {
                break;
            }
            input.StartUsn = next;
        }

        for (record, file_reference) in refresh {
            // A file deleted after its last journal entry fails to open. Its
            // deletion arrives with the next update.
            if let Some(id) = state.id(record)
                && let Ok((size, filetime)) = self.volume.file_info(file_reference)
            {
                index.set_size(id, size);
                index.set_modified(id, parse::unix_from_filetime(filetime as u64));
            }
        }
        state.journal = Some(Journal {
            id: saved.id,
            next_usn: input.StartUsn,
        });
        Ok(count)
    }

    /// Applies one journal entry in journal order. Name, parent and attributes
    /// come from the entry itself. Raw $MFT reads may still see the record as it
    /// was before the change, because NTFS writes metadata back lazily.
    fn apply_change(
        &self,
        index: &mut Index,
        state: &mut VolumeState,
        change: &UsnChange<'_>,
        name: &mut String,
        refresh: &mut HashMap<u64, u64>,
    ) -> Result<(), NtfsError> {
        let record = change.record as usize;
        if record < FIRST_USER_RECORD || change.reason & USN_REASON_RENAME_OLD_NAME != 0 {
            return Ok(());
        }
        let parent = state
            .id(change.parent)
            .filter(|&parent| !index.is_removed(parent));
        let (false, Some(parent)) = (change.reason & USN_REASON_FILE_DELETE != 0, parent) else {
            state.forget(index, record);
            refresh.remove(&change.record);
            return Ok(());
        };
        decode_name(change.name, name);
        let existing = state.id(change.record);
        // The same rule as in `read`: new root children must be listed in the root.
        if parent == state.root
            && existing.is_none()
            && fs::symlink_metadata(format!(r"{}:\{name}", self.drive)).is_err()
        {
            return Ok(());
        }
        let id = match existing {
            Some(id) => {
                if index.name(id) != name.as_str() {
                    index.set_name(id, name)?;
                }
                index.set_parent(id, Some(parent));
                id
            }
            None => {
                let id = index.push(name, Some(parent), 0, 0, 0)?;
                if record >= state.ids.len() {
                    state.ids.resize(record + 1, NO_ID);
                }
                state.ids[record] = id;
                id
            }
        };
        index.set_attributes(id, change.attributes);
        if existing.is_none() || change.reason & USN_REASON_SECURITY_CHANGE != 0 {
            // The journal entry carries no security id, so access is checked for this folder alone.
            index.set_group(id, 0);
        }
        refresh.insert(change.record, change.file_reference);
        Ok(())
    }

    fn serial(&self) -> Result<u64, NtfsError> {
        Ok(self.layout()?.serial)
    }

    fn layout(&self) -> Result<Layout, NtfsError> {
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
        Ok(layout)
    }

    /// Volume byte ranges of the $MFT. `buf` must hold at least one cluster.
    fn mft_extents(&self, layout: &Layout, buf: &mut [u8]) -> Result<Vec<Range<u64>>, NtfsError> {
        let cluster = &mut buf[..layout.cluster as usize];
        self.read_at(layout.mft_offset, cluster)?;
        parse::mft_extents(&mut cluster[..layout.record], layout.cluster)
            .ok_or_else(|| self.mft_error("record 0 has no readable $DATA runs"))
    }

    fn journal(&self) -> Result<Option<JournalData>, NtfsError> {
        let mut data = [0u8; 80];
        match self.volume.ioctl(FSCTL_QUERY_USN_JOURNAL, &(), &mut data) {
            Ok(len) => parse::journal_data(&data[..len])
                .map(Some)
                .ok_or_else(|| self.journal_error("unexpected USN journal data")),
            Err(e)
                if e.code() == ERROR_JOURNAL_NOT_ACTIVE.to_hresult()
                    || e.code() == ERROR_JOURNAL_DELETE_IN_PROGRESS.to_hresult() =>
            {
                Ok(None)
            }
            Err(source) => Err(self.error("querying the USN journal", source)),
        }
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

    fn journal_error(&self, reason: &'static str) -> NtfsError {
        NtfsError::Journal {
            drive: self.drive,
            reason,
        }
    }
}

fn decode_name(utf16: &[u8], out: &mut String) {
    out.clear();
    out.extend(
        char::decode_utf16(
            utf16
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
        )
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)),
    );
}

/// Attributes only come from base records, which hold `$STANDARD_INFORMATION`.
fn apply_metadata(index: &mut Index, id: u32, file: &FileRecord<'_>, group_base: u32) {
    if file.base.is_none() {
        // Security ids beyond the group bits stay unknown and get checked folder by folder.
        let group = file
            .security
            .filter(|&security| security < 1 << GROUP_SHIFT)
            .map_or(0, |security| group_base | security);
        index.set_group(id, group);
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

/// Sets the parent of every entry pushed after the volume root and removes
/// entries whose parent chain does not reach it: NTFS metadata below $Extend,
/// records orphaned while the volume was in use, cycles in damaged records,
/// and root children absent from `root_listing`.
fn link_parents(
    index: &mut Index,
    state: &VolumeState,
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
    let root = state.root;
    let first = root as usize;
    let mut parent_of = vec![NO_ID; index.len() - first];
    for &(id, parent) in parents {
        parent_of[id as usize - first] = state.id(parent).unwrap_or(NO_ID);
    }
    let mut states = vec![State::Unknown; parent_of.len()];
    states[0] = State::Reachable;
    let mut chain = Vec::new();
    for start in 0..states.len() {
        let mut at = start;
        let reachable = loop {
            match states[at] {
                State::Reachable => break true,
                State::Unreachable | State::Visiting => break false,
                State::Unknown => {
                    states[at] = State::Visiting;
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
            states[at] = verdict;
        }
    }
    for (offset, (&parent, &verdict)) in parent_of.iter().zip(&states).enumerate().skip(1) {
        let id = (first + offset) as u32;
        if verdict == State::Reachable {
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
