use std::error::Error;
use std::path::Path;

use embed_manifest::manifest::ExecutionLevel;
use embed_manifest::{embed_manifest, new_manifest};
use resvg::{tiny_skia, usvg};

/// Window and taskbar icon. Windows scales it down for the title bar.
/// Must match `WINDOW_ICON_SIZE` in main.rs.
const WINDOW_ICON_SIZE: u32 = 256;
/// Tray icons show at 16 to 32 pixels depending on the display scale.
/// Must match `TRAY_ICON_SIZE` in main.rs.
const TRAY_ICON_SIZE: u32 = 32;

fn main() -> Result<(), Box<dyn Error>> {
    // Reading the $MFT needs administrator rights until the service of milestone 5 indexes.
    embed_manifest(
        new_manifest("ZTHING").requested_execution_level(ExecutionLevel::RequireAdministrator),
    )?;

    // Rendered here, so the program needs no SVG renderer at run time.
    let out = std::env::var("OUT_DIR")?;
    render(
        "assets/icon.svg",
        WINDOW_ICON_SIZE,
        &Path::new(&out).join("window_icon.rgba"),
    )?;
    render(
        "assets/tray_icon.svg",
        TRAY_ICON_SIZE,
        &Path::new(&out).join("tray_icon.rgba"),
    )?;
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets");
    Ok(())
}

/// Writes straight (not premultiplied) RGBA, the format window and tray icons take.
fn render(svg: &str, size: u32, target: &Path) -> Result<(), Box<dyn Error>> {
    let tree = usvg::Tree::from_data(&std::fs::read(svg)?, &usvg::Options::default())?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or("icon size is zero")?;
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let rgba: Vec<u8> = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.red(), color.green(), color.blue(), color.alpha()]
        })
        .collect();
    std::fs::write(target, rgba)?;
    Ok(())
}
