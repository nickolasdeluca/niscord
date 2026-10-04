//! RGBA to NV12 conversion for hardware encoders.
//!
//! BT.601 limited range, the same matrix OpenH264 uses on both ends, so
//! colours match whichever encoder produced the stream.

/// Rows per parallel job are chosen so each thread gets a decent chunk;
/// small frames aren't worth the thread overhead.
const PARALLEL_MIN_PIXELS: usize = 640 * 360;

/// Bytes an NV12 frame of this size takes: full-size Y plane, then
/// interleaved U/V at half resolution.
pub fn nv12_len(width: usize, height: usize) -> usize {
    width * height * 3 / 2
}

/// Convert tightly packed RGBA (even width and height) to NV12 in `out`.
pub fn rgba_to_nv12(rgba: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    out.resize(nv12_len(width, height), 0);
    convert_into(rgba, width, height, out);
}

/// Like [`rgba_to_nv12`], into a buffer of exactly [`nv12_len`] bytes.
pub fn convert_into(rgba: &[u8], width: usize, height: usize, out: &mut [u8]) {
    assert!(width.is_multiple_of(2) && height.is_multiple_of(2), "NV12 needs even dimensions");
    assert!(rgba.len() >= width * height * 4);
    assert_eq!(out.len(), nv12_len(width, height));
    let (y_plane, uv_plane) = out.split_at_mut(width * height);

    // Work in pairs of rows: two Y rows share one UV row.
    let pairs = height / 2;
    let threads = if width * height >= PARALLEL_MIN_PIXELS {
        std::thread::available_parallelism().map_or(1, |n| n.get()).min(4)
    } else {
        1
    };
    let pairs_per_job = pairs.div_ceil(threads);
    let rgba_rows = rgba[..width * height * 4].chunks(pairs_per_job * 2 * width * 4);
    let y_rows = y_plane.chunks_mut(pairs_per_job * 2 * width);
    let uv_rows = uv_plane.chunks_mut(pairs_per_job * width);

    if threads == 1 {
        convert_rows(rgba, width, y_plane, uv_plane);
        return;
    }
    std::thread::scope(|scope| {
        for ((src, y), uv) in rgba_rows.zip(y_rows).zip(uv_rows) {
            scope.spawn(move || convert_rows(src, width, y, uv));
        }
    });
}

#[inline(always)]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

fn convert_rows(rgba: &[u8], width: usize, y: &mut [u8], uv: &mut [u8]) {
    let row = width * 4;
    for ((src, y), uv) in rgba.chunks_exact(row * 2).zip(y.chunks_exact_mut(width * 2)).zip(uv.chunks_exact_mut(width))
    {
        let (top, bottom) = src.split_at(row);
        let (y_top, y_bottom) = y.split_at_mut(width);
        for x in (0..width).step_by(2) {
            let p = |row: &[u8], x: usize| (row[x * 4] as i32, row[x * 4 + 1] as i32, row[x * 4 + 2] as i32);
            let (r0, g0, b0) = p(top, x);
            let (r1, g1, b1) = p(top, x + 1);
            let (r2, g2, b2) = p(bottom, x);
            let (r3, g3, b3) = p(bottom, x + 1);
            y_top[x] = luma(r0, g0, b0);
            y_top[x + 1] = luma(r1, g1, b1);
            y_bottom[x] = luma(r2, g2, b2);
            y_bottom[x + 1] = luma(r3, g3, b3);
            // Chroma from the 2x2 average.
            let r = (r0 + r1 + r2 + r3 + 2) >> 2;
            let g = (g0 + g1 + g2 + g3 + 2) >> 2;
            let b = (b0 + b1 + b2 + b3 + 2) >> 2;
            uv[x] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            uv[x + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primaries_land_on_bt601_limited_values() {
        // 2x2 blocks of white, black, red.
        for (rgb, yuv) in
            [([255, 255, 255], (235, 128, 128)), ([0, 0, 0], (16, 128, 128)), ([255, 0, 0], (82, 90, 240))]
        {
            let rgba: Vec<u8> = (0..4).flat_map(|_| [rgb[0], rgb[1], rgb[2], 255]).collect();
            let mut out = Vec::new();
            rgba_to_nv12(&rgba, 2, 2, &mut out);
            assert_eq!(out.len(), 6);
            assert!(out[..4].iter().all(|y| y.abs_diff(yuv.0) <= 1), "{rgb:?}: Y {:?}", &out[..4]);
            assert!(out[4].abs_diff(yuv.1) <= 1 && out[5].abs_diff(yuv.2) <= 1, "{rgb:?}: UV {:?}", &out[4..]);
        }
    }

    #[test]
    fn parallel_and_serial_agree() {
        let (w, h) = (1280, 720);
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        let mut parallel = Vec::new();
        rgba_to_nv12(&rgba, w, h, &mut parallel);
        let mut serial = vec![0; nv12_len(w, h)];
        let (y, uv) = serial.split_at_mut(w * h);
        convert_rows(&rgba, w, y, uv);
        assert!(parallel == serial);
    }
}
