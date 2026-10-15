use std::io::Cursor;

use anyhow::{Context, Result};
use image::{
    AnimationDecoder, DynamicImage, ImageDecoder, ImageEncoder,
    codecs::{gif::GifDecoder, jpeg::JpegEncoder, png::PngEncoder},
};
use ratatui::layout::Rect;

use crate::config::{
    Size,
    album_art::{HorizontalAlign, VerticalAlign},
};

#[derive(Debug, Clone, Copy)]
pub struct AlignedArea {
    pub area: Rect,
    pub size_px: Size,
    pub cell_width_px: f64,
    pub cell_height_px: f64,
}

#[derive(Debug, Clone, Copy)]
struct SizePx {
    width: f64,
    height: f64,
}

/// Returns a new aligned area contained by [`available_area`] with aspect ratio
/// provided by [`image_size`]. Constrains area by [`max_size_px`].
/// Returns the input [`available_area`] and [`max_size_px`] if terminal's size
/// cannot be determined properly. Also returns resulting area size in pixels.
pub fn create_aligned_area(
    available_area_cells: Rect,
    image_size_px: (u32, u32),
    max_size_px: Size,
    halign: HorizontalAlign,
    valign: VerticalAlign,
) -> AlignedArea {
    // Validate areas, sizes
    if image_size_px.0 == 0 || image_size_px.1 == 0 {
        log::warn!(available_area_cells:?, max_size_px:?; "Invalid image size");
        return AlignedArea {
            area: available_area_cells,
            size_px: Size { width: 0, height: 0 },
            cell_width_px: 0.0,
            cell_height_px: 0.0,
        };
    }
    if max_size_px.width == 0 || max_size_px.height == 0 {
        log::warn!(available_area_cells:?, max_size_px:?; "Max size is zero, cannot render image");
        return AlignedArea {
            area: available_area_cells,
            size_px: Size { width: 0, height: 0 },
            cell_width_px: 0.0,
            cell_height_px: 0.0,
        };
    }
    if available_area_cells.width == 0 || available_area_cells.height == 0 {
        log::warn!(available_area_cells:?, max_size_px:?; "Available area is empty, cannot render image");
        return AlignedArea {
            area: available_area_cells,
            size_px: Size { width: 0, height: 0 },
            cell_width_px: 0.0,
            cell_height_px: 0.0,
        };
    }

    // Query terminal size and calculate cell size in pixels
    let Ok(window_size) = crossterm::terminal::window_size() else {
        log::warn!(available_area_cells:?, max_size_px:?; "Failed to query terminal size");
        return AlignedArea {
            area: available_area_cells,
            size_px: max_size_px,
            cell_width_px: 0.0,
            cell_height_px: 0.0,
        };
    };

    if window_size.width == 0
        || window_size.height == 0
        || window_size.rows == 0
        || window_size.columns == 0
    {
        log::warn!(available_area_cells:?, max_size_px:?; "Terminal returned invalid size");
        return AlignedArea {
            area: available_area_cells,
            size_px: max_size_px,
            cell_width_px: 0.0,
            cell_height_px: 0.0,
        };
    }

    let cell_width = window_size.width as f64 / window_size.columns as f64;
    let cell_height = window_size.height as f64 / window_size.rows as f64;
    log::debug!(window_size:?, cell_width:?, cell_height:?; "Terminal size");

    // Convert available area to pixel space and build pixel bounds by clamping to
    // max_size_px
    let area_px = SizePx {
        width: available_area_cells.width as f64 * cell_width,
        height: available_area_cells.height as f64 * cell_height,
    };
    let bounds_px = SizePx {
        width: area_px.width.min(max_size_px.width as f64),
        height: area_px.height.min(max_size_px.height as f64),
    };

    let img_px = SizePx { width: image_size_px.0 as f64, height: image_size_px.1 as f64 };
    log::debug!(img_px:?, area_px:?, bounds_px:?, available_cells:? = available_area_cells; "Image and bounds");

    // Fit image into the pixel bounds while preserving aspect ratio
    let used_px = {
        let scale = (bounds_px.width / img_px.width).min(bounds_px.height / img_px.height);
        SizePx { width: img_px.width * scale, height: img_px.height * scale }
    };
    log::debug!(used_px:?; "Used size in pixels");

    // Convert to cell units (ceil to ensure the allocated cell box can contain the
    // used_px) Clamp to available area to guard against any floating-point edge
    // cases
    let mut used_cells = Size {
        width: (used_px.width / cell_width).ceil() as u16,
        height: (used_px.height / cell_height).ceil() as u16,
    };
    used_cells.width = used_cells.width.min(available_area_cells.width).max(1);
    used_cells.height = used_cells.height.min(available_area_cells.height).max(1);

    log::debug!(used_px:?, used_cells:?; "Used size in pixels and cells");

    let x = match halign {
        HorizontalAlign::Left => available_area_cells.x,
        HorizontalAlign::Center => {
            let offset = available_area_cells.width.saturating_sub(used_cells.width) / 2;
            available_area_cells.x + offset
        }
        HorizontalAlign::Right => {
            available_area_cells.x + available_area_cells.width.saturating_sub(used_cells.width)
        }
    };
    let y = match valign {
        VerticalAlign::Top => available_area_cells.y,
        VerticalAlign::Center => {
            let offset = available_area_cells.height.saturating_sub(used_cells.height) / 2;
            available_area_cells.y + offset
        }
        VerticalAlign::Bottom => {
            available_area_cells.y + available_area_cells.height.saturating_sub(used_cells.height)
        }
    };

    let result = AlignedArea {
        area: Rect::new(x, y, used_cells.width, used_cells.height),
        size_px: Size { width: used_px.width as u16, height: used_px.height as u16 },
        cell_width_px: cell_width,
        cell_height_px: cell_height,
    };
    log::debug!(result:?, available_area_cells:?, cell_width, cell_height, image_size_px:?, max_size_px:?, window_size:?; "Aligned area");

    result
}

