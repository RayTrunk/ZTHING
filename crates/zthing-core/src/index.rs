use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::ops::Range;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;

use crate::query::{Kind, Only, Query, Term};

/// Windows `FILE_ATTRIBUTE_DIRECTORY`. Attributes keep the raw NTFS bit values.
pub const ATTRIBUTE_DIRECTORY: u32 = 0x10;

const NO_PARENT: u32 = u32::MAX;
/// Parent value of a removed entry. Ids stay stable, so parent links and the
/// record maps of the volume readers remain valid.
const REMOVED: u32 = u32::MAX - 1;

/// Entries per parallel work unit. Cancellation is checked once per chunk,
/// so this also bounds how long a superseded search keeps running.
const CHUNK: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Path,
    Size,
    Modified,
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index full: {entries} entries and {name_bytes} name bytes exceed 32-bit addressing")]
    Full { entries: usize, name_bytes: usize },
}

const FORMAT_MAGIC: &[u8; 8] = b"ZTHINGIX";
/// Raised whenever the layout written by [`Index::save`] changes.
pub const FORMAT_VERSION: u32 = 1;
const ENTRY_BYTES: usize = 32;
/// Entries parsed per read while loading.
const LOAD_CHUNK: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("reading the index: {0}")]
    Io(#[from] io::Error),
    #[error("index file has format version {found}, this build reads version {FORMAT_VERSION}")]
    Version { found: u32 },
    #[error("index file is damaged: {0}")]
    Damaged(&'static str),
}

fn read_array<const N: usize>(input: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Room for about 6 % more names and entries. Without it the first live update
/// after a load doubles every buffer, which briefly needs three times their size.
fn with_headroom(len: usize) -> usize {
    len + len / 16
}

fn fill(input: &mut impl Read, buf: &mut [u8]) -> Result<(), LoadError> {
    input.read_exact(buf).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => LoadError::Damaged("file is truncated"),
        _ => LoadError::Io(e),
    })
}

#[derive(Debug)]
struct Entry {
    name_offset: u32,
    name_len: u32,
    parent: u32,
    attributes: u32,
    size: u64,
    modified: i64,
}

/// All names share one buffer, so an entry costs no heap allocation of its own.
/// File paths are not stored and get rebuilt from the parent chain.
#[derive(Debug, Default)]
pub struct Index {
    names: String,
    /// Case-folded copy with the same byte layout as `names`, so one offset addresses both.
    names_folded: String,
    entries: Vec<Entry>,
    /// Built by the first `path:` search and dropped on every change to the entries.
    folder_paths: OnceLock<FolderPaths>,
}

/// Case-folded full paths of all folders. A file's path is its parent's path
/// plus its name, so `path:` searches skip walking the parent chain.
#[derive(Debug, Default)]
struct FolderPaths {
    text: String,
    spans: HashMap<u32, Range<usize>>,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    /// `parent` may refer to an entry that is pushed later.
    /// `modified` is in Unix seconds (UTC).
    pub fn push(
        &mut self,
        name: &str,
        parent: Option<u32>,
        attributes: u32,
        size: u64,
        modified: i64,
    ) -> Result<u32, IndexError> {
        if self.entries.len() >= REMOVED as usize {
            return Err(self.full(name));
        }
        let name_offset = self.append_name(name)?;
        self.entries.push(Entry {
            name_offset,
            name_len: name.len() as u32,
            parent: parent.unwrap_or(NO_PARENT),
            attributes,
            size,
            modified,
        });
        Ok(self.entries.len() as u32 - 1)
    }

    /// The old name stays in the buffers as garbage until the index is rebuilt.
    pub fn set_name(&mut self, id: u32, name: &str) -> Result<(), IndexError> {
        let name_offset = self.append_name(name)?;
        let entry = &mut self.entries[id as usize];
        entry.name_offset = name_offset;
        entry.name_len = name.len() as u32;
        Ok(())
    }

    fn append_name(&mut self, name: &str) -> Result<u32, IndexError> {
        if self.names.len() + name.len() > u32::MAX as usize {
            return Err(self.full(name));
        }
        let offset = self.names.len() as u32;
        self.names.push_str(name);
        fold_case_into(&mut self.names_folded, name);
        self.folder_paths.take();
        Ok(offset)
    }

