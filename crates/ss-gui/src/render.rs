//! High-quality framebuffer presentation.
//!
//! The decoder gives us a BGRX framebuffer at the server's backing resolution
//! (2560x1600 on the lab server). The window is often smaller, so `draw()`
//! needs a resampler. The old path sampled a single source pixel per output
//! pixel, which at exactly 2:1 threw away three of every four pixels. This module
//! keeps that mapping available for A/B tests (`Filter::Nearest`) but defaults
//! to area averaging for exact integer downscales and bilinear otherwise.
//!
//! Everything that depends on the geometry is computed in `Scaler::new`, which
//! is only called when the window or framebuffer size changes. `Scaler::apply`
//! allocates nothing.

/// User-facing filter choice. `Auto` is the default sharp path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Auto,
    Nearest,
    Box,
    Bilinear,
}

impl Filter {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "nearest" => Some(Self::Nearest),
            "box" => Some(Self::Box),
            "bilinear" => Some(Self::Bilinear),
            _ => None,
        }
    }

    /// Resolve `Auto` (and an inapplicable `Box`) for this pair of sizes.
    fn resolve(self, src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> Mode {
        if src_w == dst_w && src_h == dst_h {
            return Mode::Identity;
        }
        let integer_downscale = src_w >= dst_w
            && src_h >= dst_h
            && src_w % dst_w == 0
            && src_h % dst_h == 0;
        match self {
            Self::Auto => {
                if integer_downscale {
                    Mode::Box
                } else {
                    Mode::Bilinear
                }
            }
            Self::Box => {
                if integer_downscale {
                    Mode::Box
                } else {
                    // Area averaging needs exact blocks. Falling back to
                    // bilinear is better than silently changing the geometry.
                    Mode::Bilinear
                }
            }
            Self::Nearest => Mode::Nearest,
            Self::Bilinear => Mode::Bilinear,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Identity,
    Nearest,
    Box,
    Bilinear,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Nearest => "nearest",
            Self::Box => "box",
            Self::Bilinear => "bilinear",
        }
    }
}

enum Plan {
    Identity,
    Nearest {
        xmap: Vec<usize>,
        ymap: Vec<usize>,
    },
    Box {
        sx: usize,
        sy: usize,
    },
    Bilinear {
        x0: Vec<usize>,
        x1: Vec<usize>,
        wx: Vec<f32>,
        y0: Vec<usize>,
        y1: Vec<usize>,
        wy: Vec<f32>,
    },
}

pub struct Scaler {
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    filter: Filter,
    mode: Mode,
    plan: Plan,
}

impl Scaler {
    pub fn new(
        src_w: usize,
        src_h: usize,
        dst_w: usize,
        dst_h: usize,
        filter: Filter,
    ) -> Self {
        assert!(src_w > 0 && src_h > 0 && dst_w > 0 && dst_h > 0);
        let mode = filter.resolve(src_w, src_h, dst_w, dst_h);
        let plan = match mode {
            Mode::Identity => Plan::Identity,
            Mode::Nearest => Plan::Nearest {
                xmap: nearest_map(dst_w, src_w),
                ymap: nearest_map(dst_h, src_h),
            },
            Mode::Box => Plan::Box {
                sx: src_w / dst_w,
                sy: src_h / dst_h,
            },
            Mode::Bilinear => {
                let (x0, x1, wx) = weights(dst_w, src_w);
                let (y0, y1, wy) = weights(dst_h, src_h);
                Plan::Bilinear { x0, x1, wx, y0, y1, wy }
            }
        };
        Self { src_w, src_h, dst_w, dst_h, filter, mode, plan }
    }

    pub fn matches(
        &self,
        src_w: usize,
        src_h: usize,
        dst_w: usize,
        dst_h: usize,
        filter: Filter,
    ) -> bool {
        self.src_w == src_w
            && self.src_h == src_h
            && self.dst_w == dst_w
            && self.dst_h == dst_h
            && self.filter == filter
    }