pub fn resize_image(
    image_data: &[u8],
    available_area: Rect,
    max_size_px: Size,
    halign: HorizontalAlign,
    valign: VerticalAlign,
) -> Result<(DynamicImage, AlignedArea)> {
    let image = image::ImageReader::new(Cursor::new(image_data))
        .with_guessed_format()
        .context("Unable to guess image format")?
        .decode()
        .context("Unable to decode image")?;

    let result_area = create_aligned_area(
        available_area,
        (image.width(), image.height()),
        max_size_px,
        halign,
        valign,
    );

    let result = image.resize(
        result_area.size_px.width.into(),
        result_area.size_px.height.into(),
        image::imageops::FilterType::Lanczos3,
    );

    Ok((result, result_area))
}

pub fn jpg_encode(img: &DynamicImage) -> Result<Vec<u8>> {
    let mut jpg = Vec::new();
    JpegEncoder::new(&mut jpg).encode_image(img)?;
    Ok(jpg)
}

/// Per-channel tolerance for "this pixel is the same colour as the bar".
/// JPEG ringing across a flat bar leaves it a few levels off the corner
/// pixel, and a byte-exact scan measures the bars inconsistently — measured
/// on real thumbnails, byte-exact widths came out 31/39 on a sample whose
/// true bars are 43/40, which then failed the pair-similarity test and left
/// the letterbox in place.
const LETTERBOX_BAR_TOLERANCE: i32 = 20;
/// The two bars of a pair may differ in size by at most this ratio. A real
/// letterbox is roughly even; a picture that merely happens to have a flat
/// edge is not. Measured on `hqdefault.jpg`: 43/40 … 45/45 (0.93 … 1.00).
const LETTERBOX_PAIR_SIMILARITY: f64 = 0.80;
/// A bar pair below this fraction of its axis is noise, not letterboxing.
const LETTERBOX_MIN_FRACTION: f64 = 0.02;

/// Whether two colours are the same within [`LETTERBOX_BAR_TOLERANCE`].
fn same_bar_colour(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b.iter()).all(|(x, y)| (*x as i32 - *y as i32).abs() <= LETTERBOX_BAR_TOLERANCE)
}

