//! Decoded pictures (planar 4:2:0, or packed RGB from a JPEG) → display RGBA, reduced and rotated
//! on the way.

/// The samples of one plane.
#[derive(Clone, Copy)]
pub(crate) enum Samples<'a> {
    U8(&'a [u8]),
    /// More than 8 bits per sample (HEVC Main 10).
    #[cfg_attr(not(feature = "hevc"), allow(dead_code))]
    U16(&'a [u16]),
}

impl Samples<'_> {
    /// Sample `i` as 8 bits (`shift`: the bits above 8); 0 outside the plane.
    fn at(&self, i: usize, shift: u32) -> i32 {
        match self {
            Samples::U8(s) => s.get(i).copied().map_or(0, i32::from),
            Samples::U16(s) => s.get(i).copied().map_or(0, |v| i32::from(v >> shift.min(8))),
        }
    }
}

/// A decoded picture as its decoder describes it.
pub(crate) struct Yuv<'a> {
    pub width: u32,
    pub height: u32,
    pub chroma_width: u32,
    pub chroma_height: u32,
    pub y: Samples<'a>,
    pub u: Samples<'a>,
    pub v: Samples<'a>,
    pub y_stride: usize,
    pub uv_stride: usize,
    pub bit_depth: u32,
    /// ITU-T H.273 matrix coefficients (1: BT.709, 5 / 6: BT.601, 9: BT.2020, 2: unspecified).
    pub matrix: u8,
    pub full_range: bool,
}

/// Fixed-point (14 fractional bits) conversion for one matrix and range.
struct Coefficients {
    y_offset: i32,
    y_scale: i32,
    r_v: i32,
    g_u: i32,
    g_v: i32,
    b_u: i32,
}

const ONE: f64 = 16_384.0;

fn coefficients(matrix: u8, full_range: bool, height: u32) -> Coefficients {
    let (kr, kb) = match matrix {
        1 => (0.2126, 0.0722),
        5 | 6 => (0.299, 0.114),
        9 | 10 => (0.2627, 0.0593),
        // unspecified: what players assume, HD is BT.709 and SD is BT.601
        _ if height >= 720 => (0.2126, 0.0722),
        _ => (0.299, 0.114),
    };
    let kg = 1.0 - kr - kb;
    let (y_range, c_range) = if full_range { (255.0, 255.0) } else { (219.0, 224.0) };
    let c = 255.0 / c_range * ONE;
    Coefficients {
        y_offset: if full_range { 0 } else { 16 },
        y_scale: (255.0 / y_range * ONE) as i32,
        r_v: (2.0 * (1.0 - kr) * c) as i32,
        g_u: (2.0 * kb * (1.0 - kb) / kg * c) as i32,
        g_v: (2.0 * kr * (1.0 - kr) / kg * c) as i32,
        b_u: (2.0 * (1.0 - kb) * c) as i32,
    }
}

/// Pictures smaller than this (pixels) are converted on the calling thread.
const PARALLEL_FROM: usize = 1 << 18;
/// The most threads one picture is converted on.
const MAX_WORKERS: usize = 8;

fn workers(pixels: usize) -> usize {
    if cfg!(target_arch = "wasm32") || pixels < PARALLEL_FROM {
        return 1;
    }
    std::thread::available_parallelism().map_or(1, usize::from).clamp(1, MAX_WORKERS)
}