    pub fn mode_name(&self) -> &'static str {
        self.mode.name()
    }

    /// Resample `src` (BGRX, `src_w * src_h`) into `dst` (`dst_w * dst_h`).
    /// No allocation: all maps and weights were built by `new`.
    pub fn apply(&self, src: &[u32], dst: &mut [u32]) {
        debug_assert_eq!(src.len(), self.src_w * self.src_h);
        debug_assert_eq!(dst.len(), self.dst_w * self.dst_h);
        match &self.plan {
            Plan::Identity => dst.copy_from_slice(src),
            Plan::Nearest { xmap, ymap } => {
                for (y, &sy) in ymap.iter().enumerate() {
                    let row = &src[sy * self.src_w..(sy + 1) * self.src_w];
                    let out = &mut dst[y * self.dst_w..(y + 1) * self.dst_w];
                    for (x, px) in out.iter_mut().enumerate() {
                        *px = row[xmap[x]];
                    }
                }
            }
            Plan::Box { sx, sy } => {
                if *sx == 2 && *sy == 2 {
                    // The common 2560x1600 -> 1280x800 case. Unroll the 2x2
                    // block and average with shifts: integer division here was
                    // the difference between ~2 ms and ~20 ms per present.
                    for y in 0..self.dst_h {
                        let r0 = &src[(2 * y) * self.src_w..(2 * y + 1) * self.src_w];
                        let r1 = &src[(2 * y + 1) * self.src_w..(2 * y + 2) * self.src_w];
                        let out = &mut dst[y * self.dst_w..(y + 1) * self.dst_w];
                        for ((c0, c1), px) in r0
                            .chunks_exact(2)
                            .zip(r1.chunks_exact(2))
                            .zip(out.iter_mut())
                        {
                            let p00 = c0[0];
                            let p01 = c0[1];
                            let p10 = c1[0];
                            let p11 = c1[1];
                            let b = ((p00 & 0xFF)
                                + (p01 & 0xFF)
                                + (p10 & 0xFF)
                                + (p11 & 0xFF)
                                + 2)
                                >> 2;
                            let g = (((p00 >> 8) & 0xFF)
                                + ((p01 >> 8) & 0xFF)
                                + ((p10 >> 8) & 0xFF)
                                + ((p11 >> 8) & 0xFF)
                                + 2)
                                >> 2;
                            let r = (((p00 >> 16) & 0xFF)
                                + ((p01 >> 16) & 0xFF)
                                + ((p10 >> 16) & 0xFF)
                                + ((p11 >> 16) & 0xFF)
                                + 2)
                                >> 2;
                            *px = b | (g << 8) | (r << 16);
                        }
                    }
                } else {
                    let n = (sx * sy) as u64;
                    let half = n / 2;
                    for y in 0..self.dst_h {
                        let sy0 = y * sy;
                        for x in 0..self.dst_w {
                            let sx0 = x * sx;
                            let mut b = 0u64;
                            let mut g = 0u64;
                            let mut r = 0u64;
                            for yy in 0..*sy {
                                let row = &src[(sy0 + yy) * self.src_w + sx0
                                    ..(sy0 + yy) * self.src_w + sx0 + sx];
                                for &px in row {
                                    b += (px & 0xFF) as u64;
                                    g += ((px >> 8) & 0xFF) as u64;
                                    r += ((px >> 16) & 0xFF) as u64;
                                }
                            }
                            dst[y * self.dst_w + x] = ((b + half) / n) as u32
                                | (((g + half) / n) as u32) << 8
                                | (((r + half) / n) as u32) << 16;
                        }
                    }
                }
            }
            Plan::Bilinear { x0, x1, wx, y0, y1, wy } => {
                #[cfg(target_arch = "x86_64")]
                {
                    // SAFETY: SSE2 is part of the x86_64 baseline.
                    unsafe {
                        bilinear_sse2(
                            src, dst, self.src_w, self.dst_w, self.dst_h, x0, x1, wx, y0, y1, wy,
                        );
                    }
                }
                #[cfg(not(target_arch = "x86_64"))]
                bilinear_scalar(src, dst, self.src_w, self.dst_w, self.dst_h, x0, x1, wx, y0, y1, wy);
            }
        }
    }
}