/// The bar widths at the two ends of one axis.
///
/// A row/column counts as bar only when it is uniform across its **whole
/// extent** and matches the corner colour that seeds the run. Scanning the
/// full row (not a single sample pixel) is what keeps ordinary artwork out:
/// real covers contain flat runs inside the picture, and a sampled scan
/// reports those as borders.
///
/// `lines[y]` yields the pixels of line `y` along the axis being measured.
fn uniform_edges(
    lines: impl Iterator<Item = Vec<[u8; 3]>>,
    len: usize,
) -> (usize, usize) {
    let lines: Vec<Vec<[u8; 3]>> = lines.collect();
    if lines.is_empty() {
        return (0, 0);
    }
    let Some(first_seed) = lines[0].first().copied() else { return (0, 0) };
    let mut start = 0usize;
    for line in &lines {
        let Some(px) = line.first().copied() else { break };
        // Uniform across the row AND matching the seed: a bar spans the full
        // width, so any picture content in the row ends the run.
        if same_bar_colour(px, first_seed) && line.iter().all(|p| same_bar_colour(*p, first_seed)) {
            start += 1;
        } else {
            break;
        }
    }
    let Some(last_seed) = lines.last().and_then(|l| l.first().copied()) else {
        return (0, 0);
    };
    let mut end = 0usize;
    for line in lines.iter().rev() {
        let Some(px) = line.first().copied() else { break };
        if same_bar_colour(px, last_seed) && line.iter().all(|p| same_bar_colour(*p, last_seed)) {
            end += 1;
        } else {
            break;
        }
    }
    // A fully uniform axis is a solid image, not a letterbox.
    if start >= len || end >= len || start + end >= len {
        return (0, 0);
    }
    (start, end)
}

/// Whether a pair of bars is a *symmetric* letterbox/pillarbox pair worth
/// trimming. Requiring **both** bars is what protects genuine artwork: a real
/// cover can easily have a uniform run on one edge only (measured:
/// `maxresdefault.jpg` has 69 uniform columns on its right edge and none on
/// its left), and trimming that would silently crop the picture.
fn is_crop_pair(lead: usize, trail: usize, len: usize) -> bool {
    if lead == 0 || trail == 0 {
        return false;
    }
    let (small, large) = (lead.min(trail) as f64, lead.max(trail) as f64);
    if large <= 0.0 || small / large < LETTERBOX_PAIR_SIMILARITY {
        return false;
    }
    ((lead + trail) as f64) / (len as f64) >= LETTERBOX_MIN_FRACTION
}

/// Crop the symmetric letterbox (top/bottom) or pillarbox (left/right) bars
/// off a stream thumbnail, returning the encoded PNG bytes.
///
/// YouTube thumbnails are 4:3 letterboxes of a 16:9 picture — `hqdefault.jpg`
/// is 480x360 with 43-45 px of solid bar top and bottom — so the picture does
/// not fill a 16:9 art box. Trimming the bars yields the true ratio.
///
/// Undecodable bytes, bar-free artwork and one-sided uniform edges are
/// returned **byte-identically** untouched; the caller can therefore apply
/// this unconditionally.
pub fn crop_letterbox_bytes(data: &[u8], enabled: bool) -> Vec<u8> {
    if !enabled {
        return data.to_vec();
    }
    let Ok(image) = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .and_then(|reader| reader.decode().map_err(std::io::Error::other))
    else {
        return data.to_vec();
    };
    let Some(cropped) = crop_letterbox(&image) else {
        return data.to_vec();
    };
    let mut png = Vec::new();
    let encoder = PngEncoder::new(&mut png);
    if encoder
        .write_image(
            cropped.as_bytes(),
            cropped.width(),
            cropped.height(),
            cropped.color().into(),
        )
        .is_err()
    {
        return data.to_vec();
    }
    png
}

