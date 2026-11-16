//! Draws the window's icon files from their SVG sources in `icons/`:
//! `cargo run -p multibox-ui --example icons`.
//!
//! - `multibox.ico`, built into the Windows program by `build.rs`. Up to 32 px
//!   it takes the bolder `multibox-small.svg`, above that `multibox.svg`.
//! - `multibox-macos.png`, which the program hands to the Dock on macOS: it
//!   runs as a bare executable, with no bundle to take an icon from.
//! - `multibox.icns`, the same picture in every size an app bundle uses.

use resvg::{tiny_skia, usvg};
use std::path::Path;

fn render(svg: &Path, size: u32) -> Vec<u8> {
    let data = std::fs::read(svg).unwrap_or_else(|err| panic!("{}: {err}", svg.display()));
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default())
        .unwrap_or_else(|err| panic!("{}: {err}", svg.display()));
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("a nonzero size");
    let scale = size as f32 / tree.size().width();
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().expect("PNG encoding")
}

/// An ICO file of PNG images, which Windows has read since Vista.
fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let count = u16::try_from(images.len()).expect("a few images");
    let mut out = Vec::new();
    for word in [0, 1, count] {
        out.extend(word.to_le_bytes());
    }
    let mut offset = 6 + 16 * images.len();
    for (size, png) in images {
        // 256 is written as 0.
        let side = u8::try_from(*size).unwrap_or(0);
        out.extend([side, side, 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend(u32::try_from(png.len()).unwrap().to_le_bytes());
        out.extend(u32::try_from(offset).unwrap().to_le_bytes());
        offset += png.len();
    }
    for (_, png) in images {
        out.extend(png);
    }
    out
}

/// An ICNS file of PNG images, under the types macOS looks for.
fn icns(images: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let total = 8 + images.iter().map(|(_, png)| 8 + png.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend(b"icns");
    out.extend(u32::try_from(total).unwrap().to_be_bytes());
    for (kind, png) in images {
        out.extend(*kind);
        out.extend(u32::try_from(8 + png.len()).unwrap().to_be_bytes());
        out.extend(png);
    }
    out
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    println!("{} ({} KB)", path.display(), bytes.len().div_ceil(1024));
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("icons");
    let small = dir.join("multibox-small.svg");
    let tile = dir.join("multibox.svg");
    let mac = dir.join("multibox-macos.svg");

    let windows: Vec<(u32, Vec<u8>)> = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
        .into_iter()
        .map(|size| {
            let source = if size <= 32 { &small } else { &tile };
            (size, render(source, size))
        })
        .collect();
    write(&dir.join("multibox.ico"), &ico(&windows));

    write(&dir.join("multibox-macos.png"), &render(&mac, 512));

    let bundle: Vec<(&[u8; 4], Vec<u8>)> = [
        (b"icp4", 16),
        (b"ic11", 32),
        (b"icp5", 32),
        (b"ic12", 64),
        (b"icp6", 64),
        (b"ic07", 128),
        (b"ic13", 256),
        (b"ic08", 256),
        (b"ic14", 512),
        (b"ic09", 512),
        (b"ic10", 1024),
    ]
    .into_iter()
    .map(|(kind, size)| (kind, render(&mac, size)))
    .collect();
    write(&dir.join("multibox.icns"), &icns(&bundle));
}
