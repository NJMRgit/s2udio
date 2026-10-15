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
