use std::collections::HashMap;
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
        let name_end = self.names.len() + name.len();
        if self.entries.len() >= REMOVED as usize || name_end > u32::MAX as usize {
            return Err(IndexError::Full {
                entries: self.entries.len(),
                name_bytes: name_end,
            });
        }
        self.entries.push(Entry {
            name_offset: self.names.len() as u32,
            name_len: name.len() as u32,
            parent: parent.unwrap_or(NO_PARENT),
            attributes,
            size,
            modified,
        });
        self.names.push_str(name);
        fold_case_into(&mut self.names_folded, name);
        self.folder_paths.take();
        Ok(self.entries.len() as u32 - 1)
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

    /// Releases the spare capacity that growing the buffers left behind.
    pub fn shrink_to_fit(&mut self) {
        self.names.shrink_to_fit();
        self.names_folded.shrink_to_fit();
        self.entries.shrink_to_fit();
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