/// A `w` x `h` picture as RGBA (opaque), turned `rotation` quarter turns clockwise;
/// `pixel(col, row)` is its colour before the turn. Large pictures are filled on several threads
/// (a 4K frame a thread at a time costs more than decoding it). → (width, height, pixels).
fn paint(w: usize, h: usize, rotation: u8, pixel: impl Fn(usize, usize) -> [u8; 3] + Sync) -> (u32, u32, Vec<u8>) {
    let (w, h) = (w.max(1), h.max(1));
    let (out_w, out_h) = if rotation % 2 == 1 { (h, w) } else { (w, h) };
    let mut out = vec![255u8; out_w.saturating_mul(out_h).saturating_mul(4)];
    let line = out_w * 4;
    // the rows from `first` on: each output pixel looks up where it comes from
    let fill = |first: usize, rows: &mut [u8]| {
        for (dy, row) in rows.chunks_exact_mut(line).enumerate() {
            let y = first + dy;
            for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (col, row) = match rotation % 4 {
                    0 => (x, y),
                    1 => (y, h - 1 - x),
                    2 => (w - 1 - x, h - 1 - y),
                    _ => (w - 1 - y, x),
                };
                let [r, g, b] = pixel(col, row);
                *px = [r, g, b, 255];
            }
        }
    };
    let workers = workers(out_w * out_h).min(out_h);
    if workers <= 1 {
        fill(0, &mut out);
    } else {
        let band = out_h.div_ceil(workers);
        std::thread::scope(|scope| {
            for (i, rows) in out.chunks_mut(band * line).enumerate() {
                let fill = &fill;
                scope.spawn(move || fill(i * band, rows));
            }
        });
    }
    (out_w as u32, out_h as u32, out)
}

/// For each of `count` output samples, `divisor` apart: its index in a plane `size` long, and in
/// the chroma plane (`chroma` long) that goes with it.
fn taps(count: usize, divisor: usize, size: usize, chroma: usize) -> Vec<(usize, usize)> {
    (0..count)
        .map(|i| {
            let at = (i * divisor).min(size.saturating_sub(1));
            (at, (at * chroma).checked_div(size).unwrap_or(0).min(chroma.saturating_sub(1)))
        })
        .collect()
}

/// `picture` as RGBA (opaque), every `divisor`-th sample in both directions, turned `rotation`
/// quarter turns clockwise. → (width, height, pixels).
pub(crate) fn to_rgba(picture: &Yuv, rotation: u8, divisor: u32) -> (u32, u32, Vec<u8>) {
    let d = divisor.max(1) as usize;
    let (width, height) = (picture.width as usize, picture.height as usize);
    let (w, h) = ((width / d).max(1), (height / d).max(1));
    let k = coefficients(picture.matrix, picture.full_range, picture.height);
    let shift = picture.bit_depth.saturating_sub(8);
    let columns = taps(w, d, width, picture.chroma_width as usize);
    let rows = taps(h, d, height, picture.chroma_height as usize);
    paint(w, h, rotation, |col, row| {
        let ((sx, cx), (sy, cy)) = (columns.get(col).copied().unwrap_or_default(), rows.get(row).copied().unwrap_or_default());
        let luma = (picture.y.at(sy * picture.y_stride + sx, shift) - k.y_offset) * k.y_scale;
        let u = picture.u.at(cy * picture.uv_stride + cx, shift) - 128;
        let v = picture.v.at(cy * picture.uv_stride + cx, shift) - 128;
        let r = (luma + k.r_v * v + 8192) >> 14;
        let g = (luma - k.g_u * u - k.g_v * v + 8192) >> 14;
        let b = (luma + k.b_u * u + 8192) >> 14;
        [r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8]
    })
}

