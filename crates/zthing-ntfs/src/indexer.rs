//! Loads, updates and saves the index of all volumes.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use zthing_core::Index;

use crate::{Journal, NO_ID, NtfsError, NtfsVolume, VolumeState};

const FILE_MAGIC: &[u8; 8] = b"ZTHINGDB";
/// Raised whenever the volume part of the file changes. The index part carries
/// its own version, see [`zthing_core::FORMAT_VERSION`].
const FILE_VERSION: u32 = 2;
/// Deleted entries keep their slot until a compaction, which runs once a tenth of the index is dead.
const COMPACT_SHARE: usize = 10;
/// Replaying this many journal entries takes milliseconds, writing the file about a second.
const SAVE_AFTER_ENTRIES: usize = 10_000;

pub struct Indexer {
    index: Index,
    volumes: Vec<NtfsVolume>,
    /// Same order as `volumes`.
    states: Vec<VolumeState>,
    /// Journal entries applied since the last load or save, `usize::MAX` after a rebuild.
    unsaved: usize,
}

pub struct OpenReport {
    /// Drives that could not be indexed. The other drives are searchable.
    pub skipped: Vec<NtfsError>,
    /// Why the saved index was not used, if the volumes were read again.
    pub rebuilt: Option<String>,
}

impl Indexer {
    /// Loads the index saved at `path` and replays each volume's journal. If
    /// that fails for any volume, all volumes are read again.
    pub fn open(path: &Path, drives: &[char]) -> Result<(Self, OpenReport), NtfsError> {
        let mut skipped = Vec::new();
        let volumes: Vec<NtfsVolume> = drives
            .iter()
            .filter_map(|&drive| NtfsVolume::open(drive).map_err(|e| skipped.push(e)).ok())
            .collect();
        let (index, states, mut rebuilt) = match load(path, &volumes) {
            Ok((index, states)) => (index, states, None),
            Err(reason) => (Index::new(), Vec::new(), Some(reason)),
        };
        let mut indexer = Self {
            index,
            volumes,
            states,
            unsaved: 0,
        };
        if rebuilt.is_none()
            && let Err(e) = indexer.update()
        {
            rebuilt = Some(e.to_string());
        }
        if rebuilt.is_some() {
            skipped.extend(indexer.rebuild()?);
        } else if indexer.needs_compaction() {
            indexer.compact();
        }
        Ok((indexer, OpenReport { skipped, rebuilt }))
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Drive letter of every indexed volume and whether its journal keeps it current.
    pub fn volumes(&self) -> impl Iterator<Item = (char, bool)> + '_ {
        self.states
            .iter()
            .map(|state| (state.drive, state.journal.is_some()))
    }

    pub fn needs_compaction(&self) -> bool {
        self.index.removed_count() * COMPACT_SHARE > self.index.len()
    }

    /// Drops deleted entries and renumbers the rest. Ids handed out before are invalid afterwards.
    pub fn compact(&mut self) {
        let remap = self.index.compact();
        let new_id = |id: u32| remap.get(id as usize).copied().flatten();
        for state in &mut self.states {
            // A volume root has no parent and therefore always survives.
            state.root = new_id(state.root).unwrap_or(state.root);
            for id in &mut state.ids {
                *id = new_id(*id).unwrap_or(NO_ID);
            }
            let records: Vec<usize> = state.links.keys().copied().collect();
            for record in records {
                // A dropped first link hands its place to the next surviving one.
                let ids: Vec<u32> = state.ids_of(record).filter_map(new_id).collect();
                state.set_ids(record, &ids);
            }
        }
        self.unsaved = usize::MAX;
    }

    /// Applies the journal entries of all volumes and returns how many MFT
    /// journal entries there were. On [`NtfsError::Journal`] call [`Indexer::rebuild`].
    pub fn update(&mut self) -> Result<usize, NtfsError> {
        let mut changed = 0;
        for (volume, state) in self.volumes.iter().zip(&mut self.states) {
            changed += volume.catch_up(&mut self.index, state)?;
        }
        self.unsaved = self.unsaved.saturating_add(changed);
        Ok(changed)
    }

    /// Reads all volumes again. Returns the volumes that failed, which are
    /// dropped from the index.
    pub fn rebuild(&mut self) -> Result<Vec<NtfsError>, NtfsError> {
        let mut failed = Vec::new();
        let mut kept = Vec::new();
        self.index = Index::new();
        self.states.clear();
        for (number, volume) in self.volumes.drain(..).enumerate() {
            let first = self.index.len() as u32;
            match volume.read(&mut self.index, number as u8) {
                Ok(state) => {
                    kept.push(volume);
                    self.states.push(state);
                }
                Err(e) => {
                    // A volume that failed halfway must not leave unlinked entries behind.
                    for id in first..self.index.len() as u32 {
                        self.index.remove(id);
                    }
                    failed.push(e);
                }
            }
        }
        self.volumes = kept;
        self.index.shrink_to_fit();
        self.unsaved = usize::MAX;
        self.update()?;
        Ok(failed)
    }

    /// True after a rebuild or once enough journal entries were applied that
    /// replaying them on the next start would cost more than writing the file.
    pub fn needs_save(&self) -> bool {
        self.unsaved >= SAVE_AFTER_ENTRIES
    }

