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

/// The size [`to_rgba`] gives for a picture of `width` x `height`.
pub(crate) fn output_size(width: u32, height: u32, rotation: u8, divisor: u32) -> (u32, u32) {
    let d = divisor.max(1);
    let (w, h) = ((width / d).max(1), (height / d).max(1));
    if rotation % 2 == 1 { (h, w) } else { (w, h) }
}

/// `picture` as RGBA (opaque), every `divisor`-th sample in both directions, turned `rotation`
/// quarter turns clockwise. → (width, height, pixels).
pub(crate) fn to_rgba(picture: &Yuv, rotation: u8, divisor: u32) -> (u32, u32, Vec<u8>) {
    let d = divisor.max(1) as usize;
    let (width, height) = (picture.width as usize, picture.height as usize);
    let (w, h) = ((width / d).max(1), (height / d).max(1));
    let (out_w, out_h) = output_size(picture.width, picture.height, rotation, divisor);
    let (out_w, out_h) = (out_w as usize, out_h as usize);
    let mut out = vec![255u8; out_w.saturating_mul(out_h).saturating_mul(4)];
    let k = coefficients(picture.matrix, picture.full_range, picture.height);
    let shift = picture.bit_depth.saturating_sub(8);
    let (cw, ch) = (picture.chroma_width as usize, picture.chroma_height as usize);
    for row in 0..h {
        let sy = (row * d).min(height.saturating_sub(1));
        let cy = (sy * ch).checked_div(height).unwrap_or(0).min(ch.saturating_sub(1));
        for col in 0..w {
            let sx = (col * d).min(width.saturating_sub(1));
            let cx = (sx * cw).checked_div(width).unwrap_or(0).min(cw.saturating_sub(1));
            let luma = (picture.y.at(sy * picture.y_stride + sx, shift) - k.y_offset) * k.y_scale;
            let u = picture.u.at(cy * picture.uv_stride + cx, shift) - 128;
            let v = picture.v.at(cy * picture.uv_stride + cx, shift) - 128;
            let r = (luma + k.r_v * v + 8192) >> 14;
            let g = (luma - k.g_u * u - k.g_v * v + 8192) >> 14;
            let b = (luma + k.b_u * u + 8192) >> 14;
            let (x, y) = turned(col, row, w, h, rotation);
            if let Some(px) = out.get_mut((y * out_w + x) * 4..(y * out_w + x) * 4 + 3) {
                px.copy_from_slice(&[r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8]);
            }
        }
    }
    (out_w as u32, out_h as u32, out)
}

/// Where the sample at (`col`, `row`) of a `w` x `h` picture is shown after `rotation` quarter
/// turns clockwise.
fn turned(col: usize, row: usize, w: usize, h: usize, rotation: u8) -> (usize, usize) {
    match rotation % 4 {
        0 => (col, row),
        1 => (h - 1 - row, col),
        2 => (w - 1 - col, h - 1 - row),
        _ => (row, w - 1 - col),
    }
}

/// [`to_rgba`] for a picture that is already display values, `channels` bytes a pixel: grey (1),
/// RGB (3) or RGB and a fourth byte that is ignored (4).
pub(crate) fn packed_to_rgba(pixels: &[u8], channels: usize, width: u32, height: u32, rotation: u8, divisor: u32) -> (u32, u32, Vec<u8>) {
    let d = divisor.max(1) as usize;
    let (out_w, out_h) = output_size(width, height, rotation, divisor);
    let (out_w, out_h) = (out_w as usize, out_h as usize);
    let (width, height) = (width as usize, height as usize);
    let (w, h) = ((width / d).max(1), (height / d).max(1));
    let mut out = vec![255u8; out_w.saturating_mul(out_h).saturating_mul(4)];
    for row in 0..h {
        let sy = (row * d).min(height.saturating_sub(1));
        for col in 0..w {
            let sx = (col * d).min(width.saturating_sub(1));
            let at = (sy * width + sx) * channels;
            // outside the picture's data: black, never out of bounds
            let rgb = match (channels, pixels.get(at..at + channels)) {
                (1, Some(&[v])) => [v; 3],
                (3, Some(&[r, g, b])) | (4, Some(&[r, g, b, _])) => [r, g, b],
                _ => [0; 3],
            };
            let (x, y) = turned(col, row, w, h, rotation);
            if let Some(px) = out.get_mut((y * out_w + x) * 4..(y * out_w + x) * 4 + 3) {
                px.copy_from_slice(&rgb);
            }
        }
    }
    (out_w as u32, out_h as u32, out)
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