    fn full(&self, name: &str) -> IndexError {
        IndexError::Full {
            entries: self.entries.len(),
            name_bytes: self.names.len() + name.len(),
        }
    }

    pub fn removed_count(&self) -> usize {
        self.entries.iter().filter(|e| e.parent == REMOVED).count()
    }

    /// Writes the format that [`Index::load`] reads. The folded names are
    /// rebuilt on load, which keeps the file a third smaller.
    pub fn save(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(FORMAT_MAGIC)?;
        out.write_all(&FORMAT_VERSION.to_le_bytes())?;
        out.write_all(&(self.entries.len() as u64).to_le_bytes())?;
        out.write_all(&(self.names.len() as u64).to_le_bytes())?;
        out.write_all(self.names.as_bytes())?;
        for entry in &self.entries {
            let mut bytes = [0u8; ENTRY_BYTES];
            bytes[0..4].copy_from_slice(&entry.name_offset.to_le_bytes());
            bytes[4..8].copy_from_slice(&entry.name_len.to_le_bytes());
            bytes[8..12].copy_from_slice(&entry.parent.to_le_bytes());
            bytes[12..16].copy_from_slice(&entry.attributes.to_le_bytes());
            bytes[16..24].copy_from_slice(&entry.size.to_le_bytes());
            bytes[24..32].copy_from_slice(&entry.modified.to_le_bytes());
            out.write_all(&bytes)?;
        }
        Ok(())
    }

    /// Reads what [`Index::save`] wrote. Every offset and parent is checked, so
    /// a damaged file yields an error instead of a later panic.
    pub fn load(input: &mut impl Read) -> Result<Self, LoadError> {
        if &read_array::<8>(input)? != FORMAT_MAGIC {
            return Err(LoadError::Damaged("not a ZTHING index"));
        }
        let version = u32::from_le_bytes(read_array(input)?);
        if version != FORMAT_VERSION {
            return Err(LoadError::Version { found: version });
        }
        let too_large = |_| LoadError::Damaged("length too large");
        let count = usize::try_from(u64::from_le_bytes(read_array(input)?)).map_err(too_large)?;
        let names_len =
            usize::try_from(u64::from_le_bytes(read_array(input)?)).map_err(too_large)?;
        // Exact reservations keep the peak memory at the final size. A damaged
        // length makes them fail with an error instead of aborting the process.
        let mut names = Vec::new();
        names
            .try_reserve_exact(with_headroom(names_len))
            .map_err(|_| LoadError::Damaged("length too large"))?;
        names.resize(names_len, 0);
        fill(input, &mut names)?;
        let names =
            String::from_utf8(names).map_err(|_| LoadError::Damaged("names are not UTF-8"))?;

        let mut entries = Vec::new();
        entries
            .try_reserve_exact(with_headroom(count))
            .map_err(|_| LoadError::Damaged("length too large"))?;
        let mut chunk = vec![0u8; ENTRY_BYTES * LOAD_CHUNK];
        while entries.len() < count {
            let len = (count - entries.len()).min(LOAD_CHUNK) * ENTRY_BYTES;
            fill(input, &mut chunk[..len])?;
            for bytes in chunk[..len].chunks_exact(ENTRY_BYTES) {
                let u32_at = |at: usize| {
                    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
                };
                let u64_at = |at: usize| {
                    let mut field = [0u8; 8];
                    field.copy_from_slice(&bytes[at..at + 8]);
                    field
                };
                let entry = Entry {
                    name_offset: u32_at(0),
                    name_len: u32_at(4),
                    parent: u32_at(8),
                    attributes: u32_at(12),
                    size: u64::from_le_bytes(u64_at(16)),
                    modified: i64::from_le_bytes(u64_at(24)),
                };
                let start = entry.name_offset as usize;
                if !names.is_char_boundary(start)
                    || !names.is_char_boundary(start + entry.name_len as usize)
                {
                    return Err(LoadError::Damaged("name outside the name buffer"));
                }
                if entry.parent as usize >= count && entry.parent < REMOVED {
                    return Err(LoadError::Damaged("parent outside the index"));
                }
                entries.push(entry);
            }
        }
        let mut names_folded = String::with_capacity(with_headroom(names.len()));
        fold_case_into(&mut names_folded, &names);
        Ok(Self {
            names,
            names_folded,
            entries,
            folder_paths: OnceLock::new(),
        })
    }