/// [`to_rgba`] for a picture that is already display values, `channels` bytes a pixel: grey (1),
/// RGB (3) or RGB and a fourth byte that is ignored (4).
pub(crate) fn packed_to_rgba(pixels: &[u8], channels: usize, width: u32, height: u32, rotation: u8, divisor: u32) -> (u32, u32, Vec<u8>) {
    let d = divisor.max(1) as usize;
    let (width, height) = (width as usize, height as usize);
    let (w, h) = ((width / d).max(1), (height / d).max(1));
    paint(w, h, rotation, |col, row| {
        let (sx, sy) = ((col * d).min(width.saturating_sub(1)), (row * d).min(height.saturating_sub(1)));
        let at = (sy * width + sx) * channels;
        // outside the picture's data: black, never out of bounds
        match (channels, pixels.get(at..at + channels)) {
            (1, Some(&[v])) => [v; 3],
            (3, Some(&[r, g, b])) | (4, Some(&[r, g, b, _])) => [r, g, b],
            _ => [0; 3],
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture<'a>(y: &'a [u8], u: &'a [u8], v: &'a [u8], matrix: u8, full_range: bool) -> Yuv<'a> {
        Yuv {
            width: 4,
            height: 2,
            chroma_width: 2,
            chroma_height: 1,
            y: Samples::U8(y),
            u: Samples::U8(u),
            v: Samples::U8(v),
            y_stride: 4,
            uv_stride: 2,
            bit_depth: 8,
            matrix,
            full_range,
        }
    }

    #[test]
    fn video_range_grey_and_primaries() {
        // black, white, and the red chroma extreme of BT.709 video range
        let (w, h, px) = to_rgba(&picture(&[16, 16, 235, 235, 16, 16, 235, 235], &[128, 128], &[128, 128], 1, false), 0, 1);
        assert_eq!((w, h), (4, 2));
        assert_eq!(&px[..4], &[0, 0, 0, 255]);
        assert_eq!(&px[8..12], &[255, 255, 255, 255]);
        let (_, _, red) = to_rgba(&picture(&[63; 8], &[102, 102], &[240, 240], 1, false), 0, 1);
        assert!(red[0] > 250 && red[1] < 5 && red[2] < 5, "{:?}", &red[..4]);
        // full range passes grey through unchanged
        let (_, _, grey) = to_rgba(&picture(&[100; 8], &[128, 128], &[128, 128], 1, true), 0, 1);
        assert_eq!(&grey[..3], &[100, 100, 100]);
    }

    #[test]
    fn quarter_turns_move_the_top_left_sample() {
        let y = [235, 16, 16, 16, 16, 16, 16, 16];
        let bright = |rotation: u8| {
            let (w, h, px) = to_rgba(&picture(&y, &[128, 128], &[128, 128], 1, false), rotation, 1);
            let at = px.as_chunks::<4>().0.iter().position(|p| p[0] > 200).unwrap();
            (w, h, at as u32 % w, at as u32 / w)
        };
        assert_eq!(bright(0), (4, 2, 0, 0));
        assert_eq!(bright(1), (2, 4, 1, 0), "90° clockwise: the top row becomes the right column");
        assert_eq!(bright(2), (4, 2, 3, 1));
        assert_eq!(bright(3), (2, 4, 0, 3));
    }

    #[test]
    fn reduced_and_high_bit_depth_and_short_planes() {
        let (w, h, _) = to_rgba(&picture(&[16; 8], &[128, 128], &[128, 128], 1, false), 0, 2);
        assert_eq!((w, h), (2, 1));
        // 10-bit samples come down to 8
        let (y, c) = ([940u16; 8], [512u16; 2]);
        let ten = Yuv { y: Samples::U16(&y), u: Samples::U16(&c), v: Samples::U16(&c), bit_depth: 10, ..picture(&[], &[], &[], 1, false) };
        assert_eq!(&to_rgba(&ten, 0, 1).2[..3], &[255, 255, 255]);
        // planes shorter than the picture claims read as black, never out of bounds
        let short = to_rgba(&picture(&[235], &[], &[], 1, false), 1, 1);
        assert_eq!(short.2.len(), 2 * 4 * 4);
    }

    /// A picture large enough to be converted on several threads comes out exactly as the same
    /// picture converted on one (each pixel alone), whichever way it is turned.
    #[test]
    fn a_large_picture_converted_on_several_threads_is_the_same_picture() {
        let (width, height) = (1024usize, 768usize);
        assert!(width * height >= PARALLEL_FROM);
        let y: Vec<u8> = (0..width * height).map(|i| ((i % width) * 7 + (i / width) * 13) as u8).collect();
        let (cw, ch) = (width / 2, height / 2);
        let u: Vec<u8> = (0..cw * ch).map(|i| (i * 5 % 251) as u8).collect();
        let v: Vec<u8> = (0..cw * ch).map(|i| (i * 11 % 241) as u8).collect();
        let big = Yuv {
            width: width as u32,
            height: height as u32,
            chroma_width: cw as u32,
            chroma_height: ch as u32,
            y: Samples::U8(&y),
            u: Samples::U8(&u),
            v: Samples::U8(&v),
            y_stride: width,
            uv_stride: cw,
            bit_depth: 8,
            matrix: 1,
            full_range: false,
        };
        let upright = to_rgba(&big, 0, 1).2;
        // one pixel at a time, from a one-pixel picture each: no threads involved
        let alone = |col: usize, row: usize| {
            let (yy, uu, vv) = ([y[row * width + col]], [u[(row / 2) * cw + col / 2]], [v[(row / 2) * cw + col / 2]]);
            let one = Yuv {
                width: 1,
                height: 1,
                chroma_width: 1,
                chroma_height: 1,
                y: Samples::U8(&yy),
                u: Samples::U8(&uu),
                v: Samples::U8(&vv),
                y_stride: 1,
                uv_stride: 1,
                ..big
            };
            // the matrix is chosen by the height when unspecified: this one names BT.709
            to_rgba(&one, 0, 1).2
        };
        for (col, row) in [(0, 0), (1023, 0), (0, 767), (1023, 767), (511, 300), (640, 95), (3, 700)] {
            assert_eq!(upright[(row * width + col) * 4..][..4], alone(col, row)[..], "({col}, {row})");
        }
        for rotation in 1..4u8 {
            let (w, h, turned) = to_rgba(&big, rotation, 1);
            assert_eq!((w, h), if rotation % 2 == 1 { (768, 1024) } else { (1024, 768) });
            for (col, row) in [(0usize, 0usize), (1023, 0), (0, 767), (1023, 767), (511, 300), (640, 95)] {
                let (x, y) = match rotation {
                    1 => (height - 1 - row, col),
                    2 => (width - 1 - col, height - 1 - row),
                    _ => (row, width - 1 - col),
                };
                assert_eq!(turned[(y * w as usize + x) * 4..][..4], upright[(row * width + col) * 4..][..4], "rotation {rotation}, ({col}, {row})");
            }
        }
        // reduced, and packed pictures, go the same way
        let (w, h, half) = to_rgba(&big, 0, 2);
        assert_eq!((w, h), (512, 384));
        assert_eq!(half[(100 * 512 + 200) * 4..][..4], upright[(200 * width + 400) * 4..][..4]);
        let rgb: Vec<u8> = (0..width * height * 3).map(|i| (i % 253) as u8).collect();
        let (_, _, packed) = packed_to_rgba(&rgb, 3, width as u32, height as u32, 2, 1);
        let from = ((height - 1 - 10) * width + (width - 1 - 20)) * 3;
        assert_eq!(packed[(10 * width + 20) * 4..][..4], [rgb[from], rgb[from + 1], rgb[from + 2], 255]);
    }

    #[test]
    fn packed_pictures_are_turned_and_reduced_the_same_way() {
        // 4 x 2, RGB: the top left pixel is red, the rest dark
        let mut rgb = vec![10u8; 4 * 2 * 3];
        rgb[..3].copy_from_slice(&[250, 0, 0]);
        let red = |rotation: u8| {
            let (w, h, px) = packed_to_rgba(&rgb, 3, 4, 2, rotation, 1);
            let at = px.as_chunks::<4>().0.iter().position(|p| p == &[250, 0, 0, 255]).unwrap();
            (w, h, at as u32 % w, at as u32 / w)
        };
        assert_eq!(red(0), (4, 2, 0, 0));
        assert_eq!(red(1), (2, 4, 1, 0));
        assert_eq!(red(2), (4, 2, 3, 1));
        assert_eq!(red(3), (2, 4, 0, 3));
        assert_eq!(packed_to_rgba(&rgb, 3, 4, 2, 0, 2).2, [250, 0, 0, 255, 10, 10, 10, 255]);
        // grey, and RGB with a fourth byte
        assert_eq!(packed_to_rgba(&[7, 9], 1, 2, 1, 0, 1).2, [7, 7, 7, 255, 9, 9, 9, 255]);
        assert_eq!(packed_to_rgba(&[1, 2, 3, 99], 4, 1, 1, 0, 1).2, [1, 2, 3, 255]);
        // data shorter than the picture claims reads as black, never out of bounds
        assert_eq!(packed_to_rgba(&[5, 5, 5], 3, 2, 2, 1, 1).2.len(), 2 * 2 * 4);
        assert_eq!(packed_to_rgba(&[], 0, 2, 2, 0, 1).2[..3], [0, 0, 0]);
    }
}
