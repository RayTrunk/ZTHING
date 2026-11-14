//! Loads the preview of the selected hit on a thread of its own, so slow
//! thumbnail providers (video, large PDFs) never stall the list.

use std::fs::{self, File};
use std::io::Read;
use std::os::windows::fs::MetadataExt;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::UNIX_EPOCH;

use eframe::egui;

use crate::shell;

/// Text previews show the start of a file only.
const TEXT_BYTES: u64 = 64 * 1024;
/// Longer side of thumbnails. The pane scales them down to its width.
const THUMBNAIL_SIZE: i32 = 512;
/// Set on cloud placeholders such as OneDrive files on demand. Reading their
/// content would download them.
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;

pub enum Content {
    Text(String),
    Image(egui::ColorImage),
    /// Neither text nor picture, for example a file that vanished.
    None,
}

pub struct Preview {
    pub path: String,
    pub content: Content,
    /// Unix seconds, `None` where the file system did not tell.
    pub created: Option<i64>,
    /// Windows file attributes, 0 if unknown.
    pub attributes: u32,
}

/// Answers each requested path, skipping paths that newer requests replaced.
pub fn spawn(ctx: egui::Context, requests: Receiver<String>, result: Arc<Mutex<Option<Preview>>>) {
    thread::spawn(move || {
        // Without COM only text previews work, which `load` copes with.
        let _ = shell::init_thread();
        while let Ok(mut path) = requests.recv() {
            while let Ok(newer) = requests.try_recv() {
                path = newer;
            }
            let preview = load(path);
            *result.lock().unwrap_or_else(PoisonError::into_inner) = Some(preview);
            ctx.request_repaint();
        }
    });
}

fn load(path: String) -> Preview {
    let metadata = fs::metadata(&path).ok();
    let attributes = metadata.as_ref().map_or(0, MetadataExt::file_attributes);
    let created = metadata
        .as_ref()
        .and_then(|metadata| metadata.created().ok())
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs() as i64);
    let placeholder =
        attributes & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_OFFLINE) != 0;
    let is_file = metadata.as_ref().is_some_and(fs::Metadata::is_file);

    let text = (is_file && !placeholder)
        .then(|| {
            let mut bytes = Vec::new();
            File::open(&path)
                .and_then(|file| file.take(TEXT_BYTES).read_to_end(&mut bytes))
                .ok()?;
            decode_text(&bytes)
        })
        .flatten();
    let content = match text {
        Some(text) => Content::Text(text),
        None => match shell::thumbnail(&path, THUMBNAIL_SIZE, placeholder) {
            Ok(thumbnail) => Content::Image(egui::ColorImage::from_rgba_unmultiplied(
                [thumbnail.width, thumbnail.height],
                &thumbnail.rgba,
            )),
            Err(_) => Content::None,
        },
    };
    Preview {
        path,
        content,
        created,
        attributes,
    }
}

/// Text in UTF-8 (with or without BOM) or UTF-16 with BOM. Anything else counts
/// as binary and gets a thumbnail instead.
fn decode_text(bytes: &[u8]) -> Option<String> {
    if let Some(units) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units = units
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair));
        return Some(
            char::decode_utf16(units)
                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect(),
        );
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if bytes.contains(&0) {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text.to_owned()),
        // The 64 KB cut may split the last character, which is still text.
        Err(e) if e.error_len().is_none() => {
            Some(String::from_utf8_lossy(&bytes[..e.valid_up_to()]).into_owned())
        }
        Err(_) => None,
    }
}

/// `R H S A` style letters for the attributes people care about.
pub fn attribute_letters(attributes: u32) -> String {
    [
        (0x1, 'R'),
        (0x2, 'H'),
        (0x4, 'S'),
        (0x20, 'A'),
        (0x800, 'C'),
        (0x4000, 'E'),
        (
            FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_OFFLINE,
            'O',
        ),
    ]
    .into_iter()
    .filter(|&(bit, _)| attributes & bit != 0)
    .map(|(_, letter)| letter)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_told_apart_from_binary() {
        assert_eq!(
            decode_text(b"fn main() {}\n").as_deref(),
            Some("fn main() {}\n")
        );
        assert_eq!(
            decode_text("\u{FEFF}Größe".as_bytes()).as_deref(),
            Some("Größe")
        );
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("Äpfel".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_text(&utf16).as_deref(), Some("Äpfel"));
        // Cut inside "ö", as the 64 KB limit may do.
        assert_eq!(
            decode_text(&"schön".as_bytes()[..4]).as_deref(),
            Some("sch")
        );
        assert_eq!(decode_text(b"MZ\x90\x00\x03"), None);
        assert_eq!(decode_text(&[0x25, 0x50, 0xC3, 0x28]), None);
    }

    #[test]
    fn attributes_become_letters() {
        assert_eq!(attribute_letters(0x1 | 0x2 | 0x20), "RHA");
        assert_eq!(attribute_letters(0x0040_0000), "O");
        assert_eq!(attribute_letters(0), "");
    }
}
