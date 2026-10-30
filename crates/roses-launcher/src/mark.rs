// The oldrusty mark: a flat geometric monogram drawn from code. A dark
// rounded square holds the wordmark's rust-red square brackets `[ ]` around a
// small gold "o" (the site's colours: --bg #09090b, --accent #d63a55, --gold
// #e5aa6c). No image file and no game asset is involved.
//
// This file is shared with build.rs (through `include!`) to make the exe
// icon, so it uses nothing outside `core`/`std`. `mark_svg` describes the very
// same shapes for the website (src/assets/brand/).

/// The mark's shapes in a 512-unit square (x, y, width, height).
#[allow(dead_code)]
const MARK_UNITS: f32 = 512.0;
#[allow(dead_code)]
const MARK_TILE_RADIUS: f32 = 112.0;
#[allow(dead_code)]
const MARK_BG: [u8; 3] = [0x09, 0x09, 0x0b];
#[allow(dead_code)]
const MARK_RUST: [u8; 3] = [0xd6, 0x3a, 0x55];
#[allow(dead_code)]
const MARK_GOLD: [u8; 3] = [0xe5, 0xaa, 0x6c];
/// The brackets: stems and arms, each a bar (x, y, w, h).
#[allow(dead_code)]
const MARK_BARS: [[f32; 4]; 6] = [
    [96.0, 112.0, 40.0, 288.0],
    [96.0, 112.0, 92.0, 40.0],
    [96.0, 360.0, 92.0, 40.0],
    [376.0, 112.0, 40.0, 288.0],
    [324.0, 112.0, 92.0, 40.0],
    [324.0, 360.0, 92.0, 40.0],
];
/// The "o": centre, outer radius and stroke.
#[allow(dead_code)]
const MARK_O: [f32; 4] = [256.0, 256.0, 62.0, 34.0];

#[allow(dead_code)]
fn mark_clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

/// Signed distance to a rounded box centred at (cx, cy).
#[allow(dead_code)]
fn mark_round_box(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (x - cx).abs() - (hw - r);
    let qy = (y - cy).abs() - (hh - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// Straight-alpha RGBA of the mark at (x, y) in mark units, `px` = one
/// output pixel in mark units.
#[allow(dead_code)]
fn mark_pixel(x: f32, y: f32, px: f32) -> [f32; 4] {
    let half = MARK_UNITS / 2.0;
    let tile = mark_clamp01(0.5 - mark_round_box(x, y, half, half, half, half, MARK_TILE_RADIUS) / px);
    if tile <= 0.0 {
        return [0.0; 4];
    }
    let mut bars = f32::MAX;
    for [bx, by, bw, bh] in MARK_BARS {
        bars = bars.min(mark_round_box(x, y, bx + bw / 2.0, by + bh / 2.0, bw / 2.0, bh / 2.0, 0.0));
    }
    let [ox, oy, r, stroke] = MARK_O;
    let ring = (((x - ox).powi(2) + (y - oy).powi(2)).sqrt() - (r - stroke / 2.0)).abs() - stroke / 2.0;
    let mut color = MARK_BG.map(|c| c as f32 / 255.0);
    for (d, paint) in [(bars, MARK_RUST), (ring, MARK_GOLD)] {
        let a = mark_clamp01(0.5 - d / px);
        for i in 0..3 {
            color[i] += (paint[i] as f32 / 255.0 - color[i]) * a;
        }
    }
    [color[0], color[1], color[2], tile]
}

/// The mark as straight-alpha RGBA bytes, `size` x `size`, row-major.
#[allow(dead_code)]
pub fn mark_rgba(size: u32) -> Vec<u8> {
    let n = size.max(1) as f32;
    let px = MARK_UNITS / n;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for j in 0..size {
        for i in 0..size {
            // 4x4 samples per pixel keep the small icon sizes even.
            let mut acc = [0.0f32; 4];
            for sj in 0..4 {
                for si in 0..4 {
                    let x = (i as f32 + (si as f32 + 0.5) / 4.0) * px;
                    let y = (j as f32 + (sj as f32 + 0.5) / 4.0) * px;
                    let [r, g, b, a] = mark_pixel(x, y, px / 4.0);
                    acc[0] += r * a;
                    acc[1] += g * a;
                    acc[2] += b * a;
                    acc[3] += a;
                }
            }
            let a = acc[3] / 16.0;
            let (r, g, b) = if acc[3] > 0.0 { (acc[0] / acc[3], acc[1] / acc[3], acc[2] / acc[3]) } else { (0.0, 0.0, 0.0) };
            for v in [r, g, b, a] {
                out.push((mark_clamp01(v) * 255.0 + 0.5) as u8);
            }
        }
    }
    out
}

/// The same mark as an SVG document (for the website's favicon).
#[allow(dead_code)]
pub fn mark_svg() -> String {
    let hex = |c: [u8; 3]| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 512 512\" width=\"512\" height=\"512\">\n  <title>oldrusty</title>\n  <rect width=\"512\" height=\"512\" rx=\"{}\" fill=\"{}\"/>\n  <g fill=\"{}\">\n",
        MARK_TILE_RADIUS,
        hex(MARK_BG),
        hex(MARK_RUST)
    );
    for [x, y, w, h] in MARK_BARS {
        svg.push_str(&format!("    <rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\"/>\n"));
    }
    let [ox, oy, r, stroke] = MARK_O;
    svg.push_str(&format!(
        "  </g>\n  <circle cx=\"{ox}\" cy=\"{oy}\" r=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{stroke}\"/>\n</svg>\n",
        r - stroke / 2.0,
        hex(MARK_GOLD)
    ));
    svg
}
