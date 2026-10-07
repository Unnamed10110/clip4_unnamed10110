//! Build script: generates the tray/exe icon procedurally, then embeds icon + manifest
//! (PerMonitorV2, asInvoker, Common Controls v6) as resources.

use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/clip4.manifest");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_else(|_| ".".into()));

    fs::write(out.join("clip4.ico"), make_ico()).expect("write ico");
    fs::copy("assets/clip4.manifest", out.join("clip4.manifest")).expect("copy manifest");
    // Relative file names resolve against the .rc's own directory.
    fs::write(out.join("clip4.rc"), "1 ICON \"clip4.ico\"\n1 24 \"clip4.manifest\"\n").expect("write rc");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let _ = embed_resource::compile_for(out.join("clip4.rc"), ["clip4"], embed_resource::NONE);
    }
}

// ---------------------------------------------------------------- clip4.ico

fn rrect(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let (hx, hy) = ((x1 - x0) / 2.0 - r, (y1 - y0) / 2.0 - r);
    let (dx, dy) = ((px - cx).abs() - hx, (py - cy).abs() - hy);
    let (ox, oy) = (dx.max(0.0), dy.max(0.0));
    (ox * ox + oy * oy).sqrt() + dx.max(dy).min(0.0) <= r
}

/// Colour at (u, v) in 0..1 space, or None for transparent: dark board, accent clip, text lines.
fn glyph(u: f32, v: f32) -> Option<[u8; 3]> {
    const ACCENT: [u8; 3] = [0x00, 0xE0, 0x6A];
    if rrect(u, v, 0.34, 0.06, 0.66, 0.24, 0.07) {
        return Some(ACCENT);
    }
    if rrect(u, v, 0.14, 0.14, 0.86, 0.94, 0.12) {
        for (i, w) in [(0usize, 0.50f32), (1, 0.50), (2, 0.34)] {
            let y = 0.42 + i as f32 * 0.17;
            if rrect(u, v, 0.28, y - 0.035, 0.28 + w, y + 0.035, 0.035) {
                return Some(ACCENT);
            }
        }
        return Some([0x1E, 0x1F, 0x24]);
    }
    None
}

fn make_ico() -> Vec<u8> {
    let sizes = [16u32, 24, 32, 48, 64];
    let mut images: Vec<Vec<u8>> = Vec::new();
    for &s in &sizes {
        let mut xor = vec![0u8; (s * s * 4) as usize];
        let ss = 4u32;
        for y in 0..s {
            for x in 0..s {
                let (mut r, mut g, mut b, mut a) = (0f32, 0f32, 0f32, 0f32);
                for sy in 0..ss {
                    for sx in 0..ss {
                        let u = (x as f32 + (sx as f32 + 0.5) / ss as f32) / s as f32;
                        let v = (y as f32 + (sy as f32 + 0.5) / ss as f32) / s as f32;
                        if let Some(c) = glyph(u, v) {
                            r += c[0] as f32;
                            g += c[1] as f32;
                            b += c[2] as f32;
                            a += 1.0;
                        }
                    }
                }
                let total = (ss * ss) as f32;
                // BMP rows are bottom-up, pixels BGRA (straight alpha).
                let row = s - 1 - y;
                let o = ((row * s + x) * 4) as usize;
                if a > 0.0 {
                    xor[o] = (b / a) as u8;
                    xor[o + 1] = (g / a) as u8;
                    xor[o + 2] = (r / a) as u8;
                    xor[o + 3] = (a / total * 255.0) as u8;
                }
            }
        }
        let mask_stride = s.div_ceil(32) * 4;
        let mut img = Vec::new();
        img.extend_from_slice(&40u32.to_le_bytes());
        img.extend_from_slice(&(s as i32).to_le_bytes());
        img.extend_from_slice(&((s * 2) as i32).to_le_bytes());
        img.extend_from_slice(&1u16.to_le_bytes());
        img.extend_from_slice(&32u16.to_le_bytes());
        img.extend_from_slice(&[0u8; 24]);
        img.extend_from_slice(&xor);
        img.extend_from_slice(&vec![0u8; (mask_stride * s) as usize]);
        images.push(img);
    }
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (i, &s) in sizes.iter().enumerate() {
        ico.push(s as u8);
        ico.push(s as u8);
        ico.extend_from_slice(&[0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(images[i].len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += images[i].len() as u32;
    }
    for img in images {
        ico.extend_from_slice(&img);
    }
    ico
}
