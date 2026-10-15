//! The engine sends masks as opaque grey RGBA PNGs, white where the provider should generate.
//! Providers want white-for-fill greyscale, black-for-fill greyscale (Ideogram), or the image
//! itself with a transparent hole (OpenAI); these convert. All bounded, none panic.

use crate::{GenError, Result};

/// Largest image converted here, in pixels (about 6300 x 6300).
pub const MAX_PIXELS: u64 = 40_000_000;
/// Decoder memory limit.
const DECODE_LIMIT: usize = 256 << 20;

fn invalid(msg: &str) -> GenError {
    GenError::Invalid(msg.to_owned())
}

/// Any 8- or 16-bit PNG as 8-bit RGBA.
pub fn decode_rgba8(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new_with_limits(std::io::Cursor::new(bytes), png::Limits { bytes: DECODE_LIMIT });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|_| invalid("the image isn't a readable PNG"))?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(invalid("the image is too large"));
    }
    let size = reader.output_buffer_size().ok_or_else(|| invalid("the image is too large"))?;
    let mut buf = vec![0; size];
    let frame = reader.next_frame(&mut buf).map_err(|_| invalid("the image isn't a readable PNG"))?;
    buf.truncate(frame.buffer_size());
    let rgba = match frame.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.as_chunks::<3>().0.iter().flat_map(|&[r, g, b]| [r, g, b, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.as_chunks::<2>().0.iter().flat_map(|&[v, a]| [v, v, v, a]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        png::ColorType::Indexed => return Err(invalid("the image isn't a readable PNG")),
    };
    if rgba.len() as u64 != u64::from(w) * u64::from(h) * 4 {
        return Err(invalid("the image isn't a readable PNG"));
    }
    Ok((w, h, rgba))
}

fn encode(w: u32, h: u32, color: png::ColorType, data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut e = png::Encoder::new(&mut out, w, h);
    e.set_color(color);
    e.set_depth(png::BitDepth::Eight);
    let mut wr = e.write_header().map_err(|_| invalid("could not encode the mask"))?;
    wr.write_image_data(data).map_err(|_| invalid("could not encode the mask"))?;
    wr.finish().map_err(|_| invalid("could not encode the mask"))?;
    Ok(out)
}

/// How much each pixel is to be generated, 0..=255 (the red channel times alpha).
pub fn coverage(mask: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let (w, h, px) = decode_rgba8(mask)?;
    let cov = px.as_chunks::<4>().0.iter().map(|&[r, _, _, a]| ((u16::from(r) * u16::from(a) + 127) / 255) as u8).collect();
    Ok((w, h, cov))
}

/// Greyscale, white = generate.
pub fn white_fill(mask: &[u8]) -> Result<Vec<u8>> {
    let (w, h, cov) = coverage(mask)?;
    encode(w, h, png::ColorType::Grayscale, &cov)
}

/// Greyscale, black = generate (Ideogram).
pub fn black_fill(mask: &[u8]) -> Result<Vec<u8>> {
    let (w, h, cov) = coverage(mask)?;
    let inv: Vec<u8> = cov.iter().map(|c| 255 - c).collect();
    encode(w, h, png::ColorType::Grayscale, &inv)
}

/// The image with alpha 0 where at least half covered (OpenAI: fully transparent = edit).
pub fn alpha_hole(image: &[u8], mask: &[u8]) -> Result<Vec<u8>> {
    let (w, h, mut px) = decode_rgba8(image)?;
    let (mw, mh, cov) = coverage(mask)?;
    if (mw, mh) != (w, h) {
        return Err(invalid("the mask must be the image's size"));
    }
    for (p, &c) in px.as_chunks_mut::<4>().0.iter_mut().zip(&cov) {
        p[3] = if c >= 128 { 0 } else { 255 };
    }
    encode(w, h, png::ColorType::Rgba, &px)
}

/// The image as an RGB PNG (alpha dropped), a quarter smaller for size-limited uploads.
pub fn opaque_rgb(image: &[u8]) -> Result<Vec<u8>> {
    let (w, h, px) = decode_rgba8(image)?;
    let rgb: Vec<u8> = px.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| [r, g, b]).collect();
    encode(w, h, png::ColorType::Rgb, &rgb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{mask_png, png};

    #[test]
    fn white_fill_keeps_and_black_fill_inverts_coverage() {
        let (w, h, white) = decode_rgba8(&white_fill(&mask_png(4, 2)).unwrap()).unwrap();
        assert_eq!((w, h), (4, 2));
        assert_eq!(&white[0..4], &[255, 255, 255, 255], "left: fill");
        assert_eq!(&white[12..16], &[0, 0, 0, 255], "right: keep");
        let (_, _, black) = decode_rgba8(&black_fill(&mask_png(4, 2)).unwrap()).unwrap();
        assert_eq!(&black[0..4], &[0, 0, 0, 255]);
        assert_eq!(&black[12..16], &[255, 255, 255, 255]);
    }

    #[test]
    fn partial_coverage_is_kept_in_grey_masks_and_thresholded_for_alpha() {
        let px: Vec<u8> = [0u8, 127, 128, 255].iter().flat_map(|&g| [g, g, g, 255]).collect();
        let mut out = Vec::new();
        {
            let mut e = png::Encoder::new(&mut out, 4, 1);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            let mut wr = e.write_header().unwrap();
            wr.write_image_data(&px).unwrap();
        }
        let (_, _, cov) = coverage(&out).unwrap();
        assert_eq!(cov, vec![0, 127, 128, 255]);
        let (_, _, hole) = decode_rgba8(&alpha_hole(&png(4, 1), &out).unwrap()).unwrap();
        let alpha: Vec<u8> = hole.chunks(4).map(|p| p[3]).collect();
        assert_eq!(alpha, vec![255, 255, 0, 0], "alpha 0 = edit, from 50% coverage up");
        assert_eq!(&hole[0..3], &[128, 128, 128], "the image's colour is kept");
    }

    #[test]
    fn alpha_hole_needs_matching_sizes_and_opaque_rgb_drops_alpha() {
        assert!(matches!(alpha_hole(&png(4, 4), &mask_png(4, 2)), Err(GenError::Invalid(_))));
        let rgb = opaque_rgb(&png(3, 3)).unwrap();
        let mut d = png::Decoder::new(std::io::Cursor::new(&rgb[..]));
        d.set_transformations(png::Transformations::IDENTITY);
        assert_eq!(d.read_info().unwrap().info().color_type, png::ColorType::Rgb);
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    #[test]
    fn junk_and_huge_headers_are_errors_not_panics() {
        assert!(decode_rgba8(b"not a png").is_err());
        assert!(decode_rgba8(&[]).is_err());
        let mut huge = png(1, 1);
        // IHDR width/height live at bytes 16..24; claim 100000 x 100000, then fix the chunk CRC
        // (over "IHDR" + 13 data bytes, bytes 12..29) so the decoder reaches the size guard.
        huge[16..20].copy_from_slice(&100_000u32.to_be_bytes());
        huge[20..24].copy_from_slice(&100_000u32.to_be_bytes());
        let crc = crc32(&huge[12..29]);
        huge[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(decode_rgba8(&huge), Err(GenError::Invalid("the image is too large".into())));
    }
}
