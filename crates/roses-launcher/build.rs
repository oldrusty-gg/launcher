//! Embeds the launcher's own oldrusty mark (src/mark.rs, drawn from code) as the
//! executable's icon. The .res file is written by hand and handed to the MSVC
//! linker, so no resource compiler or extra crate is needed.

include!("src/mark.rs");

const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

fn main() {
    println!("cargo:rerun-if-changed=src/mark.rs");
    println!("cargo:rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let path = out.join("roses-launcher-icon.res");
    std::fs::write(&path, icon_res()).expect("write icon resource");
    println!("cargo:rustc-link-arg-bins={}", path.display());
    // The video recorder's Media Foundation DLLs load on first use, so
    // Windows N without the Media Feature Pack still starts the launcher
    // (scripts/release/audit.py checks this).
    for dll in ["mfplat.dll", "mfreadwrite.dll"] {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
    }
    println!("cargo:rustc-link-arg-bins=delayimp.lib");
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// One entry of a .res file with numeric type and name.
fn resource(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    push_u32(out, data.len() as u32);
    push_u32(out, 32);
    push_u16(out, 0xffff);
    push_u16(out, kind);
    push_u16(out, 0xffff);
    push_u16(out, id);
    push_u32(out, 0);
    push_u16(out, flags);
    push_u16(out, 0x0409);
    push_u32(out, 0);
    push_u32(out, 0);
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// A 32-bit icon image: BITMAPINFOHEADER, bottom-up BGRA, empty AND mask.
fn icon_dib(size: u32) -> Vec<u8> {
    let rgba = mark_rgba(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::new();
    push_u32(&mut out, 40);
    push_u32(&mut out, size);
    push_u32(&mut out, size * 2);
    push_u16(&mut out, 1);
    push_u16(&mut out, 32);
    push_u32(&mut out, 0);
    push_u32(&mut out, size * size * 4 + mask_row * size);
    for _ in 0..4 {
        push_u32(&mut out, 0);
    }
    for row in (0..size).rev() {
        for col in 0..size {
            let i = ((row * size + col) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    out.extend(std::iter::repeat_n(0u8, (mask_row * size) as usize));
    out
}

fn icon_res() -> Vec<u8> {
    let mut out = Vec::new();
    // The empty header that starts every 32-bit .res file.
    push_u32(&mut out, 0);
    push_u32(&mut out, 32);
    push_u16(&mut out, 0xffff);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0xffff);
    push_u16(&mut out, 0);
    for _ in 0..4 {
        push_u32(&mut out, 0);
    }
    let mut group = Vec::new();
    push_u16(&mut group, 0);
    push_u16(&mut group, 1);
    push_u16(&mut group, SIZES.len() as u16);
    for (index, size) in SIZES.into_iter().enumerate() {
        let id = index as u16 + 1;
        let image = icon_dib(size);
        resource(&mut out, 3, id, 0x1010, &image);
        let byte = if size >= 256 { 0 } else { size as u8 };
        group.extend_from_slice(&[byte, byte, 0, 0]);
        push_u16(&mut group, 1);
        push_u16(&mut group, 32);
        push_u32(&mut group, image.len() as u32);
        push_u16(&mut group, id);
    }
    resource(&mut out, 14, 1, 0x1030, &group);
    out
}
