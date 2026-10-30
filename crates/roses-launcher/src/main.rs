#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg_attr(not(windows), allow(dead_code))]
mod canvas;
#[cfg_attr(not(windows), allow(dead_code))]
mod mark;
#[cfg_attr(not(windows), allow(dead_code))]
mod pad;
#[cfg_attr(not(windows), allow(dead_code))]
mod record;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
fn main() {
    if let Err(error) = windows::run() {
        windows::show_error(&error);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "The native Roses launcher currently supports Windows. Use the roses CLI on this platform."
    );
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_mark_is_an_opaque_tile_with_clear_rounded_corners() {
        let size = 64;
        let rgba = crate::mark::mark_rgba(size);
        let px = |x: u32, y: u32| &rgba[((y * size + x) * 4) as usize..((y * size + x) * 4 + 4) as usize];
        assert_eq!(px(32, 4)[3], 255, "the tile");
        assert_eq!(px(0, 0)[3], 0, "a rounded corner");
        assert_eq!(px(63, 63)[3], 0);
        // A bracket stem is rust red, the "o" ring gold, the tile dark.
        assert_eq!(px(14, 32), [0xd6, 0x3a, 0x55, 255]);
        assert_eq!(px(32, 32 - 6)[..3], [0xe5, 0xaa, 0x6c]);
        assert!(px(32, 32)[0] < 20, "the middle of the o is the tile");
        assert!(crate::mark::mark_svg().contains("#d63a55"));
    }

    /// `ROSES_WRITE_BRAND=<folder>`: writes the mark as `oldrusty-512.png`
    /// and `oldrusty.svg` (the website's src/assets/brand/).
    #[test]
    fn write_the_brand_files_when_asked() {
        let Some(dir) = std::env::var_os("ROSES_WRITE_BRAND") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let size = 512;
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, size, size);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&crate::mark::mark_rgba(size)).unwrap();
        }
        std::fs::write(dir.join("oldrusty-512.png"), png).unwrap();
        std::fs::write(dir.join("oldrusty.svg"), crate::mark::mark_svg()).unwrap();
    }
}
