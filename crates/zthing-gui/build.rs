use std::error::Error;
use std::path::Path;

use resvg::{tiny_skia, usvg};

/// Window and taskbar icon. Windows scales it down for the title bar.
/// Must match `WINDOW_ICON_SIZE` in lib.rs.
const WINDOW_ICON_SIZE: u32 = 256;
/// Tray icons show at 16 to 32 pixels depending on the display scale.
/// Must match `TRAY_ICON_SIZE` in lib.rs.
const TRAY_ICON_SIZE: u32 = 32;
/// Sizes Explorer picks from for the program icon, from list view to large tiles.
const EXE_ICON_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_MANIFEST: u16 = 24;
const LANG_EN_US: u16 = 0x0409;
/// The service reads the drives, so the window runs with the user's rights.
/// The rest is what Windows 10 and 11 programs declare: modern common
/// controls for dialogs and menus, per-monitor DPI, UTF-8 and long paths.
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="ZTHING" version="1.0.0.0"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#;

fn main() -> Result<(), Box<dyn Error>> {
    // Rendered here, so the program needs no SVG renderer at run time.
    let out = std::env::var("OUT_DIR")?;
    let out = Path::new(&out);
    let logo = usvg::Tree::from_data(
        &std::fs::read("assets/icon.svg")?,
        &usvg::Options::default(),
    )?;
    let tray = usvg::Tree::from_data(
        &std::fs::read("assets/tray_icon.svg")?,
        &usvg::Options::default(),
    )?;
    std::fs::write(
        out.join("window_icon.rgba"),
        straight_rgba(&render(&logo, WINDOW_ICON_SIZE)?),
    )?;
    std::fs::write(
        out.join("tray_icon.rgba"),
        straight_rgba(&render(&tray, TRAY_ICON_SIZE)?),
    )?;

    let pngs = EXE_ICON_SIZES
        .iter()
        .map(|&size| Ok((size, render(&logo, size)?.encode_png()?)))
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    // zthing.ico for the installer, zthing.res linked into the program.
    std::fs::write(out.join("zthing.ico"), ico(&pngs))?;
    let res = out.join("zthing.res");
    std::fs::write(&res, resources(&pngs))?;
    // link.exe takes compiled resource files as input. Test binaries need no icon.
    println!("cargo:rustc-link-arg-bins={}", res.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets");
    Ok(())
}

fn render(tree: &usvg::Tree, size: u32) -> Result<tiny_skia::Pixmap, Box<dyn Error>> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or("icon size is zero")?;
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap)
}

/// Window and tray icons take straight (not premultiplied) RGBA.
fn straight_rgba(pixmap: &tiny_skia::Pixmap) -> Vec<u8> {
    pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.red(), color.green(), color.blue(), color.alpha()]
        })
        .collect()
}

/// The directory entry fields shared by .ico files and RT_GROUP_ICON: width,
/// height (0 means 256), colour count, reserved, planes, bits per pixel, bytes.
fn icon_entry(out: &mut Vec<u8>, size: u32, bytes: usize) {
    let side = if size >= 256 { 0 } else { size as u8 };
    out.extend([side, side, 0, 0]);
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend((bytes as u32).to_le_bytes());
}

/// An .ico file with PNG images, which Windows reads since Vista.
fn ico(pngs: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend([0, 0, 1, 0]);
    out.extend((pngs.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * pngs.len();
    for (size, png) in pngs {
        icon_entry(&mut out, *size, png.len());
        out.extend((offset as u32).to_le_bytes());
        offset += png.len();
    }
    for (_, png) in pngs {
        out.extend(png);
    }
    out
}

/// A compiled resource file (.res) with the icon images, the icon group that
/// Explorer shows for the program, and the manifest.
fn resources(pngs: &[(u32, Vec<u8>)]) -> Vec<u8> {
    // A .res file starts with an empty entry that marks the format.
    let mut out = Vec::new();
    resource(&mut out, 0, 0, 0, &[]);
    let mut group = Vec::new();
    group.extend([0, 0, 1, 0]);
    group.extend((pngs.len() as u16).to_le_bytes());
    for (id, (size, png)) in (1u16..).zip(pngs) {
        resource(&mut out, RT_ICON, id, LANG_EN_US, png);
        icon_entry(&mut group, *size, png.len());
        group.extend(id.to_le_bytes());
    }
    resource(&mut out, RT_GROUP_ICON, 1, LANG_EN_US, &group);
    resource(&mut out, RT_MANIFEST, 1, LANG_EN_US, MANIFEST.as_bytes());
    out
}

/// One entry with a numeric type and name. Every header and every data block
/// starts on a four-byte boundary.
fn resource(out: &mut Vec<u8>, kind: u16, name: u16, language: u16, data: &[u8]) {
    const HEADER_SIZE: u32 = 32;
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(HEADER_SIZE.to_le_bytes());
    out.extend([0xFF, 0xFF]);
    out.extend(kind.to_le_bytes());
    out.extend([0xFF, 0xFF]);
    out.extend(name.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // data version
    // MOVEABLE | PURE | DISCARDABLE, except for the empty entry.
    let flags: u16 = if kind == 0 { 0 } else { 0x1030 };
    out.extend(flags.to_le_bytes());
    out.extend(language.to_le_bytes());
    out.extend(0u32.to_le_bytes()); // version
    out.extend(0u32.to_le_bytes()); // characteristics
    out.extend(data);
    out.resize(out.len().next_multiple_of(4), 0);
}