/// Scalar bilinear fallback and the tail of the SSE2 path. f32 weights keep
/// the arithmetic simple; the 2:1 common case uses the integer box path.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
fn bilinear_scalar(
    src: &[u32],
    dst: &mut [u32],
    src_w: usize,
    dst_w: usize,
    dst_h: usize,
    x0: &[usize],
    x1: &[usize],
    wx: &[f32],
    y0: &[usize],
    y1: &[usize],
    wy: &[f32],
) {
    for y in 0..dst_h {
        let row0 = &src[y0[y] * src_w..(y0[y] + 1) * src_w];
        let row1 = &src[y1[y] * src_w..(y1[y] + 1) * src_w];
        let wy = wy[y];
        let out = &mut dst[y * dst_w..(y + 1) * dst_w];
        for (x, px) in out.iter_mut().enumerate() {
            let wx = wx[x];
            let p00 = row0[x0[x]];
            let p01 = row0[x1[x]];
            let p10 = row1[x0[x]];
            let p11 = row1[x1[x]];
            let mut value = 0u32;
            for shift in [0u32, 8, 16] {
                let a = ((p00 >> shift) & 0xFF) as f32;
                let b = ((p01 >> shift) & 0xFF) as f32;
                let c = ((p10 >> shift) & 0xFF) as f32;
                let d = ((p11 >> shift) & 0xFF) as f32;
                let top = a + (b - a) * wx;
                let bot = c + (d - c) * wx;
                let v = (top + (bot - top) * wy + 0.5) as u32;
                value |= v << shift;
            }
            *px = value;
        }
    }
}

/// SSE2 bilinear, four output pixels at a time. The gather is scalar (four
/// adjacent samples per output pixel), but the interpolation is not: on the
/// 1600x1000 window this is the difference between a ~20 ms and a ~6 ms draw.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[allow(clippy::too_many_arguments)]
unsafe fn bilinear_sse2(
    src: &[u32],
    dst: &mut [u32],
    src_w: usize,
    dst_w: usize,
    dst_h: usize,
    x0: &[usize],
    x1: &[usize],
    wx: &[f32],
    y0: &[usize],
    y1: &[usize],
    wy: &[f32],
) {
    use std::arch::x86_64::*;

    let mask = _mm_set1_epi32(0xFF);
    let half = _mm_set1_ps(0.5);
    for y in 0..dst_h {
        let row0 = &src[y0[y] * src_w..(y0[y] + 1) * src_w];
        let row1 = &src[y1[y] * src_w..(y1[y] + 1) * src_w];
        let wy = wy[y];
        let wyv = _mm_set1_ps(wy);
        let out = &mut dst[y * dst_w..(y + 1) * dst_w];
        let mut x = 0usize;
        while x + 4 <= dst_w {
            let p00 = _mm_set_epi32(
                row0[x0[x + 3]] as i32,
                row0[x0[x + 2]] as i32,
                row0[x0[x + 1]] as i32,
                row0[x0[x]] as i32,
            );
            let p01 = _mm_set_epi32(
                row0[x1[x + 3]] as i32,
                row0[x1[x + 2]] as i32,
                row0[x1[x + 1]] as i32,
                row0[x1[x]] as i32,
            );
            let p10 = _mm_set_epi32(
                row1[x0[x + 3]] as i32,
                row1[x0[x + 2]] as i32,
                row1[x0[x + 1]] as i32,
                row1[x0[x]] as i32,
            );
            let p11 = _mm_set_epi32(
                row1[x1[x + 3]] as i32,
                row1[x1[x + 2]] as i32,
                row1[x1[x + 1]] as i32,
                row1[x1[x]] as i32,
            );
            let wxv = _mm_loadu_ps(wx.as_ptr().add(x));
            let mut value = _mm_setzero_si128();
            for shift in [0i32, 8, 16] {
                let count = _mm_cvtsi32_si128(shift);
                let a = _mm_cvtepi32_ps(_mm_and_si128(_mm_srl_epi32(p00, count), mask));
                let b = _mm_cvtepi32_ps(_mm_and_si128(_mm_srl_epi32(p01, count), mask));
                let c = _mm_cvtepi32_ps(_mm_and_si128(_mm_srl_epi32(p10, count), mask));
                let d = _mm_cvtepi32_ps(_mm_and_si128(_mm_srl_epi32(p11, count), mask));
                let top = _mm_add_ps(a, _mm_mul_ps(_mm_sub_ps(b, a), wxv));
                let bot = _mm_add_ps(c, _mm_mul_ps(_mm_sub_ps(d, c), wxv));
                let v = _mm_add_ps(top, _mm_add_ps(_mm_mul_ps(_mm_sub_ps(bot, top), wyv), half));
                let v = _mm_cvttps_epi32(v);
                value = _mm_or_si128(value, _mm_sll_epi32(v, count));
            }
            _mm_storeu_si128(out.as_mut_ptr().add(x) as *mut __m128i, value);
            x += 4;
        }
        // Tail (dst_w not a multiple of four): use the scalar formula.
        for x in x..dst_w {
            let wx = wx[x];
            let p00 = row0[x0[x]];
            let p01 = row0[x1[x]];
            let p10 = row1[x0[x]];
            let p11 = row1[x1[x]];
            let mut value = 0u32;
            for shift in [0u32, 8, 16] {
                let a = ((p00 >> shift) & 0xFF) as f32;
                let b = ((p01 >> shift) & 0xFF) as f32;
                let c = ((p10 >> shift) & 0xFF) as f32;
                let d = ((p11 >> shift) & 0xFF) as f32;
                let top = a + (b - a) * wx;
                let bot = c + (d - c) * wx;
                let v = (top + (bot - top) * wy + 0.5) as u32;
                value |= v << shift;
            }
            out[x] = value;
        }
    }
}