/// The image-level crop behind [`crop_letterbox_bytes`]. `None` when nothing
/// worth cropping was found (so the caller keeps the original bytes).
pub fn crop_letterbox(image: &DynamicImage) -> Option<DynamicImage> {
    let rgb = image.to_rgb8();
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    if w < 3 || h < 3 {
        return None;
    }
    // Top/bottom bars: each row is measured across its **whole width**, so
    // flat runs inside the picture never read as a bar.
    let (top, bottom) = uniform_edges(
        (0..h as u32).map(|y| {
            (0..w as u32).map(|x| rgb.get_pixel(x, y).0).collect::<Vec<[u8; 3]>>()
        }),
        h,
    );
    if is_crop_pair(top, bottom, h) {
        let new_h = u32::try_from(h - top - bottom).ok()?;
        return Some(image.crop_imm(0, u32::try_from(top).ok()?, image.width(), new_h));
    }
    // Left/right bars: each column is measured across its whole height.
    let (left, right) = uniform_edges(
        (0..w as u32).map(|x| {
            (0..h as u32).map(|y| rgb.get_pixel(x, y).0).collect::<Vec<[u8; 3]>>()
        }),
        w,
    );
    if is_crop_pair(left, right, w) {
        let new_w = u32::try_from(w - left - right).ok()?;
        return Some(image.crop_imm(u32::try_from(left).ok()?, 0, new_w, image.height()));
    }
    None
}

pub struct GifData<'frames> {
    pub frames: image::Frames<'frames>,
    pub dimensions: (u32, u32),
}

