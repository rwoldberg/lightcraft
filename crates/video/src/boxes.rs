//! The few boxes the demuxer does not surface: an embedded preview image and the movie's
//! creation time. Only box headers are read, each level is bounded, and every offset is checked
//! against its parent: a malformed file yields `None`, never a panic or a large read.

use filmcraft_isobmff::ByteSource;

/// Boxes looked at per level before giving up (a real `moov` or `udta` holds a handful).
const MAX_BOXES: usize = 4096;

/// A preview image larger than this is not one.
pub(crate) const MAX_PREVIEW: u64 = 64 << 20;

/// A box's payload: `[start, end)` in the file.
#[derive(Clone, Copy)]
struct Span {
    start: u64,
    end: u64,
}

fn read<const N: usize>(src: &dyn ByteSource, at: u64, end: u64) -> Option<[u8; N]> {
    if at.checked_add(N as u64)? > end {
        return None;
    }
    let mut buf = [0u8; N];
    src.read_at(at, &mut buf).ok()?;
    Some(buf)
}

/// The first box of type `kind` directly inside `within`.
fn find(src: &dyn ByteSource, within: Span, kind: &[u8; 4]) -> Option<Span> {
    let mut at = within.start;
    for _ in 0..MAX_BOXES {
        let head: [u8; 8] = read(src, at, within.end)?;
        let size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
        let (header, size) = match size {
            // the box runs to the end of its parent
            0 => (8, within.end.checked_sub(at)?),
            1 => (16, u64::from_be_bytes(read(src, at.checked_add(8)?, within.end)?)),
            n => (8, n),
        };
        let end = at.checked_add(size)?;
        if size < header || end > within.end {
            return None;
        }
        if &head[4..8] == kind {
            return Some(Span { start: at + header, end });
        }
        at = end;
    }
    None
}

fn path(src: &dyn ByteSource, mut within: Span, kinds: &[&[u8; 4]]) -> Option<Span> {
    for kind in kinds {
        within = find(src, within, kind)?;
    }
    Some(within)
}

/// The children of a `meta` box. ISO files start it with a version/flags word, QuickTime files
/// don't: there the first child's type sits where ISO has its first child's size.
fn meta_children(src: &dyn ByteSource, meta: Span) -> Option<Span> {
    let head: [u8; 8] = read(src, meta.start, meta.end)?;
    let quicktime = head[4..8].iter().all(|b| b.is_ascii_alphanumeric() || *b == b' ');
    Some(Span { start: if quicktime { meta.start } else { meta.start.checked_add(4)? }, end: meta.end })
}

fn is_image(src: &dyn ByteSource, span: Span) -> bool {
    let len = span.end.saturating_sub(span.start);
    if !(8..=MAX_PREVIEW).contains(&len) {
        return false;
    }
    let Some(magic) = read::<4>(src, span.start, span.end) else { return false };
    magic[..3] == [0xff, 0xd8, 0xff] || magic == [0x89, b'P', b'N', b'G']
}

/// Where the file keeps a preview image (a JPEG or PNG): cover art in the metadata list
/// (`meta/ilst/covr`, under `moov` or `moov/udta`), or the thumbnail some cameras store in
/// `moov/udta` (`CNTH/CNDA`). → (offset, length).
pub(crate) fn preview(src: &dyn ByteSource) -> Option<(u64, u64)> {
    let file = Span { start: 0, end: src.len() };
    let moov = find(src, file, b"moov")?;
    let udta = find(src, moov, b"udta");
    let cover = |parent: Span| {
        let meta = meta_children(src, find(src, parent, b"meta")?)?;
        let data = path(src, meta, &[b"ilst", b"covr", b"data"])?;
        // a `data` box: type indicator and locale, then the image
        Some(Span { start: data.start.checked_add(8)?, end: data.end })
    };
    let camera = |udta: Span| path(src, udta, &[b"CNTH", b"CNDA"]);
    [udta.and_then(cover), cover(moov), udta.and_then(camera)]
        .into_iter()
        .flatten()
        .find(|span| is_image(src, *span))
        .map(|span| (span.start, span.end - span.start))
}

/// The movie header's creation time as seconds since 1970 (UTC), when it is set.
pub(crate) fn created(src: &dyn ByteSource) -> Option<i64> {
    let file = Span { start: 0, end: src.len() };
    let mvhd = path(src, file, &[b"moov", b"mvhd"])?;
    let version = read::<1>(src, mvhd.start, mvhd.end)?[0];
    let at = mvhd.start.checked_add(4)?;
    let since_1904 = match version {
        0 => u64::from(u32::from_be_bytes(read(src, at, mvhd.end)?)),
        1 => u64::from_be_bytes(read(src, at, mvhd.end)?),
        _ => return None,
    };
    // QuickTime counts from 1904; a zero (or pre-1970) time means "not set"
    let unix = i64::try_from(since_1904).ok()?.checked_sub(2_082_844_800)?;
    (unix > 0).then_some(unix)
}

/// `YYYY-MM-DDTHH:MM:SS` (UTC) for seconds since 1970.
pub(crate) fn iso(unix: i64) -> Option<String> {
    let (days, secs) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    // days since 1970 → civil date (proleptic Gregorian), in 400-year eras starting on 1 March
    let z = days.checked_add(719_468)?;
    let (era, day_of_era) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let m = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * m + 2) / 5 + 1;
    let month = if m < 10 { m + 3 } else { m - 9 };
    let year = year_of_era.checked_add(era.checked_mul(400)?)? + i64::from(month <= 2);
    if !(1970..=9999).contains(&year) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(iso(0).as_deref(), Some("1970-01-01T00:00:00"));
        assert_eq!(iso(951_782_400).as_deref(), Some("2000-02-29T00:00:00"));
        assert_eq!(iso(1_728_561_723).as_deref(), Some("2024-10-10T12:02:03"));
        assert_eq!(iso(i64::MAX), None);
    }

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn boxes_that_lie_about_their_size_are_not_followed() {
        let jpeg = [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 4, 5];
        let cnth = bx(b"CNTH", &bx(b"CNDA", &jpeg));
        let good = bx(b"moov", &bx(b"udta", &cnth));
        let (at, len) = preview(&good).unwrap();
        assert_eq!(&good[at as usize..(at + len) as usize], &jpeg);
        // every truncation and every single-byte change of the sizes: found or not, never a panic
        for cut in 0..good.len() {
            let _ = preview(&good[..cut].to_vec());
        }
        for i in 0..good.len() {
            for v in [0u8, 1, 0x7f, 0xff] {
                let mut bad = good.clone();
                bad[i] = v;
                let _ = preview(&bad);
                let _ = created(&bad);
            }
        }
        // a box that claims to hold itself forever
        let mut looping = bx(b"moov", &[]);
        looping[..4].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(preview(&looping), None);
    }
}