/// Old nearest-neighbour mapping, preserved exactly for `--filter nearest`:
/// `floor(dst_index * src_len / dst_len)`.
fn nearest_map(dst_len: usize, src_len: usize) -> Vec<usize> {
    (0..dst_len)
        .map(|d| (d as u64 * src_len as u64 / dst_len as u64) as usize)
        .collect()
}

/// Axis sample positions and 16.16 weights for bilinear. Coordinates are
/// pixel centres; the first and last output pixels clamp to the edge.
fn weights(dst_len: usize, src_len: usize) -> (Vec<usize>, Vec<usize>, Vec<f32>) {
    let scale = src_len as f64 / dst_len as f64;
    let mut i0 = Vec::with_capacity(dst_len);
    let mut i1 = Vec::with_capacity(dst_len);
    let mut w = Vec::with_capacity(dst_len);
    for d in 0..dst_len {
        let pos = ((d as f64 + 0.5) * scale - 0.5).clamp(0.0, (src_len - 1) as f64);
        let lo = pos.floor() as usize;
        let frac = pos - lo as f64;
        i0.push(lo);
        i1.push((lo + 1).min(src_len - 1));
        w.push(frac as f32);
    }
    (i0, i1, w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(r: u32, g: u32, b: u32) -> u32 {
        (r << 16) | (g << 8) | b
    }

    fn channels(px: u32) -> (u32, u32, u32) {
        (px >> 16 & 0xFF, px >> 8 & 0xFF, px & 0xFF)
    }

    #[test]
    fn identity_is_a_copy() {
        let src: Vec<u32> = (0..16).map(|i| px(i, i * 2, i * 3)).collect();
        let mut dst = vec![0; 16];
        Scaler::new(4, 4, 4, 4, Filter::Auto).apply(&src, &mut dst);
        assert_eq!(dst, src);
    }

    #[test]
    fn box_averages_2x2_instead_of_sampling() {
        // Four source pixels per destination pixel: 0, 10, 20, 30 -> 15.
        let src = vec![
            px(0, 0, 0), px(10, 10, 10), px(100, 100, 100), px(110, 110, 110),
            px(20, 20, 20), px(30, 30, 30), px(120, 120, 120), px(130, 130, 130),
        ];
        let mut dst = vec![0; 2];
        let scaler = Scaler::new(4, 2, 2, 1, Filter::Auto);
        assert_eq!(scaler.mode_name(), "box");
        scaler.apply(&src, &mut dst);
        assert_eq!(channels(dst[0]), (15, 15, 15));
        assert_eq!(channels(dst[1]), (115, 115, 115));
    }

    #[test]
    fn box_does_not_carry_between_channels() {
        // One red and one blue pixel of 255 each. A naive u32 sum would turn
        // the low byte into a carry; each channel must average independently.
        let src = vec![px(255, 0, 0), px(0, 255, 255)];
        let mut dst = vec![0; 1];
        Scaler::new(2, 1, 1, 1, Filter::Box).apply(&src, &mut dst);
        assert_eq!(channels(dst[0]), (128, 128, 128));
    }

    #[test]
    fn box_uses_rounding_not_truncation() {
        let src = vec![px(1, 1, 1), px(2, 2, 2)];
        let mut dst = vec![0; 1];
        Scaler::new(2, 1, 1, 1, Filter::Box).apply(&src, &mut dst);
        assert_eq!(channels(dst[0]), (2, 2, 2)); // (3 + 1) / 2
    }

    #[test]
    fn nearest_preserves_the_old_mapping() {
        // dst 2x2 from src 4x4: sample (0,0), (2,0), (0,2), (2,2).
        let src: Vec<u32> = (0..16).map(|i| px(i, i, i)).collect();
        let mut dst = vec![0; 4];
        let scaler = Scaler::new(4, 4, 2, 2, Filter::Nearest);
        assert_eq!(scaler.mode_name(), "nearest");
        scaler.apply(&src, &mut dst);
        assert_eq!(dst, vec![px(0, 0, 0), px(2, 2, 2), px(8, 8, 8), px(10, 10, 10)]);
    }

    #[test]
    fn auto_prefers_box_for_integer_downscale_and_bilinear_otherwise() {
        assert_eq!(Scaler::new(2560, 1600, 1280, 800, Filter::Auto).mode_name(), "box");
        assert_eq!(Scaler::new(2560, 1600, 1600, 1000, Filter::Auto).mode_name(), "bilinear");
        assert_eq!(Scaler::new(2560, 1600, 2560, 1600, Filter::Auto).mode_name(), "identity");
    }

    #[test]
    fn explicit_box_falls_back_to_bilinear_when_not_integral() {
        assert_eq!(Scaler::new(2560, 1600, 1600, 1000, Filter::Box).mode_name(), "bilinear");
    }

    #[test]
    fn bilinear_preserves_a_uniform_field() {
        let src = vec![px(17, 34, 51); 8 * 8];
        let mut dst = vec![0; 5 * 3];
        Scaler::new(8, 8, 5, 3, Filter::Bilinear).apply(&src, &mut dst);
        assert!(dst.iter().all(|&p| p == px(17, 34, 51)));
    }

    #[test]
    fn bilinear_interpolates_between_endpoints() {
        // One row, two pixels 0 and 255 -> four outputs. Pixel-centre sampling
        // gives 0, 64, 191, 255 (the ends clamp).
        let src = vec![px(0, 0, 0), px(255, 255, 255)];
        let mut dst = vec![0; 4];
        Scaler::new(2, 1, 4, 1, Filter::Bilinear).apply(&src, &mut dst);
        let v: Vec<u32> = dst.iter().map(|&p| channels(p).0).collect();
        assert_eq!(v, vec![0, 64, 191, 255]);
    }

    #[test]
    fn sse2_matches_scalar_bilinear_on_varied_data_and_tail() {
        #[cfg(target_arch = "x86_64")]
        {
            let mut seed = 0x1234_5678u32;
            let src: Vec<u32> = (0..37 * 23)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    seed
                })
                .collect();
            let scaler = Scaler::new(37, 23, 101, 61, Filter::Bilinear);
            let mut fast = vec![0; 101 * 61];
            let mut slow = vec![0; 101 * 61];
            scaler.apply(&src, &mut fast);
            if let Plan::Bilinear { x0, x1, wx, y0, y1, wy } = &scaler.plan {
                bilinear_scalar(
                    &src,
                    &mut slow,
                    scaler.src_w,
                    scaler.dst_w,
                    scaler.dst_h,
                    x0,
                    x1,
                    wx,
                    y0,
                    y1,
                    wy,
                );
            } else {
                panic!("expected bilinear plan");
            }
            assert_eq!(fast, slow);
        }
    }

    #[test]
    fn filter_names_parse() {
        assert_eq!(Filter::parse("auto"), Some(Filter::Auto));
        assert_eq!(Filter::parse("nearest"), Some(Filter::Nearest));
        assert_eq!(Filter::parse("box"), Some(Filter::Box));
        assert_eq!(Filter::parse("bilinear"), Some(Filter::Bilinear));
        assert_eq!(Filter::parse("sharpen"), None);
    }
}