    /// Writes the index and the journal positions to `path`.
    /// A temporary file is renamed over `path`, so a crash never leaves half a file.
    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Several processes may save to the same path, so each writes its own temporary file.
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        let mut out = BufWriter::new(File::create(&temporary)?);
        out.write_all(FILE_MAGIC)?;
        out.write_all(&FILE_VERSION.to_le_bytes())?;
        out.write_all(&(self.states.len() as u32).to_le_bytes())?;
        for state in &self.states {
            write_state(&mut out, state)?;
        }
        self.index.save(&mut out)?;
        out.into_inner()
            .map_err(io::IntoInnerError::into_error)?
            .sync_all()?;
        fs::rename(&temporary, path)?;
        self.unsaved = 0;
        Ok(())
    }
}

/// Returns why the saved index cannot be used.
fn load(path: &Path, volumes: &[NtfsVolume]) -> Result<(Index, Vec<VolumeState>), String> {
    let file = File::open(path).map_err(|e| format!("no saved index ({e})"))?;
    let mut input = BufReader::new(file);
    let states = read_states(&mut input).map_err(|e| format!("saved index unreadable: {e}"))?;
    let index = Index::load(&mut input).map_err(|e| format!("saved index unusable: {e}"))?;

    let saved: Vec<char> = states.iter().map(|s| s.drive).collect();
    let current: Vec<char> = volumes.iter().map(NtfsVolume::drive).collect();
    if saved != current {
        return Err(format!("drives changed from {saved:?} to {current:?}"));
    }
    for (state, volume) in states.iter().zip(volumes) {
        let drive = state.drive;
        if volume.serial().map_err(|e| e.to_string())? != state.serial {
            return Err(format!("{drive}: is a different volume than before"));
        }
        if state.journal.is_none() {
            return Err(format!("{drive}: has no USN journal to replay"));
        }
        let valid = |id: u32| (id as usize) < index.len();
        let links = state.links.values().flatten();
        if !valid(state.root)
            || !state
                .ids
                .iter()
                .chain(links)
                .all(|&id| id == NO_ID || valid(id))
        {
            return Err(format!(
                "{drive}: saved record map does not match the index"
            ));
        }
    }
    Ok((index, states))
}

fn write_state(out: &mut impl Write, state: &VolumeState) -> io::Result<()> {
    out.write_all(&u32::from(state.drive).to_le_bytes())?;
    out.write_all(&state.serial.to_le_bytes())?;
    let journal = state.journal.unwrap_or(Journal { id: 0, next_usn: 0 });
    out.write_all(&[u8::from(state.journal.is_some())])?;
    out.write_all(&journal.id.to_le_bytes())?;
    out.write_all(&journal.next_usn.to_le_bytes())?;
    out.write_all(&state.root.to_le_bytes())?;
    out.write_all(&(state.ids.len() as u64).to_le_bytes())?;
    for id in &state.ids {
        out.write_all(&id.to_le_bytes())?;
    }
    out.write_all(&(state.links.len() as u64).to_le_bytes())?;
    for (&record, ids) in &state.links {
        out.write_all(&(record as u64).to_le_bytes())?;
        out.write_all(&(ids.len() as u32).to_le_bytes())?;
        for id in ids {
            out.write_all(&id.to_le_bytes())?;
        }
    }
    Ok(())
}

fn read_states(input: &mut impl Read) -> io::Result<Vec<VolumeState>> {
    if &array::<8>(input)? != FILE_MAGIC {
        return Err(invalid("not a ZTHING index file".to_owned()));
    }
    let version = u32::from_le_bytes(array(input)?);
    if version != FILE_VERSION {
        return Err(invalid(format!(
            "file version {version}, this build reads version {FILE_VERSION}"
        )));
    }
    let count = u32::from_le_bytes(array(input)?);
    let mut states = Vec::new();
    for _ in 0..count {
        let drive = char::from_u32(u32::from_le_bytes(array(input)?))
            .ok_or_else(|| invalid("drive letter".to_owned()))?;
        let serial = u64::from_le_bytes(array(input)?);
        let has_journal = array::<1>(input)?[0] != 0;
        let journal = Journal {
            id: u64::from_le_bytes(array(input)?),
            next_usn: i64::from_le_bytes(array(input)?),
        };
        let root = u32::from_le_bytes(array(input)?);
        let id_count = u64::from_le_bytes(array(input)?);
        let mut bytes = Vec::new();
        let expected = id_count.saturating_mul(4);
        // Grows while reading, so a damaged count cannot force a huge allocation.
        input.take(expected).read_to_end(&mut bytes)?;
        if bytes.len() as u64 != expected {
            return Err(invalid("record map is truncated".to_owned()));
        }
        let ids = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&id| u32::from_le_bytes(id))
            .collect();
        let link_count = u64::from_le_bytes(array(input)?);
        let mut links = HashMap::new();
        for _ in 0..link_count {
            let record = usize::try_from(u64::from_le_bytes(array(input)?))
                .map_err(|_| invalid("hard link record".to_owned()))?;
            let count = u32::from_le_bytes(array(input)?);
            // Pushed one by one, so a damaged count fails at the end of the file.
            let mut more = Vec::new();
            for _ in 0..count {
                more.push(u32::from_le_bytes(array(input)?));
            }
            links.insert(record, more);
        }
        states.push(VolumeState {
            drive,
            serial,
            journal: has_journal.then_some(journal),
            root,
            ids,
            links,
        });
    }
    Ok(states)
}

fn array<const N: usize>(input: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn invalid(what: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}
