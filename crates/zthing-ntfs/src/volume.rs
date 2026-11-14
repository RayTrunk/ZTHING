//! All Windows calls of the crate. Each `unsafe` block only passes buffers
//! that stay alive and unaliased for the duration of a synchronous call.

use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileBasicInfo, FileIdType,
    FileStandardInfo, GetDriveTypeW, GetFileInformationByHandleEx, GetLogicalDrives,
    GetVolumeInformationW, OPEN_EXISTING, OpenFileById, ReadFile,
};
use windows::Win32::System::IO::{DeviceIoControl, OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
use windows::core::{HSTRING, Result};

/// `DRIVE_FIXED` from WinBase.h. The windows crate only exports it behind the
/// large `Win32_System_WindowsProgramming` feature.
const DRIVE_FIXED: u32 = 3;

pub fn fixed_drives() -> Vec<char> {
    // SAFETY: takes no arguments and returns a bit mask.
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| char::from(b'A' + bit))
        .filter(|drive| {
            let root = HSTRING::from(format!(r"{drive}:\"));
            // SAFETY: `root` is a null-terminated wide string that outlives the call.
            unsafe { GetDriveTypeW(&root) == DRIVE_FIXED }
        })
        .collect()
}

pub fn filesystem(drive: char) -> Result<String> {
    let root = HSTRING::from(format!(r"{drive}:\"));
    let mut name = [0u16; 261];
    // SAFETY: `root` and `name` outlive the call, `name` has the documented MAX_PATH + 1 size.
    unsafe { GetVolumeInformationW(&root, None, None, None, None, Some(&mut name)) }?;
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..len]))
}

pub struct Volume {
    handle: HANDLE,
}

impl Volume {
    pub fn open(drive: char) -> Result<Self> {
        let path = HSTRING::from(format!(r"\\.\{drive}:"));
        // SAFETY: `path` outlives the call. The handle is owned by `Volume` and closed in `Drop`.
        let handle = unsafe {
            CreateFileW(
                &path,
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }?;
        Ok(Self { handle })
    }

    /// Returns the number of bytes written to `output`.
    pub fn ioctl<T>(&self, code: u32, input: &T, output: &mut [u8]) -> Result<usize> {
        let mut returned = 0u32;
        // SAFETY: `input` and `output` are valid for their sizes until the synchronous call returns.
        unsafe {
            DeviceIoControl(
                self.handle,
                code,
                Some(std::ptr::from_ref(input).cast()),
                size_of::<T>() as u32,
                Some(output.as_mut_ptr().cast()),
                output.len() as u32,
                Some(&mut returned),
                None,
            )
        }?;
        Ok(returned as usize)
    }

    /// Reads raw volume bytes. `offset` and `buf.len()` must be multiples of the sector size.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut overlapped = OVERLAPPED {
            Anonymous: OVERLAPPED_0 {
                Anonymous: OVERLAPPED_0_0 {
                    Offset: offset as u32,
                    OffsetHigh: (offset >> 32) as u32,
                },
            },
            ..Default::default()
        };
        let mut read = 0u32;
        // SAFETY: the handle is synchronous, so `buf` and `overlapped` are only used until the call returns.
        unsafe {
            ReadFile(
                self.handle,
                Some(buf),
                Some(&mut read),
                Some(&mut overlapped),
            )
        }?;
        Ok(read as usize)
    }
}

impl Volume {
    /// Size and last write time (FILETIME) of a file on this volume, read through
    /// the file system and therefore never older than its cached metadata.
    pub fn file_info(&self, file_reference: u64) -> Result<(u64, i64)> {
        let id = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 {
                FileId: file_reference as i64,
            },
        };
        // SAFETY: `id` outlives the call. The returned handle is closed below.
        let file = unsafe {
            OpenFileById(
                self.handle,
                &id,
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        }?;
        let mut basic = FILE_BASIC_INFO::default();
        let mut standard = FILE_STANDARD_INFO::default();
        // SAFETY: each buffer has exactly the size of its information class.
        let result = unsafe {
            GetFileInformationByHandleEx(
                file,
                FileBasicInfo,
                std::ptr::from_mut(&mut basic).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
            .and_then(|()| {
                GetFileInformationByHandleEx(
                    file,
                    FileStandardInfo,
                    std::ptr::from_mut(&mut standard).cast(),
                    size_of::<FILE_STANDARD_INFO>() as u32,
                )
            })
        };
        // SAFETY: the handle came from OpenFileById and is closed exactly once.
        let _ = unsafe { CloseHandle(file) };
        result?;
        Ok((standard.EndOfFile as u64, basic.LastWriteTime))
    }
}

// SAFETY: a HANDLE is valid in every thread of the process. Win32 serializes
// the synchronous calls made on it, and `Volume` never hands out the raw handle.
unsafe impl Send for Volume {}
unsafe impl Sync for Volume {}

impl Drop for Volume {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed exactly once.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}