pub fn get_gif_frames(data: &[u8]) -> Result<Option<GifData<'_>>> {
    // http://www.matthewflickinger.com/lab/whatsinagif/bits_and_bytes.asp
    if data.len() < 6 || data[0..6] != *b"GIF89a" {
        return Ok(None);
    }

    if GifDecoder::new(Cursor::new(data))?.into_frames().take(2).count() > 1 {
        let gif = GifDecoder::new(Cursor::new(data))?;
        Ok(Some(GifData { dimensions: gif.dimensions(), frames: gif.into_frames() }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod letterbox_crop_tests {
    use super::{crop_letterbox, crop_letterbox_bytes};
    use image::{DynamicImage, Rgb, RgbImage};

    /// A 16:9-ish picture with solid bars top/bottom (the `hqdefault` shape).
    fn letterboxed(w: u32, h: u32, bar: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = if y < bar || y >= h - bar {
                Rgb([0, 0, 0])
            } else {
                // A non-uniform "picture" so a crop cannot pass by accident.
                Rgb([(x % 251) as u8, (y % 241) as u8, 128])
            };
        }
        DynamicImage::ImageRgb8(img)
    }

    /// A picture with solid bars left/right (pillarbox).
    fn pillarboxed(w: u32, h: u32, bar: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = if x < bar || x >= w - bar {
                Rgb([0, 0, 0])
            } else {
                Rgb([(x % 251) as u8, (y % 241) as u8, 128])
            };
        }
        DynamicImage::ImageRgb8(img)
    }

    /// Bar-free artwork: fully non-uniform, so nothing may be trimmed.
    fn plain(w: u32, h: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8]);
        }
        DynamicImage::ImageRgb8(img)
    }

    /// The `maxresdefault` shape: a uniform run on ONE edge only, which must
    /// never be trimmed (it is real artwork that happens to end uniformly).
    fn one_sided_uniform(w: u32, h: u32, uniform_cols: u32) -> DynamicImage {
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = if x >= w - uniform_cols {
                Rgb([7, 7, 7])
            } else {
                Rgb([(x % 251) as u8, (y % 241) as u8, 90])
            };
        }
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn trims_letterbox_bars() {
        let img = letterboxed(480, 360, 45);
        let cropped = crop_letterbox(&img).expect("the bars are a symmetric pair");
        assert_eq!(cropped.height(), 360 - 90, "top and bottom bars are both trimmed");
        assert_eq!(cropped.width(), 480, "width is untouched");
    }

    #[test]
    fn trims_pillarbox_bars() {
        let img = pillarboxed(480, 360, 40);
        let cropped = crop_letterbox(&img).expect("the bars are a symmetric pair");
        assert_eq!(cropped.width(), 480 - 80, "left and right bars are both trimmed");
        assert_eq!(cropped.height(), 360);
    }

    #[test]
    fn leaves_bar_free_artwork_alone() {
        assert!(crop_letterbox(&plain(480, 360)).is_none(), "no bars to trim");
    }

    /// The measured `maxresdefault` case: a long uniform run on one edge and
    /// none on the other is artwork, not a letterbox.
    #[test]
    fn leaves_a_one_sided_uniform_edge_alone() {
        let img = one_sided_uniform(1280, 720, 69);
        assert!(
            crop_letterbox(&img).is_none(),
            "a one-sided uniform edge is never cropped"
        );
    }

    /// A one-sided letterbox (bars top, none bottom) is not a symmetric pair.
    #[test]
    fn leaves_a_one_sided_letterbox_alone() {
        let mut img = RgbImage::new(480, 360);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = if y < 45 { Rgb([0, 0, 0]) } else { Rgb([(x % 251) as u8, (y % 241) as u8, 128]) };
        }
        assert!(crop_letterbox(&DynamicImage::ImageRgb8(img)).is_none());
    }

    /// A big bar on one side and a sliver on the other is asymmetry, not a
    /// letterbox: the size ratio guard rejects it.
    #[test]
    fn leaves_an_asymmetric_pair_alone() {
        let mut img = RgbImage::new(480, 360);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = if y < 80 || y >= 358 {
                Rgb([0, 0, 0])
            } else {
                Rgb([(x % 251) as u8, (y % 241) as u8, 128])
            };
        }
        assert!(
            crop_letterbox(&DynamicImage::ImageRgb8(img)).is_none(),
            "80 px against 2 px is not a comparable pair"
        );
    }

    /// A solid image is not a letterbox: cropping it would delete the image.
    #[test]
    fn refuses_to_crop_away_a_solid_image() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(480, 360, Rgb([9, 9, 9])));
        assert!(crop_letterbox(&img).is_none());
    }

    /// A pair too small to matter is left alone (`min/max` and fraction).
    #[test]
    fn ignores_a_negligible_pair() {
        // 2 px of 360 = 0.55% per side, together 1.1% < 2%.
        let img = letterboxed(480, 360, 2);
        assert!(crop_letterbox(&img).is_none(), "a hairline border is not a letterbox");
    }

    /// The crop keeps the picture, not the bars: the first row of the result
    /// is picture content.
    #[test]
    fn keeps_the_picture_not_the_bars() {
        let img = letterboxed(480, 360, 45);
        let cropped = crop_letterbox(&img).expect("bars are trimmed");
        let rgb = cropped.to_rgb8();
        let first = rgb.get_pixel(0, 0).0;
        assert_ne!(first, [0, 0, 0], "the bar was removed, not kept");
    }

    // ── the byte-level helper the pane and the MPRIS writers share ──────

    fn jpeg_bytes(img: &DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new(&mut out);
        enc.encode_image(img).expect("encode");
        out
    }

    #[test]
    fn crop_bytes_trims_a_letterboxed_thumbnail() {
        let data = jpeg_bytes(&letterboxed(480, 360, 45));
        let out = crop_letterbox_bytes(&data, true);
        assert_ne!(out, data, "the bars were trimmed");
        let decoded = image::load_from_memory(&out).expect("decodes");
        assert!(
            decoded.height() < 360,
            "the cropped image is shorter: {}",
            decoded.height()
        );
    }

    #[test]
    fn crop_bytes_passes_bar_free_artwork_through_byte_identically() {
        let data = jpeg_bytes(&plain(480, 360));
        assert_eq!(crop_letterbox_bytes(&data, true), data, "no crop, no re-encode");
    }

    #[test]
    fn crop_bytes_returns_undecodable_bytes_verbatim() {
        let junk = b"not an image at all".to_vec();
        assert_eq!(crop_letterbox_bytes(&junk, true), junk);
    }

    #[test]
    fn crop_bytes_is_disabled_by_the_option() {
        let data = jpeg_bytes(&letterboxed(480, 360, 45));
        assert_eq!(
            crop_letterbox_bytes(&data, false),
            data,
            "crop_letterbox: false leaves the bytes untouched"
        );
    }
}
