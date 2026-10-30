//! A tiny software painter over 32-bit 0x00RRGGBB pixels (the layout of a
//! top-down GDI DIB section). Everything the launcher draws that GDI cannot
//! anti-alias or blend - backdrops, translucent panels, rounded buttons and
//! icons - goes through here; GDI then draws the text on top.

#[derive(Clone)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA, row-major.
    pub data: Vec<u8>,
}

impl Rgba {
    /// The `src` part (x, y, w, h) shrunk to `width` x `height` by exact area
    /// averaging (a box filter over each output pixel's whole footprint, in
    /// premultiplied alpha): no aliasing or shimmer at large reductions, where
    /// bilinear sampling would skip most source pixels.
    pub fn area_scaled(&self, src: (i32, i32, i32, i32), width: u32, height: u32) -> Rgba {
        let (sx, sy, sw, sh) = src;
        let mut data = vec![0u8; (width * height * 4) as usize];
        if self.width == 0 || self.height == 0 || sw <= 0 || sh <= 0 {
            return Rgba { width, height, data };
        }
        let (kx, ky) = (sw as f64 / width as f64, sh as f64 / height as f64);
        // The covered source spans (index, weight) of an output column or row.
        let spans = |start: i32, k: f64, n: u32, limit: u32| -> Vec<Vec<(usize, f64)>> {
            (0..n)
                .map(|o| {
                    let (a, b) = (start as f64 + o as f64 * k, start as f64 + (o + 1) as f64 * k);
                    let mut out = Vec::new();
                    let mut p = a.floor();
                    while p < b {
                        let w = (p + 1.0).min(b) - p.max(a);
                        if w > 0.0 {
                            out.push(((p as i64).clamp(0, limit as i64 - 1) as usize, w));
                        }
                        p += 1.0;
                    }
                    out
                })
                .collect()
        };
        let (cols, rows) = (spans(sx, kx, width, self.width), spans(sy, ky, height, self.height));
        for (oy, row) in rows.iter().enumerate() {
            for (ox, col) in cols.iter().enumerate() {
                let mut acc = [0.0f64; 4];
                let mut total = 0.0;
                for &(py, wy) in row {
                    for &(px, wx) in col {
                        let w = wx * wy;
                        let i = (py * self.width as usize + px) * 4;
                        let a = self.data[i + 3] as f64 / 255.0;
                        acc[0] += self.data[i] as f64 * a * w;
                        acc[1] += self.data[i + 1] as f64 * a * w;
                        acc[2] += self.data[i + 2] as f64 * a * w;
                        acc[3] += a * w;
                        total += w;
                    }
                }
                let o = (oy * width as usize + ox) * 4;
                if total > 0.0 && acc[3] > 0.0 {
                    for c in 0..3 {
                        data[o + c] = (acc[c] / acc[3]).round().clamp(0.0, 255.0) as u8;
                    }
                    data[o + 3] = (acc[3] / total * 255.0).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        Rgba { width, height, data }
    }

    /// Fade near-black pixels out (alpha ramps from 0 at brightness `low` to
    /// the pixel's own alpha at `high`), lifting art off its black backing.
    pub fn key_out_black(&mut self, low: f32, high: f32) {
        for p in self.data.chunks_exact_mut(4) {
            let level = p[0].max(p[1]).max(p[2]) as f32;
            let keep = smoothstep(low, high, level);
            p[3] = (p[3] as f32 * keep).round() as u8;
        }
    }
}

pub struct Surface<'a> {
    pub width: i32,
    pub height: i32,
    pub pixels: &'a mut [u32],
}

pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

fn channels(c: u32) -> [f32; 3] {
    [
        ((c >> 16) & 255) as f32,
        ((c >> 8) & 255) as f32,
        (c & 255) as f32,
    ]
}

fn pack(c: [f32; 3]) -> u32 {
    let q = |v: f32| v.clamp(0.0, 255.0).round() as u32;
    q(c[0]) << 16 | q(c[1]) << 8 | q(c[2])
}

pub fn mix(a: u32, b: u32, t: f32) -> u32 {
    let (a, b) = (channels(a), channels(b));
    let t = t.clamp(0.0, 1.0);
    pack([
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ])
}

pub fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A small deterministic hash in [-0.5, 0.5) used as dither against banding.
fn dither(x: i32, y: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    (h & 0xff) as f32 / 256.0 - 0.5
}

/// Signed distance to a rounded box centred at (cx, cy) with half extents.
pub fn rounded_box(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (x - cx).abs() - hw + r;
    let qy = (y - cy).abs() - hh + r;
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

/// Signed distance to a segment with thickness `w` (a capsule).
pub fn segment(x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32, w: f32) -> f32 {
    let (px, py, dx, dy) = (x - ax, y - ay, bx - ax, by - ay);
    let t = ((px * dx + py * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let (ex, ey) = (px - dx * t, py - dy * t);
    (ex * ex + ey * ey).sqrt() - w * 0.5
}

/// A person (head and shoulders) of size `r` centred at (cx, cy).
pub fn person(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> f32 {
    let (hx, hy) = (x - cx, y - (cy - r * 0.38));
    let head = (hx * hx + hy * hy).sqrt() - r * 0.40;
    let body = rounded_box(x, y, cx, cy + r * 0.78, r * 0.80, r * 0.52, r * 0.50).max(y - (cy + r * 0.92));
    // A small gap between head and shoulders.
    let gap = r * 0.12 - head;
    head.min(body.max(gap))
}

/// A gear of outer radius `r` centred at (cx, cy) with eight teeth.
pub fn gear(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> f32 {
    let (px, py) = (x - cx, y - cy);
    let body = (px * px + py * py).sqrt() - r * 0.72;
    let step = std::f32::consts::TAU / 8.0;
    let angle = py.atan2(px);
    let k = (angle / step).round() * step;
    let (s, c) = k.sin_cos();
    let (lx, ly) = (px * c + py * s, -px * s + py * c);
    let tooth = rounded_box(lx, ly, r * 0.80, 0.0, r * 0.20, r * 0.17, r * 0.05);
    let hole = r * 0.30 - (px * px + py * py).sqrt();
    body.min(tooth).max(hole)
}

impl Surface<'_> {
    #[inline]
    fn blend_at(&mut self, x: i32, y: i32, color: [f32; 3], alpha: f32) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height || alpha <= 0.0 {
            return;
        }
        let index = (y * self.width + x) as usize;
        let d = channels(self.pixels[index]);
        let a = alpha.min(1.0);
        self.pixels[index] = pack([
            d[0] + (color[0] - d[0]) * a,
            d[1] + (color[1] - d[1]) * a,
            d[2] + (color[2] - d[2]) * a,
        ]);
    }

    fn clip(&self, x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
        (
            x.max(0),
            y.max(0),
            (x + w).min(self.width),
            (y + h).min(self.height),
        )
    }

    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let (x0, y0, x1, y1) = self.clip(x, y, w, h);
        for yy in y0..y1 {
            let row = (yy * self.width) as usize;
            self.pixels[row + x0 as usize..row + x1.max(x0) as usize].fill(color);
        }
    }

    pub fn blend_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32, alpha: f32) {
        let (x0, y0, x1, y1) = self.clip(x, y, w, h);
        let c = channels(color);
        for yy in y0..y1 {
            for xx in x0..x1 {
                self.blend_at(xx, yy, c, alpha);
            }
        }
    }

    /// Per-pixel colour and alpha from `f(x, y)` over a rectangle, dithered.
    pub fn shade(&mut self, x: i32, y: i32, w: i32, h: i32, f: impl Fn(f32, f32) -> (u32, f32)) {
        let (x0, y0, x1, y1) = self.clip(x, y, w, h);
        for yy in y0..y1 {
            for xx in x0..x1 {
                let (color, alpha) = f(xx as f32 + 0.5, yy as f32 + 0.5);
                if alpha <= 0.0 {
                    continue;
                }
                let n = dither(xx, yy);
                let mut c = channels(color);
                for v in &mut c {
                    *v += n;
                }
                self.blend_at(xx, yy, c, alpha);
            }
        }
    }

    /// Anti-aliased fill of the region where `sdf(x, y) < 0` within a box.
    pub fn sdf(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        color: u32,
        alpha: f32,
        sdf: impl Fn(f32, f32) -> f32,
    ) {
        let (x0, y0, x1, y1) = self.clip(x, y, w, h);
        let c = channels(color);
        for yy in y0..y1 {
            for xx in x0..x1 {
                let d = sdf(xx as f32 + 0.5, yy as f32 + 0.5);
                let coverage = (0.5 - d).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend_at(xx, yy, c, coverage * alpha);
                }
            }
        }
    }

    /// Fill a rounded rectangle.
    #[allow(clippy::too_many_arguments)]
    pub fn rounded(&mut self, x: i32, y: i32, w: i32, h: i32, r: f32, color: u32, alpha: f32) {
        let (cx, cy) = (x as f32 + w as f32 / 2.0, y as f32 + h as f32 / 2.0);
        let (hw, hh) = (w as f32 / 2.0, h as f32 / 2.0);
        self.sdf(x - 1, y - 1, w + 2, h + 2, color, alpha, |px, py| {
            rounded_box(px, py, cx, cy, hw, hh, r)
        });
    }

    /// A rounded rectangle outline of width `t`.
    #[allow(clippy::too_many_arguments)]
    pub fn rounded_outline(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        r: f32,
        t: f32,
        color: u32,
        alpha: f32,
    ) {
        let (cx, cy) = (x as f32 + w as f32 / 2.0, y as f32 + h as f32 / 2.0);
        let (hw, hh) = (w as f32 / 2.0, h as f32 / 2.0);
        self.sdf(x - 1, y - 1, w + 2, h + 2, color, alpha, |px, py| {
            let d = rounded_box(px, py, cx, cy, hw, hh, r);
            d.max(-(d + t))
        });
    }

    /// Draw the `src` part (x, y, w, h) of `image` scaled (bilinear) into
    /// `dst`, fading to transparent over `feather` pixels at every edge.
    pub fn image_fit_feathered(
        &mut self,
        image: &Rgba,
        src: (i32, i32, i32, i32),
        dst: (i32, i32, i32, i32),
        feather: f32,
    ) {
        let (sx, sy, sw, sh) = src;
        let (x, y, w, h) = dst;
        if image.width == 0 || image.height == 0 || w <= 0 || h <= 0 || sw <= 0 || sh <= 0 {
            return;
        }
        let fetch = |px: i32, py: i32| -> [f32; 4] {
            let px = px.clamp(sx.max(0), (sx + sw).min(image.width as i32) - 1) as usize;
            let py = py.clamp(sy.max(0), (sy + sh).min(image.height as i32) - 1) as usize;
            let i = (py * image.width as usize + px) * 4;
            let a = image.data[i + 3] as f32 / 255.0;
            [
                image.data[i] as f32 * a,
                image.data[i + 1] as f32 * a,
                image.data[i + 2] as f32 * a,
                a,
            ]
        };
        let (kx, ky) = (sw as f32 / w as f32, sh as f32 / h as f32);
        let (x0, y0, x1, y1) = self.clip(x, y, w, h);
        let feather = feather.max(1.0);
        for yy in y0..y1 {
            let ly = (yy - y) as f32 + 0.5;
            let fy = sy as f32 + ly * ky - 0.5;
            let (iy, ty) = (fy.floor() as i32, fy - fy.floor());
            let edge_y = (ly.min(h as f32 - ly) / feather).min(1.0);
            for xx in x0..x1 {
                let lx = (xx - x) as f32 + 0.5;
                let fx = sx as f32 + lx * kx - 0.5;
                let (ix, tx) = (fx.floor() as i32, fx - fx.floor());
                let (a, b, c, d) = (
                    fetch(ix, iy),
                    fetch(ix + 1, iy),
                    fetch(ix, iy + 1),
                    fetch(ix + 1, iy + 1),
                );
                let mut out = [0.0f32; 4];
                for k in 0..4 {
                    let top = a[k] + (b[k] - a[k]) * tx;
                    let bottom = c[k] + (d[k] - c[k]) * tx;
                    out[k] = top + (bottom - top) * ty;
                }
                let edge = edge_y.min((lx.min(w as f32 - lx) / feather).min(1.0));
                let alpha = out[3] * smoothstep(0.0, 1.0, edge);
                if out[3] > 0.0 && alpha > 0.0 {
                    self.blend_at(
                        xx,
                        yy,
                        [out[0] / out[3], out[1] / out[3], out[2] / out[3]],
                        alpha,
                    );
                }
            }
        }
    }

    /// Composite a straight-alpha image at (x, y) with an extra opacity.
    pub fn rgba_over(&mut self, image: &Rgba, x: i32, y: i32, opacity: f32) {
        for j in 0..image.height as i32 {
            for i in 0..image.width as i32 {
                let k = ((j * image.width as i32 + i) * 4) as usize;
                let a = image.data[k + 3] as f32 / 255.0 * opacity;
                if a > 0.0 {
                    self.blend_at(
                        x + i,
                        y + j,
                        [
                            image.data[k] as f32,
                            image.data[k + 1] as f32,
                            image.data[k + 2] as f32,
                        ],
                        a,
                    );
                }
            }
        }
    }

    /// Copy a rectangle of another surface's pixels (same layout) here at (0, 0).
    pub fn copy_from(&mut self, source: &[u32], source_width: i32, sx: i32, sy: i32) {
        let source_height = source.len() as i32 / source_width.max(1);
        for y in 0..self.height {
            let yy = sy + y;
            if yy < 0 || yy >= source_height {
                continue;
            }
            for x in 0..self.width {
                let xx = sx + x;
                if xx < 0 || xx >= source_width {
                    continue;
                }
                self.pixels[(y * self.width + x) as usize] =
                    source[(yy * source_width + xx) as usize];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blending_and_fills_stay_inside_the_surface() {
        let mut pixels = vec![0u32; 16 * 8];
        let mut s = Surface {
            width: 16,
            height: 8,
            pixels: &mut pixels,
        };
        s.fill(-4, -4, 100, 100, rgb(10, 20, 30));
        s.blend_rect(2, 2, 4, 4, rgb(255, 255, 255), 0.5);
        s.rounded(8, 0, 30, 30, 4.0, rgb(200, 0, 0), 1.0);
        assert_eq!(pixels[0], rgb(10, 20, 30));
        assert_eq!(pixels[2 * 16 + 2], rgb(133, 138, 143));
        assert_eq!(pixels[4 * 16 + 12], rgb(200, 0, 0));
    }

    #[test]
    fn feathered_images_fade_at_the_edges_only() {
        let image = Rgba {
            width: 4,
            height: 4,
            data: vec![255; 64],
        };
        let mut pixels = vec![0u32; 40 * 40];
        let mut s = Surface {
            width: 40,
            height: 40,
            pixels: &mut pixels,
        };
        s.image_fit_feathered(&image, (0, 0, 4, 4), (0, 0, 40, 40), 8.0);
        assert_eq!(pixels[20 * 40 + 20], rgb(255, 255, 255));
        assert!(pixels[0] < rgb(40, 40, 40));
    }

    #[test]
    fn area_scaling_averages_whole_footprints() {
        // A 1-pixel checkerboard averages to flat grey, with no aliasing.
        let mut data = Vec::new();
        for y in 0..40u32 {
            for x in 0..40u32 {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                data.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let image = Rgba { width: 40, height: 40, data };
        let small = image.area_scaled((0, 0, 40, 40), 7, 7);
        for p in small.data.chunks_exact(4) {
            assert!((p[0] as i32 - 128).abs() <= 12, "{p:?}");
            assert_eq!(p[3], 255);
        }
        // Transparent pixels thin alpha but do not darken colour.
        let half = Rgba {
            width: 2,
            height: 1,
            data: vec![200, 100, 50, 255, 0, 0, 0, 0],
        };
        let one = half.area_scaled((0, 0, 2, 1), 1, 1);
        assert_eq!(one.data, vec![200, 100, 50, 128]);
    }

    #[test]
    fn keying_removes_black_and_keeps_bright() {
        let mut image = Rgba {
            width: 2,
            height: 1,
            data: vec![10, 12, 8, 255, 220, 40, 60, 255],
        };
        image.key_out_black(24.0, 64.0);
        assert_eq!(image.data[3], 0);
        assert_eq!(image.data[7], 255);
    }

    #[test]
    fn shapes_have_inside_and_outside() {
        assert!(gear(0.0, 10.0, 0.0, 0.0, 10.0) < 0.5);
        assert!(gear(0.0, 0.0, 0.0, 0.0, 10.0) > 0.0);
        assert!(segment(5.0, 0.0, 0.0, 0.0, 10.0, 0.0, 2.0) < 0.0);
        assert!(rounded_box(0.0, 0.0, 0.0, 0.0, 5.0, 5.0, 1.0) < 0.0);
    }
}
