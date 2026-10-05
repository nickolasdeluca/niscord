//! CPU image downscaling. Good enough for thumbnails and previews; the video
//! pipeline will scale on the GPU.

use crate::RgbaImage;

/// Largest size with `src`'s aspect ratio that fits in `max_w` x `max_h`,
/// never upscaling. Both dimensions are at least 1.
pub fn fit(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (1, 1);
    }
    if src_w <= max_w && src_h <= max_h {
        return (src_w, src_h);
    }
    let scale = (max_w as f64 / src_w as f64).min(max_h as f64 / src_h as f64);
    (((src_w as f64 * scale).round() as u32).max(1), ((src_h as f64 * scale).round() as u32).max(1))
}

/// Downscale tightly packed RGBA8 `src` (with `row_bytes` stride) to fit in
/// `max_w` x `max_h` using a box filter (each output pixel averages the source
/// pixels it covers, which avoids the shimmering of nearest-neighbour).
pub fn downscale_rgba(src: &[u8], width: u32, height: u32, row_bytes: usize, max_w: u32, max_h: u32) -> RgbaImage {
    let (dw, dh) = fit(width, height, max_w, max_h);
    if (dw, dh) == (width, height) {
        // No scaling needed: just strip the row padding.
        let row = width as usize * 4;
        let pixels = src.chunks(row_bytes).take(height as usize).flat_map(|r| &r[..row]).copied().collect();
        return RgbaImage { width, height, pixels };
    }
    let mut pixels = vec![0u8; dw as usize * dh as usize * 4];

    // Source column range for each output column, computed once.
    let cols: Vec<(usize, usize)> = (0..dw)
        .map(|x| {
            let x0 = (x as u64 * width as u64 / dw as u64) as usize;
            let x1 = (((x as u64 + 1) * width as u64 / dw as u64) as usize).max(x0 + 1);
            (x0, x1)
        })
        .collect();

    for y in 0..dh {
        let y0 = (y as u64 * height as u64 / dh as u64) as usize;
        let y1 = (((y as u64 + 1) * height as u64 / dh as u64) as usize).max(y0 + 1);
        let out_row = &mut pixels[y as usize * dw as usize * 4..][..dw as usize * 4];
        for (x, &(x0, x1)) in cols.iter().enumerate() {
            let mut sum = [0u32; 4];
            for sy in y0..y1 {
                let row = &src[sy * row_bytes..];
                for px in row[x0 * 4..x1 * 4].as_chunks::<4>().0 {
                    sum[0] += px[0] as u32;
                    sum[1] += px[1] as u32;
                    sum[2] += px[2] as u32;
                    sum[3] += px[3] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let out = &mut out_row[x * 4..x * 4 + 4];
            for c in 0..4 {
                out[c] = ((sum[c] + n / 2) / n) as u8;
            }
        }
    }
    RgbaImage { width: dw, height: dh, pixels }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_and_never_upscales() {
        assert_eq!(fit(3840, 2160, 320, 180), (320, 180));
        assert_eq!(fit(1000, 2000, 320, 180), (90, 180));
        assert_eq!(fit(100, 50, 320, 180), (100, 50));
        assert_eq!(fit(10000, 1, 320, 180), (320, 1));
    }

    #[test]
    fn box_filter_averages() {
        // 2x2 image: black, white / white, black -> one mid-grey pixel.
        #[rustfmt::skip]
        let src = [
            0, 0, 0, 255,   255, 255, 255, 255,
            255, 255, 255, 255,   0, 0, 0, 255,
        ];
        let out = downscale_rgba(&src, 2, 2, 8, 1, 1);
        assert_eq!((out.width, out.height), (1, 1));
        assert_eq!(out.pixels, vec![128, 128, 128, 255]);
    }

    #[test]
    fn respects_row_padding() {
        // 1x2 image with 4 bytes of padding per row.
        let src = [10, 20, 30, 255, 9, 9, 9, 9, 30, 40, 50, 255, 9, 9, 9, 9];
        let out = downscale_rgba(&src, 1, 2, 8, 1, 2);
        assert_eq!(out.pixels, vec![10, 20, 30, 255, 30, 40, 50, 255]);
    }
}