    pub fn set_parent(&mut self, id: u32, parent: Option<u32>) {
        self.entries[id as usize].parent = parent.unwrap_or(NO_PARENT);
        self.folder_paths.take();
    }

    /// Hides the entry from searches. Its name stays in the buffers.
    pub fn remove(&mut self, id: u32) {
        self.entries[id as usize].parent = REMOVED;
        self.folder_paths.take();
    }

    pub fn is_removed(&self, id: u32) -> bool {
        self.entries[id as usize].parent == REMOVED
    }

    pub fn set_attributes(&mut self, id: u32, attributes: u32) {
        self.entries[id as usize].attributes = attributes;
        self.folder_paths.take();
    }

    pub fn set_size(&mut self, id: u32, size: u64) {
        self.entries[id as usize].size = size;
    }

    /// `modified` is in Unix seconds (UTC).
    pub fn set_modified(&mut self, id: u32, modified: i64) {
        self.entries[id as usize].modified = modified;
    }

    /// Sorts ascending. Name and path order ignores case.
    pub fn sort(&self, ids: &mut [u32], key: SortKey) {
        match key {
            SortKey::Name => ids.par_sort_by(|&a, &b| {
                self.name_in(&self.names_folded, a)
                    .cmp(self.name_in(&self.names_folded, b))
            }),
            SortKey::Path => ids.par_sort_by_cached_key(|&id| {
                let mut path = String::new();
                self.write_path(id, &self.names_folded, &mut path);
                path
            }),
            SortKey::Size => ids.par_sort_by_key(|&id| self.entries[id as usize].size),
            SortKey::Modified => ids.par_sort_by_key(|&id| self.entries[id as usize].modified),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn name(&self, id: u32) -> &str {
        self.name_in(&self.names, id)
    }

    pub fn path(&self, id: u32) -> String {
        let mut path = String::new();
        self.write_path(id, &self.names, &mut path);
        path
    }

    pub fn is_dir(&self, id: u32) -> bool {
        self.entries[id as usize].attributes & ATTRIBUTE_DIRECTORY != 0
    }

    pub fn size(&self, id: u32) -> u64 {
        self.entries[id as usize].size
    }

    pub fn modified(&self, id: u32) -> i64 {
        self.entries[id as usize].modified
    }

    /// Releases the spare capacity that growing the buffers left behind, except
    /// for the headroom that live updates need.
    pub fn shrink_to_fit(&mut self) {
        self.names.shrink_to(with_headroom(self.names.len()));
        self.names_folded
            .shrink_to(with_headroom(self.names_folded.len()));
        self.entries.shrink_to(with_headroom(self.entries.len()));
    }

    pub fn heap_size(&self) -> usize {
        self.names.capacity()
            + self.names_folded.capacity()
            + self.entries.capacity() * size_of::<Entry>()
            + self.folder_paths.get().map_or(0, |paths| {
                paths.text.capacity() + paths.spans.capacity() * size_of::<(u32, Range<usize>)>()
            })
    }

    /// Returns matching ids in index order, or `None` once `cancel` is set.
    /// `within` restricts the search to earlier results, see [`Query::narrows`].
    pub fn search(
        &self,
        query: &Query,
        within: Option<&[u32]>,
        cancel: &AtomicBool,
    ) -> Option<Vec<u32>> {
        // Groups that build paths or run regexes go last, so that entries already
        // rejected by a cheap substring group never pay for them.
        let mut groups: Vec<&[Term]> = query.groups.iter().map(Vec::as_slice).collect();
        groups.sort_by_key(|group| {
            group
                .iter()
                .any(|term| term.path || matches!(term.kind, Kind::Regex(_)))
        });
        let filter = |ids: &mut dyn Iterator<Item = u32>| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let mut scratch = String::new();
            Some(
                ids.filter(|&id| !self.is_removed(id) && self.matches(id, &groups, &mut scratch))
                    .collect::<Vec<_>>(),
            )
        };
        let chunks: Option<Vec<Vec<u32>>> = match within {
            Some(ids) => ids
                .par_chunks(CHUNK)
                .map(|chunk| filter(&mut chunk.iter().copied()))
                .collect(),
            None => (0..self.entries.len().div_ceil(CHUNK))
                .into_par_iter()
                .map(|chunk| {
                    let end = ((chunk + 1) * CHUNK).min(self.entries.len());
                    filter(&mut (chunk * CHUNK..end).map(|id| id as u32))
                })
                .collect(),
        };
        Some(chunks?.concat())
    }

    fn matches(&self, id: u32, groups: &[&[Term]], scratch: &mut String) -> bool {
        groups.iter().all(|group| {
            group
                .iter()
                .any(|term| self.term_matches(id, term, scratch) != term.negate)
        })
    }

    fn term_matches(&self, id: u32, term: &Term, scratch: &mut String) -> bool {
        let entry = &self.entries[id as usize];
        let is_dir = entry.attributes & ATTRIBUTE_DIRECTORY != 0;
        match term.only {
            Some(Only::Files) if is_dir => return false,
            Some(Only::Folders) if !is_dir => return false,
            _ => {}
        }
        match &term.kind {
            Kind::All => true,
            Kind::Text(finder) => finder
                .find(self.haystack(id, term.path, !term.case, scratch))
                .is_some(),
            // A case-insensitive regex matches folded text exactly like the original,
            // and the folded text is what the folder path cache holds.
            Kind::Regex(regex) => regex.is_match(self.haystack(id, term.path, !term.case, scratch)),
            Kind::Ext(exts) => self
                .name_in(&self.names_folded, id)
                .rsplit_once('.')
                .is_some_and(|(_, ext)| exts.iter().any(|e| e == ext)),
            // Folder sizes are not indexed, so size filters only consider files.
            Kind::Size(range) => !is_dir && range.contains(&entry.size),
            Kind::Modified(range) => range.contains(&entry.modified),
        }
    }

    fn haystack<'a>(
        &'a self,
        id: u32,
        path: bool,
        folded: bool,
        scratch: &'a mut String,
    ) -> &'a [u8] {
        let names = if folded {
            &self.names_folded
        } else {
            &self.names
        };
        if !path {
            return self.name_in(names, id).as_bytes();
        }
        scratch.clear();
        if folded {
            let paths = self.folder_paths();
            if let Some(span) = paths.spans.get(&id) {
                return paths.text[span.clone()].as_bytes();
            }
            if let Some(span) = paths.spans.get(&self.entries[id as usize].parent) {
                scratch.push_str(&paths.text[span.clone()]);
                scratch.push('\\');
                scratch.push_str(self.name_in(names, id));
                return scratch.as_bytes();
            }
        }
        self.write_path(id, names, scratch);
        scratch.as_bytes()
    }

    fn folder_paths(&self) -> &FolderPaths {
        self.folder_paths.get_or_init(|| {
            let mut paths = FolderPaths::default();
            for id in 0..self.entries.len() as u32 {
                if self.is_dir(id) && !self.is_removed(id) {
                    let start = paths.text.len();
                    self.write_path(id, &self.names_folded, &mut paths.text);
                    paths.spans.insert(id, start..paths.text.len());
                }
            }
            paths
        })
    }

    fn write_path(&self, id: u32, names: &str, out: &mut String) {
        let parent = self.entries[id as usize].parent;
        if parent != NO_PARENT && parent != REMOVED {
            self.write_path(parent, names, out);
            out.push('\\');
        }
        out.push_str(self.name_in(names, id));
    }

    fn name_in<'a>(&self, names: &'a str, id: u32) -> &'a str {
        let entry = &self.entries[id as usize];
        let start = entry.name_offset as usize;
        &names[start..start + entry.name_len as usize]
    }
}

/// Lowercases per char but keeps chars whose lowercase form has a different
/// UTF-8 length (such as 'İ'), so folded and original names share byte offsets.
pub(crate) fn fold_case_into(out: &mut String, s: &str) {
    for c in s.chars() {
        let mut lower = c.to_lowercase();
        match (lower.next(), lower.next()) {
            (Some(l), None) if l.len_utf8() == c.len_utf8() => out.push(l),
            _ => out.push(c),
        }
    }
}
