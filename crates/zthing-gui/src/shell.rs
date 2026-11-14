//! The Windows shell calls of the window: thumbnails and the properties dialog.
//! Each `unsafe` block passes handles and buffers that stay valid for the
//! duration of a synchronous call.

use std::ptr;

use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SHOP_FILEPATH, SHObjectProperties, SIIGBF,
    SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY, SIIGBF_RESIZETOFIT,
};
use windows::core::{Error, HSTRING, PCWSTR, Result};

/// RGBA rows with straight (not premultiplied) alpha, top to bottom. Icons
/// come with colour in fully transparent pixels, which premultiplied drawing
/// would add onto the background.
pub struct Thumbnail {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// Prepares the calling thread for [`thumbnail`]. Thumbnail providers are COM
/// objects that expect a single-threaded apartment.
pub fn init_thread() -> Result<()> {
    // SAFETY: takes no pointers. The apartment lives as long as the thread.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok()
}

/// The picture Explorer shows for `path`, at most `size` pixels on the longer
/// side: a thumbnail where the file type has one, its icon otherwise.
/// `icon_only` avoids reading the file, which for a cloud placeholder would download it.
pub fn thumbnail(path: &str, size: i32, icon_only: bool) -> Result<Thumbnail> {
    // SAFETY: the path string outlives the call.
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(&HSTRING::from(path), None) }?;
    let flags: SIIGBF = if icon_only {
        SIIGBF_ICONONLY
    } else {
        SIIGBF_RESIZETOFIT | SIIGBF_BIGGERSIZEOK
    };
    // SAFETY: plain values in, a bitmap handle out that is deleted below.
    let bitmap = unsafe { factory.GetImage(SIZE { cx: size, cy: size }, flags) }?;
    let pixels = pixels(bitmap);
    // SAFETY: the bitmap came from GetImage and is deleted exactly once.
    let _ = unsafe { DeleteObject(bitmap.into()) };
    pixels
}

fn pixels(bitmap: HBITMAP) -> Result<Thumbnail> {
    let mut info = BITMAP::default();
    // SAFETY: `info` has exactly the size passed and outlives the call.
    let read = unsafe {
        GetObjectW(
            bitmap.into(),
            size_of::<BITMAP>() as i32,
            Some(ptr::from_mut(&mut info).cast()),
        )
    };
    if read == 0 || info.bmWidth <= 0 || info.bmHeight == 0 {
        return Err(Error::from_thread());
    }
    let (width, height) = (info.bmWidth, info.bmHeight.abs());
    let mut header = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative asks for rows top to bottom.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    // SAFETY: a screen-compatible memory DC, deleted below.
    let dc = unsafe { CreateCompatibleDC(None) };
    // SAFETY: `pixels` holds `height` rows of 32-bit pixels as `header` describes.
    let lines = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut header,
            DIB_RGB_COLORS,
        )
    };
    // SAFETY: the DC came from CreateCompatibleDC and is deleted exactly once.
    let _ = unsafe { DeleteDC(dc) };
    if lines == 0 {
        return Err(Error::from_thread());
    }
    // Bitmaps without an alpha channel come back with alpha 0 everywhere.
    let opaque = pixels.chunks_exact(4).all(|pixel| pixel[3] == 0);
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        if opaque {
            pixel[3] = 255;
        }
    }
    Ok(Thumbnail {
        width: width as usize,
        height: height as usize,
        rgba: pixels,
    })
}

/// Opens Explorer's properties dialog. It runs on its own thread and stays
/// open after this call returns.
pub fn properties(path: &str) -> Result<()> {
    // SAFETY: the path string outlives the call. No owner window, no start page.
    unsafe { SHObjectProperties(None, SHOP_FILEPATH, &HSTRING::from(path), PCWSTR::null()) }.ok()
}
