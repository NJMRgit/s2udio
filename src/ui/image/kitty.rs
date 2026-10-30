use std::{
    io::Write,
    sync::atomic::{AtomicU32, Ordering},
    time::Instant,
};

use anyhow::{Context, Result};
use base64::Engine;
use crossterm::{
    execute,
    style::{Colors, SetColors},
};
use flate2::Compression;
use itertools::Itertools;
use ratatui::prelude::{IntoCrossterm, Rect};

use super::{Backend, csi_move};
use crate::{
    config::{
        Size,
        album_art::{HorizontalAlign, VerticalAlign},
    },
    ctx::Ctx,
    shared::{
        image::{create_aligned_area, get_gif_frames, resize_image},
        tmux::tmux_write,
    },
    try_skip,
};

/// Where album-art image ids start.
///
/// The id has to be unique across every image the app transmits, because kitty
/// keys its image store by it. Three allocators coexist: the Home carousel
/// owns a small contiguous block starting at 2 (one per slot), id 1 is the
/// legacy single-image poster id, and the album art / poster overlay path
/// takes ids from here — far above the wall's block, so a growing wall can
/// never walk into the art's ids.
const IMAGE_ID_BASE: u32 = 1 << 16;
/// The highest id a placeholder cell can carry: the id IS the cell's
/// foreground colour, and kitty reads it as a **24-bit** value in true colour
/// mode (8-bit in 256-colour mode — see [`placeholder_foreground`]).
const IMAGE_ID_MAX: u32 = 1 << 24;
static NEXT_IMAGE_ID: AtomicU32 = AtomicU32::new(IMAGE_ID_BASE);

/// A fresh image id for one encoded payload, always inside the placeholder
/// space (`IMAGE_ID_BASE..IMAGE_ID_MAX`).
pub fn next_image_id() -> u32 {
    let raw = NEXT_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
    IMAGE_ID_BASE + raw % (IMAGE_ID_MAX - IMAGE_ID_BASE)
}

/// The escape that colours a placeholder cell with its image id.
///
/// Kitty's unicode placeholders carry the image id in the cell's **foreground
/// colour**: "you are limited to either 8-bit IDs in 256 color mode or 24-bit
/// IDs in true color mode". The 256-colour form (`38;5;<n>`) tops out at 255,
/// so the moment image ids moved above that (this round's allocator starts at
/// 1<<16) every picture was transmitted correctly and then drawn as **nothing**
/// — the cells carried an id the terminal could not represent. The 24-bit form
/// carries any id below 2^24.
fn placeholder_foreground(image_id: u32) -> String {
    let r = (image_id >> 16) & 0xff;
    let g = (image_id >> 8) & 0xff;
    let b = image_id & 0xff;
    format!("\x1b[38;2;{r};{g};{b}m")
}

/// How many transmitted payloads one backend keeps alive in the terminal.
///
/// A re-place of a known id costs a placement command and the placeholder
/// cells — not the payload. The cap bounds the terminal's image store: art
/// changes on every track, and a long session must not accumulate every cover
/// it ever showed.
const TRANSMIT_CACHE: usize = 6;

/// The kitty backend for one overlay (album art, the Jellyfin poster, …).
///
/// Every encoded payload carries its own image id (`next_image_id`), so this
/// backend can:
///
/// * **transmit once and place many** — the payload goes over the wire when its
///   id is new, and every later draw of the same encode is a placement command
///   (`a=p`) plus the placeholder cells. Re-placing after the frame repainted
///   the pane, or after a resize, no longer re-sends the image;
/// * **delete only its own image** — `hide` frees this backend's image id
///   instead of nuking the terminal's whole image store (`a=d,d=A`), which used
///   to take the Jellyfin poster down with the album art. The payload is
///   dropped too (not just the placement): an overlay that comes back
///   re-transmits in one go, which is cheaper than trusting an id the terminal
///   may have dropped in the meantime (a window resize can clear kitty's
///   store);
/// * **keep a small payload cache** so toggling a pane back on is free, while
///   evicting the oldest payloads so the terminal's memory stays bounded.
#[derive(Debug, Default)]
pub struct Kitty {
    /// Transmitted payload ids, oldest first.
    transmitted: Vec<u32>,
    /// The id this backend currently displays (what `hide` deletes).
    current: Option<u32>,
    /// The cell rect [`Self::current`] was drawn into: a re-place at this size
    /// needs neither a clear nor a transfer, only the placement and the cells.
    current_area: Option<Rect>,
}

impl Kitty {
    pub fn new() -> Self {
        Self::default()
    }

    /// Send the payload only if the terminal does not already hold this id,
    /// then remember it (evicting the oldest payloads past [`TRANSMIT_CACHE`]).
    ///
    /// This is the whole point of the id model: the expensive, chunked,
    /// base64'd transfer happens once per encode, and every later draw of the
    /// same payload is a placement command plus the placeholder cells.
    fn transmit_once(&mut self, w: &mut impl Write, data: &ImageData) -> Result<()> {
        if !self.transmitted.contains(&data.image_id) {
            try_skip!(
                transfer_image_data(
                    w,
                    data.image_id,
                    &data.content,
                    data.img_width,
                    data.img_height,
                    data.is_compressed
                ),
                "Failed to transfer image data"
            );
            self.remember(data.image_id, w)?;
        }
        Ok(())
    }

    /// Draw `data`, honouring `reveal` by painting only the first
    /// `reveal_columns(...)` columns of the placeholder grid.
    fn paint(
        &mut self,
        w: &mut impl Write,
        data: Data,
        ctx: &Ctx,
        reveal: super::Reveal,
    ) -> Result<()> {
        let bg = ctx.config.theme.background_color.map(IntoCrossterm::into_crossterm);
        match data {
            Data::ImageData(data) => {
                let id = data.image_id;
                try_skip!(self.transmit_once(w, &data), "Failed to transfer image data");
                // Size the placement for the grid this frame paints: a re-place
                // is this command plus the cells, never another transfer.
                try_skip!(place_image(w, id, data.aligned_area), "Failed to place the image");
                try_skip!(
                    create_unicode_placeholder_grid(
                        w,
                        bg,
                        data.aligned_area,
                        id,
                        reveal_columns(data.aligned_area.width, reveal)
                    ),
                    "Failed to create unicode placeholders"
                );
                self.current = Some(id);
                self.current_area = Some(data.aligned_area);
            }
            Data::AnimationData(data) => {
                let id = data.image_id;
                let aligned_area = data.aligned_area;
                // A GIF is transmitted as a kitty animation: the `a=T` frame
                // carries the placement (and `c`/`r`), and the terminal plays
                // the remaining frames itself. An animation is not re-placed
                // mid-flight, so `reveal` does not apply to it.
                try_skip!(transfer_animation_data(w, data), "Failed to transfer animation data");
                try_skip!(
                    create_unicode_placeholder_grid(
                        w,
                        bg,
                        aligned_area,
                        id,
                        aligned_area.width
                    ),
                    "Failed to create unicode placeholders"
                );
                self.remember(id, w)?;
                self.current = Some(id);
                self.current_area = Some(aligned_area);
            }
        }
        Ok(())
    }

    /// Free the image this backend is showing (`d=I`) and forget it, so a later
    /// draw transmits again instead of trusting a stale id.
    fn retire(&mut self, w: &mut impl Write) -> Result<()> {
        let Some(id) = self.current.take() else {
            return Ok(());
        };
        self.current_area = None;
        self.transmitted.retain(|candidate| *candidate != id);
        try_skip!(delete_image(w, id), "Failed to free the image");
        Ok(())
    }

    /// Note a transmitted id and bound the terminal's image store.
    ///
    /// The id currently on screen is never evicted (its placement is what the
    /// user is looking at), so the cache can sit one over the cap for a frame.
    fn remember(&mut self, id: u32, w: &mut impl Write) -> Result<()> {
        if !self.transmitted.contains(&id) {
            self.transmitted.push(id);
        }
        while self.transmitted.len() > TRANSMIT_CACHE {
            let Some(pos) = self
                .transmitted
                .iter()
                .position(|candidate| Some(*candidate) != self.current)
            else {
                break;
            };
            let evicted = self.transmitted.remove(pos);
            try_skip!(delete_image(w, evicted), "Failed to free an evicted image");
        }
        Ok(())
    }
}

/// Size an already-transmitted image for a `cols` x `rows` cell rectangle so a
/// unicode-placeholder grid can display it (`a=p`).
///
/// Kitty takes the *position* from the placeholder cells, so this is the only
/// command a re-place needs: no payload, no delete, no flicker.
fn place_image(w: &mut impl Write, image_id: u32, area: Rect) -> Result<()> {
    if area.width == 0 || area.height == 0 {
        return Ok(());
    }
    tmux_write!(
        w,
        "\x1b_Ga=p,U=1,i={image_id},c={cols},r={rows},q=2\x1b\\",
        cols = area.width,
        rows = area.height
    )?;
    Ok(())
}

/// How many placeholder columns to paint for a reveal.
///
/// `None` is "all of it"; a fraction is how much of the image's **width** is
/// visible. The picture keeps its size and is clipped, so the reveal is a wipe
/// rather than a squeeze — cell (row, col) always maps to the same pixels.
pub(crate) fn reveal_columns(width: u16, reveal: super::Reveal) -> u16 {
    let Some(fraction) = reveal else {
        return width;
    };
    if fraction.is_nan() {
        return width;
    }
    let fraction = f64::from(fraction.clamp(0.0, 1.0));
    let columns = (f64::from(width) * fraction).round().clamp(0.0, f64::from(width));
    columns as u16
}

/// Free an image and all of its placements (`d=I`).
fn delete_image(w: &mut impl Write, image_id: u32) -> Result<()> {
    tmux_write!(w, "\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")?;
    Ok(())
}

impl Backend for Kitty {
    type EncodedData = Data;

    fn hide(
        &mut self,
        w: &mut impl Write,
        area: Rect,
        bg_color: Option<crossterm::style::Color>,
    ) -> Result<()> {
        // Blank the cells the placeholders occupied (the tracked rect when we
        // know it: the caller's `area` is the pane, not the aligned image)...
        super::clear_area(w, bg_color, self.current_area.unwrap_or(area))?;
        self.current_area = None;
        // ...then free THIS backend's image only. The other overlays (the
        // Jellyfin poster, a Home wall tile) each own their own id now, so
        // they survive — a `d=A` here used to take the whole terminal's image
        // store with it.
        self.retire(w)?;

        Ok(())
    }

    fn display(&mut self, w: &mut impl Write, data: Self::EncodedData, ctx: &Ctx) -> Result<()> {
        self.paint(w, data, ctx, None)
    }

    /// Re-place an image this backend already transmitted (a frame repainted
    /// its cells, or the pane moved) without paying for the payload again; a
    /// genuinely different payload retires the old image first.
    fn replace(
        &mut self,
        w: &mut impl Write,
        // Unused: this backend clears the rect it actually drew (the aligned
        // image box), not the caller's pane rect.
        _size: Rect,
        bg_color: Option<crossterm::style::Color>,
        data: Self::EncodedData,
        ctx: &Ctx,
        reveal: super::Reveal,
    ) -> Result<()> {
        let (id, area) = match &data {
            Data::ImageData(data) => (data.image_id, data.aligned_area),
            Data::AnimationData(data) => (data.image_id, data.aligned_area),
        };
        // Same payload in the same box: nothing on screen has to move. This is
        // the path the reveal runs on, once per frame.
        if self.current == Some(id) && self.current_area == Some(area) {
            return self.paint(w, data, ctx, reveal);
        }
        // The image has to move (or a different one has to take its place):
        // blank what we drew, then drop the old payload unless this is the
        // same image at a new size — a resize keeps the bytes.
        if let Some(previous) = self.current_area {
            try_skip!(super::clear_area(w, bg_color, previous), "Failed to clear the old placement");
            self.current_area = None;
        }
        if self.current.is_some_and(|current| current != id) {
            try_skip!(self.retire(w), "Failed to free the previous image");
        }
        self.paint(w, data, ctx, reveal)
    }

    fn create_data(
        image_data: &[u8],
        area: Rect,
        max_size: Size,
        halign: HorizontalAlign,
        valign: VerticalAlign,
    ) -> Result<Self::EncodedData> {
        let start_time = Instant::now();
        log::debug!(bytes = image_data.len(); "Compressing image data");

        if let Some(data) = get_gif_frames(image_data)? {
            let frames = data.frames;
            let (width, height) = data.dimensions;

            let frames: Vec<AnimationFrame> = frames
                .map_ok(|frame| {
                    let delay = frame.delay().numer_denom_ms();

                    AnimationFrame {
                        delay: delay.0 / delay.1,
                        content: base64::engine::general_purpose::STANDARD
                            .encode(frame.buffer().as_raw()),
                    }
                })
                .try_collect()?;
            let aligned_area =
                create_aligned_area(area, (width, height), max_size, halign, valign).area;

            Ok(Data::AnimationData(AnimationData {
                image_id: next_image_id(),
                frames,
                is_compressed: false,
                img_width: width,
                img_height: height,
                aligned_area,
            }))
        } else {
            let (image, aligned_area) = resize_image(image_data, area, max_size, halign, valign)?;

            let rgba = image.to_rgba8();
            let payload = encode_image_payload(rgba.as_raw());

            // Round 111: report the *like-for-like* numbers. The old line
            // logged `input_bytes = image_data.len()` — the encoded source
            // file (WebP/JPEG, already compressed) — against
            // `compressed_bytes = content.len()`, the base64 of the
            // zlib-compressed **raw RGBA**. Those measure different things,
            // so the "+65%" in the report could not be read as a real
            // regression. `raw_bytes` is the correct baseline for a
            // compression ratio, and it is what decides whether the zlib
            // pass paid off.
            log::debug!(
                source_bytes = image_data.len(),
                raw_bytes = rgba.as_raw().len(),
                payload_bytes = payload.len(),
                compressed = payload.compressed,
                duration:? = start_time.elapsed();
                "Image payload prepared"
            );
            Ok(Data::ImageData(ImageData {
                // Allocated at encode time, not at draw time: the id is the
                // payload's identity, and it has to survive every re-place of
                // the same encode (see `Kitty`).
                image_id: next_image_id(),
                content: payload.content,
                is_compressed: payload.compressed,
                aligned_area: aligned_area.area,
                img_width: image.width(),
                img_height: image.height(),
            }))
        }
    }
}

/// The kitty payload for one still image, already base64-encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedPayload {
    pub content: String,
    /// Whether `content` is zlib-compressed (kitty's `o=z`).
    pub compressed: bool,
    /// The encoded payload length in bytes, before base64.
    pub encoded_len: usize,
}

impl EncodedPayload {
    /// The base64 payload length in bytes — what actually goes on the wire.
    #[must_use]
    pub fn len(&self) -> usize {
        self.content.len()
    }
}

/// zlib-compress a raw RGBA buffer and base64 it, **falling back to the
/// uncompressed bytes when zlib does not actually shrink the payload**.
///
/// Round 111: the report observed a kitty transfer where the "compressed"
/// payload was larger than the input. For photographic cover art the raw
/// RGBA is high-entropy, so zlib's overhead can outweigh its wins; and
/// because the data is then base64'd, spending CPU to make the payload
/// bigger is a straight loss on both the wire and the frame budget (this
/// app redraws art frequently in the quick-access terminal).
///
/// The comparison is against the **uncompressed** buffer length, which is
/// the only honest baseline: base64 inflates by 4/3 either way, so it
/// cancels out and the decision is made on the raw zlib result.
#[must_use]
pub fn encode_image_payload(raw_rgba: &[u8]) -> EncodedPayload {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), Compression::new(6));
    let compressed = match encoder.write_all(raw_rgba).and_then(|()| encoder.finish()) {
        Ok(compressed) => Some(compressed),
        Err(err) => {
            log::warn!(err:?; "zlib compression failed, sending the image uncompressed");
            None
        }
    };

    // Only keep the compressed form when it is genuinely smaller than the
    // raw buffer it replaces.
    match compressed {
        Some(compressed) if compressed.len() < raw_rgba.len() => EncodedPayload {
            encoded_len: compressed.len(),
            content: base64::engine::general_purpose::STANDARD.encode(&compressed),
            compressed: true,
        },
        _ => EncodedPayload {
            encoded_len: raw_rgba.len(),
            content: base64::engine::general_purpose::STANDARD.encode(raw_rgba),
            compressed: false,
        },
    }
}

fn create_unicode_placeholder_grid(
    w: &mut impl Write,
    bg_color: Option<crossterm::style::Color>,
    area: Rect,
    image_id: u32,
    columns: u16,
) -> Result<()> {
    let colors = Colors { background: bg_color, foreground: None };
    let mut buf = Vec::with_capacity(area.width as usize * area.height as usize * 2);
    execute!(buf, SetColors(colors))?;
    let columns = columns.min(area.width);
    if columns == 0 {
        return Ok(());
    }
    for y in 0..area.height {
        csi_move!(buf, area.left(), area.top() + y)?;
        // The placeholder's foreground colour IS the image id, so a second
        // overlay can no longer be mistaken for this one (the old grid
        // hardcoded id 1). 24-bit on purpose: see `placeholder_foreground`.
        buf.extend_from_slice(placeholder_foreground(image_id).as_bytes());

        for x in 0..columns {
            write!(buf, "{DELIM}{row}{col}", row = GRID[y as usize], col = GRID[x as usize])?;
        }

        write!(buf, "\x1b[39m")?;
    }
    w.write_all(&buf)?;
    w.flush()?;

    Ok(())
}

fn transfer_animation_data(w: &mut impl Write, data: AnimationData) -> Result<()> {
    let start_time = Instant::now();
    let AnimationData {
        image_id,
        frames,
        is_compressed,
        img_width,
        img_height,
        aligned_area,
    } = data;

    log::debug!(frames = frames.len(), image_id, img_width, img_height, aligned_area:?; "Transferring animation data");

    if frames.len() < 2 {
        log::warn!("Less than two frames, invalid animation data");
        return Ok(());
    }

    let mut first_frame_iter = frames[0].content.chars().peekable();
    let chunk: String = first_frame_iter.by_ref().take(4096).collect();

    let m = i32::from(first_frame_iter.peek().is_some());
    let delay = frames[0].delay;

    // Create image and transfer first frame
    tmux_write!(
        w,
        "\x1b_Gi={image_id},f=32,U=1,a=T,t=d,m={m},z={delay},q=2,s={img_width},v={img_height},c={cols},r={rows}{compression};{chunk}\x1b\\",
        compression = if is_compressed { ",o=z" } else { "" },
        cols = aligned_area.width,
        rows = aligned_area.height
    )?;

    // Transfer the rest of the first frame if any
    while first_frame_iter.peek().is_some() {
        let chunk: String = first_frame_iter.by_ref().take(4096).collect();
        let m = i32::from(first_frame_iter.peek().is_some());
        tmux_write!(w, "\x1b_Gi={image_id},m={m};{chunk}\x1b\\")?;
    }

    // Transfer rest of the frames, skip first because it was already
    // transferred
    for AnimationFrame { delay, content, .. } in frames.iter().skip(1) {
        let mut frame_iter = content.chars().peekable();
        let chunk: String = frame_iter.by_ref().take(4096).collect();

        tmux_write!(
            w,
            "\x1b_Gi={image_id},a=f,t=d,m={m},z={delay},q=2,s={img_width},v={img_height}{compression};{chunk}\x1b\\",
            compression = if is_compressed { ",o=z" } else { "" }
        )?;

        while frame_iter.peek().is_some() {
            let chunk: String = frame_iter.by_ref().take(4096).collect();
            let m = i32::from(frame_iter.peek().is_some());
            tmux_write!(w, "\x1b_Ga=f,i={image_id},m={m};{chunk}\x1b\\")?;
        }
    }

    // Run the animation
    tmux_write!(w, "\x1b_Ga=a,i={image_id},s=3\x1b\\")?;
    log::debug!(duration:? = start_time.elapsed(); "Transfer finished");

    Ok(())
}

/// Store a payload in the terminal's image store under `image_id`, **without
/// displaying it** (`a=t`, not `a=T`): the caller places it separately, so a
/// re-place never pays for the payload again.
fn transfer_image_data(
    w: &mut impl Write,
    image_id: u32,
    content: &str,
    img_width: u32,
    img_height: u32,
    is_compressed: bool,
) -> Result<()> {
    let start_time = Instant::now();
    log::debug!(
        bytes = content.len(), image_id, img_width, img_height, is_compressed;
        "Transferring image data"
    );
    let mut iter = content.chars().peekable();

    // Round 111: `o=z` is now conditional. It used to be hardcoded here
    // while the encoder decided independently, so a payload that zlib had
    // inflated could still be *declared* compressed — the terminal would
    // then fail to inflate it. The flag travels with the payload.
    let compression = if is_compressed { ",o=z" } else { "" };
    let first: String = iter.by_ref().take(4096).collect();
    // `m` says whether another chunk follows; a small payload must not claim
    // one (the terminal would wait for it forever).
    let more = i32::from(iter.peek().is_some());
    tmux_write!(
        w,
        "\x1b_Gi={image_id},f=32,U=1,t=d,a=t,m={more},q=2{compression},s={img_width},v={img_height};{first}\x1b\\"
    )?;

    while iter.peek().is_some() {
        let chunk: String = iter.by_ref().take(4096).collect();
        let m = i32::from(iter.peek().is_some());
        tmux_write!(w, "\x1b_Gi={image_id},m={m};{chunk}\x1b\\")?;
    }
    log::debug!(duration:? = start_time.elapsed(); "Transfer finished");

    Ok(())
}

#[derive(Debug, Clone)]
pub enum Data {
    ImageData(ImageData),
    AnimationData(AnimationData),
}

#[derive(derive_more::Debug, Clone)]
pub struct ImageData {
    /// The kitty image id this payload is stored under (`next_image_id`).
    image_id: u32,
    #[debug(skip)]
    content: String,
    /// Whether `content` is zlib-compressed and must be transferred with
    /// kitty's `o=z`. Carried with the payload so the encoder's decision and
    /// the transfer flag cannot disagree (round 111).
    is_compressed: bool,
    img_width: u32,
    img_height: u32,
    aligned_area: Rect,
}

#[derive(derive_more::Debug, Clone)]
pub struct AnimationFrame {
    #[debug(skip)]
    content: String,
    delay: u32,
}

#[derive(derive_more::Debug, Clone)]
pub struct AnimationData {
    /// The kitty image id the animation is stored under (`next_image_id`).
    image_id: u32,
    #[debug(skip)]
    frames: Vec<AnimationFrame>,
    is_compressed: bool,
    img_width: u32,
    img_height: u32,
    aligned_area: Rect,
}

/// One Home-carousel tile: an image encoded to fill exactly a `cols` x
/// `rows` cell rectangle (cover-cropped, so every card in a row has the same
/// shape), ready to be transferred under its own kitty image id.
///
/// The wall draws one of these per visible card, each with a distinct id;
/// the placeholder grid colours the cells with that id, which is how kitty
/// tells the images apart (the existing single-image poster path uses id 1).
#[derive(Debug, Clone)]
pub struct WallTile {
    pub id: u32,
    content: String,
    compressed: bool,
    pub cols: u16,
    pub rows: u16,
    pub width: u32,
    pub height: u32,
}

/// Encode `bytes` to exactly fill a `cols`x`rows` cell area, cropping the
/// source to the area's pixel aspect (a "cover" fit: posters stay posters,
/// thumbnails stay 16:9, and the carousel stays a clean grid).
pub fn create_wall_tile(
    bytes: &[u8],
    cols: u16,
    rows: u16,
    id: u32,
) -> Result<WallTile> {
    let image = image::load_from_memory(bytes).context("Unable to decode image")?;
    let (src_w, src_h) = (image.width(), image.height());
    if src_w == 0 || src_h == 0 || cols == 0 || rows == 0 {
        anyhow::bail!("invalid wall tile geometry");
    }
    // Terminal cell size in pixels; a missing window size falls back to a
    // typical 8x16 cell so the tile is still encoded at a sane resolution.
    let (cell_w, cell_h) = crossterm::terminal::window_size()
        .map(|ws| {
            if ws.columns == 0 || ws.rows == 0 {
                (8.0, 16.0)
            } else {
                (
                    ws.width as f64 / ws.columns as f64,
                    ws.height as f64 / ws.rows as f64,
                )
            }
        })
        .unwrap_or((8.0, 16.0));
    let target_w = ((f64::from(cols) * cell_w).round() as u32).clamp(16, 640);
    let target_h = ((f64::from(rows) * cell_h).round() as u32).clamp(16, 960);
    // Center crop to the target aspect, then resize to the exact pixel
    // rectangle (no letterboxing, no stretching).
    let target_aspect = f64::from(target_w) / f64::from(target_h);
    let src_aspect = f64::from(src_w) / f64::from(src_h);
    let (crop_w, crop_h) = if src_aspect > target_aspect {
        (((f64::from(src_h) * target_aspect).round() as u32).max(1), src_h)
    } else {
        (src_w, ((f64::from(src_w) / target_aspect).round() as u32).max(1))
    };
    let crop_x = (src_w.saturating_sub(crop_w)) / 2;
    let crop_y = (src_h.saturating_sub(crop_h)) / 2;
    let cropped = image.crop_imm(crop_x, crop_y, crop_w, crop_h);
    let resized = cropped.resize_exact(
        target_w,
        target_h,
        image::imageops::FilterType::Triangle,
    );
    let rgba = resized.to_rgba8();
    let payload = encode_image_payload(rgba.as_raw());
    Ok(WallTile {
        id,
        content: payload.content,
        compressed: payload.compressed,
        cols,
        rows,
        width: target_w,
        height: target_h,
    })
}

/// Transmit a wall tile under its image id and paint its placeholder grid
/// into `area` (cells must be exactly `tile.cols` x `tile.rows`). A retransfer
/// of an existing id requires that id to be deleted first, which
/// [`delete_wall_tile`] does.
pub fn display_wall_tile(
    w: &mut impl Write,
    tile: &WallTile,
    area: Rect,
    bg_color: Option<crossterm::style::Color>,
) -> Result<()> {
    let compression = if tile.compressed { ",o=z" } else { "" };
    let mut iter = tile.content.chars().peekable();
    let first: String = iter.by_ref().take(4096).collect();
    // `m` says whether another chunk follows; a small payload must NOT claim
    // one (the terminal would wait for it forever).
    let first_more = i32::from(iter.peek().is_some());
    tmux_write!(
        w,
        "\x1b_Ga=T,U=1,i={id},q=2,c={cols},r={rows},f=32{compression},s={width},v={height},m={first_more};{first}\x1b\\",
        id = tile.id,
        cols = tile.cols,
        rows = tile.rows,
        width = tile.width,
        height = tile.height,
    )?;
    while iter.peek().is_some() {
        let chunk: String = iter.by_ref().take(4096).collect();
        let more = i32::from(iter.peek().is_some());
        tmux_write!(w, "\x1b_Gi={id},m={more};{chunk}\x1b\\", id = tile.id)?;
    }
    // The placeholder cells: every cell names its image id in the
    // foreground colour and its (row, column) through combining diacritics.
    let colors = Colors { background: bg_color, foreground: None };
    let mut buf = Vec::with_capacity(usize::from(area.width) * usize::from(area.height) * 8);
    execute!(buf, crossterm::cursor::SavePosition, SetColors(colors))?;
    buf.extend_from_slice(placeholder_foreground(tile.id).as_bytes());
    for y in 0..area.height {
        csi_move!(buf, area.left(), area.top() + y)?;
        for x in 0..area.width {
            write!(
                buf,
                "{DELIM}{row}{col}",
                row = GRID[y as usize],
                col = GRID[x as usize]
            )?;
        }
    }
    execute!(buf, crossterm::style::ResetColor, crossterm::cursor::RestorePosition)?;
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

/// Free one wall image (its data and all its placements).
pub fn delete_wall_tile(w: &mut impl Write, id: u32) -> Result<()> {
    tmux_write!(w, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\", id = id)?;
    Ok(())
}

const DELIM: &str = "\u{10EEEE}";
const GRID: &[&str] = &[
    "\u{0305}",
    "\u{030D}",
    "\u{030E}",
    "\u{0310}",
    "\u{0312}",
    "\u{033D}",
    "\u{033E}",
    "\u{033F}",
    "\u{0346}",
    "\u{034A}",
    "\u{034B}",
    "\u{034C}",
    "\u{0350}",
    "\u{0351}",
    "\u{0352}",
    "\u{0357}",
    "\u{035B}",
    "\u{0363}",
    "\u{0364}",
    "\u{0365}",
    "\u{0366}",
    "\u{0367}",
    "\u{0368}",
    "\u{0369}",
    "\u{036A}",
    "\u{036B}",
    "\u{036C}",
    "\u{036D}",
    "\u{036E}",
    "\u{036F}",
    "\u{0483}",
    "\u{0484}",
    "\u{0485}",
    "\u{0486}",
    "\u{0487}",
    "\u{0592}",
    "\u{0593}",
    "\u{0594}",
    "\u{0595}",
    "\u{0597}",
    "\u{0598}",
    "\u{0599}",
    "\u{059C}",
    "\u{059D}",
    "\u{059E}",
    "\u{059F}",
    "\u{05A0}",
    "\u{05A1}",
    "\u{05A8}",
    "\u{05A9}",
    "\u{05AB}",
    "\u{05AC}",
    "\u{05AF}",
    "\u{05C4}",
    "\u{0610}",
    "\u{0611}",
    "\u{0612}",
    "\u{0613}",
    "\u{0614}",
    "\u{0615}",
    "\u{0616}",
    "\u{0617}",
    "\u{0657}",
    "\u{0658}",
    "\u{0659}",
    "\u{065A}",
    "\u{065B}",
    "\u{065D}",
    "\u{065E}",
    "\u{06D6}",
    "\u{06D7}",
    "\u{06D8}",
    "\u{06D9}",
    "\u{06DA}",
    "\u{06DB}",
    "\u{06DC}",
    "\u{06DF}",
    "\u{06E0}",
    "\u{06E1}",
    "\u{06E2}",
    "\u{06E4}",
    "\u{06E7}",
    "\u{06E8}",
    "\u{06EB}",
    "\u{06EC}",
    "\u{0730}",
    "\u{0732}",
    "\u{0733}",
    "\u{0735}",
    "\u{0736}",
    "\u{073A}",
    "\u{073D}",
    "\u{073F}",
    "\u{0740}",
    "\u{0741}",
    "\u{0743}",
    "\u{0745}",
    "\u{0747}",
    "\u{0749}",
    "\u{074A}",
    "\u{07EB}",
    "\u{07EC}",
    "\u{07ED}",
    "\u{07EE}",
    "\u{07EF}",
    "\u{07F0}",
    "\u{07F1}",
    "\u{07F3}",
    "\u{0816}",
    "\u{0817}",
    "\u{0818}",
    "\u{0819}",
    "\u{081B}",
    "\u{081C}",
    "\u{081D}",
    "\u{081E}",
    "\u{081F}",
    "\u{0820}",
    "\u{0821}",
    "\u{0822}",
    "\u{0823}",
    "\u{0825}",
    "\u{0826}",
    "\u{0827}",
    "\u{0829}",
    "\u{082A}",
    "\u{082B}",
    "\u{082C}",
    "\u{082D}",
    "\u{0951}",
    "\u{0953}",
    "\u{0954}",
    "\u{0F82}",
    "\u{0F83}",
    "\u{0F86}",
    "\u{0F87}",
    "\u{135D}",
    "\u{135E}",
    "\u{135F}",
    "\u{17DD}",
    "\u{193A}",
    "\u{1A17}",
    "\u{1A75}",
    "\u{1A76}",
    "\u{1A77}",
    "\u{1A78}",
    "\u{1A79}",
    "\u{1A7A}",
    "\u{1A7B}",
    "\u{1A7C}",
    "\u{1B6B}",
    "\u{1B6D}",
    "\u{1B6E}",
    "\u{1B6F}",
    "\u{1B70}",
    "\u{1B71}",
    "\u{1B72}",
    "\u{1B73}",
    "\u{1CD0}",
    "\u{1CD1}",
    "\u{1CD2}",
    "\u{1CDA}",
    "\u{1CDB}",
    "\u{1CE0}",
    "\u{1DC0}",
    "\u{1DC1}",
    "\u{1DC3}",
    "\u{1DC4}",
    "\u{1DC5}",
    "\u{1DC6}",
    "\u{1DC7}",
    "\u{1DC8}",
    "\u{1DC9}",
    "\u{1DCB}",
    "\u{1DCC}",
    "\u{1DD1}",
    "\u{1DD2}",
    "\u{1DD3}",
    "\u{1DD4}",
    "\u{1DD5}",
    "\u{1DD6}",
    "\u{1DD7}",
    "\u{1DD8}",
    "\u{1DD9}",
    "\u{1DDA}",
    "\u{1DDB}",
    "\u{1DDC}",
    "\u{1DDD}",
    "\u{1DDE}",
    "\u{1DDF}",
    "\u{1DE0}",
    "\u{1DE1}",
    "\u{1DE2}",
    "\u{1DE3}",
    "\u{1DE4}",
    "\u{1DE5}",
    "\u{1DE6}",
    "\u{1DFE}",
    "\u{20D0}",
    "\u{20D1}",
    "\u{20D4}",
    "\u{20D5}",
    "\u{20D6}",
    "\u{20D7}",
    "\u{20DB}",
    "\u{20DC}",
    "\u{20E1}",
    "\u{20E7}",
    "\u{20E9}",
    "\u{20F0}",
    "\u{2CEF}",
    "\u{2CF0}",
    "\u{2CF1}",
    "\u{2DE0}",
    "\u{2DE1}",
    "\u{2DE2}",
    "\u{2DE3}",
    "\u{2DE4}",
    "\u{2DE5}",
    "\u{2DE6}",
    "\u{2DE7}",
    "\u{2DE8}",
    "\u{2DE9}",
    "\u{2DEA}",
    "\u{2DEB}",
    "\u{2DEC}",
    "\u{2DED}",
    "\u{2DEE}",
    "\u{2DEF}",
    "\u{2DF0}",
    "\u{2DF1}",
    "\u{2DF2}",
    "\u{2DF3}",
    "\u{2DF4}",
    "\u{2DF5}",
    "\u{2DF6}",
    "\u{2DF7}",
    "\u{2DF8}",
    "\u{2DF9}",
    "\u{2DFA}",
    "\u{2DFB}",
    "\u{2DFC}",
    "\u{2DFD}",
    "\u{2DFE}",
    "\u{2DFF}",
    "\u{A66F}",
    "\u{A67C}",
    "\u{A67D}",
    "\u{A6F0}",
    "\u{A6F1}",
    "\u{A8E0}",
    "\u{A8E1}",
    "\u{A8E2}",
    "\u{A8E3}",
    "\u{A8E4}",
    "\u{A8E5}",
    "\u{A8E6}",
    "\u{A8E7}",
    "\u{A8E8}",
    "\u{A8E9}",
    "\u{A8EA}",
    "\u{A8EB}",
    "\u{A8EC}",
    "\u{A8ED}",
    "\u{A8EE}",
    "\u{A8EF}",
    "\u{A8F0}",
    "\u{A8F1}",
    "\u{AAB0}",
    "\u{AAB2}",
    "\u{AAB3}",
    "\u{AAB7}",
    "\u{AAB8}",
    "\u{AABE}",
    "\u{AABF}",
    "\u{AAC1}",
    "\u{FE20}",
    "\u{FE21}",
    "\u{FE22}",
    "\u{FE23}",
    "\u{FE24}",
    "\u{FE25}",
    "\u{FE26}",
    "\u{10A0F}",
    "\u{10A38}",
    "\u{1D185}",
    "\u{1D186}",
    "\u{1D187}",
    "\u{1D188}",
    "\u{1D189}",
    "\u{1D1AA}",
    "\u{1D1AB}",
    "\u{1D1AC}",
    "\u{1D1AD}",
    "\u{1D242}",
    "\u{1D243}",
    "\u{1D244}",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Incompressible (high-entropy) data stands in for photographic cover
    /// art: zlib cannot shrink it, and its framing overhead makes the
    /// "compressed" result *larger* than the input. Round 111's report saw
    /// exactly this shape of result. The encoder must notice and fall back
    /// to sending the bytes raw.
    #[test]
    fn incompressible_data_is_sent_uncompressed() {
        // A cheap deterministic PRNG: high-entropy, no dependency needed.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let raw: Vec<u8> = (0..64 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 0xFF) as u8
            })
            .collect();

        let payload = encode_image_payload(&raw);

        assert!(
            !payload.compressed,
            "zlib cannot shrink high-entropy data, so the raw form must be \
             sent instead of a larger 'compressed' one"
        );
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&payload.content)
            .expect("payload is valid base64");
        assert!(
            decoded == raw,
            "the fallback sends the original bytes intact (got {} bytes, want {})",
            decoded.len(),
            raw.len()
        );
    }

    /// The other half: when zlib genuinely wins (long runs of one colour,
    /// the common case for flat art / UI-ish images) it must still be used,
    /// and the `compressed` flag must agree with the bytes.
    #[test]
    fn compressible_data_is_still_compressed() {
        let raw = vec![0u8; 64 * 1024];

        let payload = encode_image_payload(&raw);

        assert!(payload.compressed, "a buffer of one repeated byte compresses well");
        assert!(
            payload.encoded_len < raw.len(),
            "the compressed form is genuinely smaller: {} vs {} raw bytes",
            payload.encoded_len,
            raw.len()
        );
        // The payload is base64(zlib(raw)), so it only equals `raw` after
        // the terminal side's inflate step.
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&payload.content)
            .expect("payload is valid base64");
        assert_eq!(payload.encoded_len, decoded.len(), "encoded_len counts pre-base64 bytes");
        let mut inflater = flate2::read::ZlibDecoder::new(decoded.as_slice());
        let mut round_tripped = Vec::new();
        std::io::Read::read_to_end(&mut inflater, &mut round_tripped)
            .expect("a compressed payload must inflate");
        assert!(
            round_tripped == raw,
            "the compressed bytes inflate to the original (got {} bytes, want {})",
            round_tripped.len(),
            raw.len()
        );
        assert_eq!(payload.len(), payload.content.len(), "len() is the base64 length");
    }

    /// A payload must never be *larger* than the raw input once base64 is
    /// accounted for, because base64 scales both candidates by the same
    /// 4/3 — so the raw pre-base64 comparison is the right decision.
    #[test]
    fn payload_is_never_worse_than_sending_raw() {
        for len in [0usize, 1, 2, 3, 1000, 8191, 8192] {
            let raw = vec![0xABu8; len];
            let payload = encode_image_payload(&raw);
            assert!(
                payload.encoded_len <= raw.len(),
                "payload of {len} raw bytes grew to {} bytes",
                payload.encoded_len
            );
        }
    }

    /// An empty buffer is a degenerate case that must not panic and must
    /// round-trip to empty.
    #[test]
    fn empty_input_round_trips() {
        let payload = encode_image_payload(&[]);
        assert_eq!(payload.encoded_len, 0);
        assert!(payload.content.is_empty());
    }

    // --- transmit-once / place-many --------------------------------------

    /// An `ImageData` payload of `len` printable characters, under `id`.
    fn payload(id: u32, len: usize) -> ImageData {
        ImageData {
            image_id: id,
            content: "A".repeat(len),
            is_compressed: false,
            img_width: 64,
            img_height: 64,
            aligned_area: Rect::new(0, 0, 8, 4),
        }
    }

    fn out_of(write: impl FnOnce(&mut Vec<u8>)) -> String {
        let mut out = Vec::new();
        write(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The payload goes over the wire once. Every later draw of the same
    /// encode is a placement plus the placeholder cells — this is what makes
    /// re-placing after a repaint or a resize cheap.
    #[test]
    fn a_payload_is_transmitted_once_per_id() {
        let mut kitty = Kitty::new();
        let data = payload(9001, 64);

        let first = out_of(|w| kitty.transmit_once(w, &data).expect("first transmit"));
        assert!(first.contains("i=9001"), "{first}");
        assert!(first.contains("a=t,"), "transmit must not display: {first}");
        assert!(!first.contains("a=T"), "{first}");

        let second = out_of(|w| kitty.transmit_once(w, &data).expect("second transmit"));
        assert!(second.is_empty(), "a re-place must not retransmit: {second}");

        let placed = out_of(|w| place_image(w, 9001, Rect::new(2, 3, 8, 4)).expect("place"));
        assert!(placed.contains("a=p,U=1,i=9001"), "{placed}");
        assert!(placed.contains("c=8,r=4"), "{placed}");
        // A payload command ends `q=2;<base64>`; a place command does not.
        assert!(!placed.contains("q=2;"), "a place carries no payload: {placed}");
    }

    /// The chunk marker must tell the truth: a payload that fits in one chunk
    /// may not claim a follow-up, or the terminal waits for a chunk that never
    /// comes.
    #[test]
    fn a_short_payload_does_not_claim_a_following_chunk() {
        let mut kitty = Kitty::new();
        let data = payload(7001, 64);
        let text = out_of(|w| kitty.transmit_once(w, &data).expect("transmit"));
        assert!(text.contains("m=0"), "{text}");
    }

    #[test]
    fn a_long_payload_announces_following_chunks() {
        let mut kitty = Kitty::new();
        // Two full 4096-char chunks plus a remainder.
        let data = payload(7002, 9000);
        let text = out_of(|w| kitty.transmit_once(w, &data).expect("transmit"));
        assert!(text.contains("m=1"), "{text}");
        // The continuation chunks name the same image id.
        assert!(text.matches("i=7002").count() >= 3, "{text}");
    }

    /// Hiding this overlay frees its own placement only. The old `d=A` took
    /// every image in the terminal with it — including the Jellyfin poster,
    /// which then had to be re-placed by hand after every clear.
    #[test]
    fn hide_frees_only_its_own_image() {
        let mut kitty = Kitty::new();
        kitty.current = Some(4242);
        kitty.transmitted.push(4242);
        let text = out_of(|w| kitty.hide(w, Rect::new(1, 1, 4, 2), None).expect("hide"));
        assert!(text.contains("a=d,d=I,i=4242"), "{text}");
        assert!(!text.contains("d=A"), "hide must not nuke the image store: {text}");
        assert_eq!(kitty.current, None, "the overlay is no longer showing");
        assert!(
            kitty.transmitted.is_empty(),
            "a freed image is not claimed as transmitted"
        );
        // Coming back re-transmits once, then places for free.
        let data = payload(4242, 32);
        let again = out_of(|w| kitty.transmit_once(w, &data).expect("re-transmit"));
        assert!(again.contains("a=t,"), "a re-shown overlay transfers once: {again}");
    }

    #[test]
    fn hide_with_nothing_displayed_deletes_nothing() {
        let mut kitty = Kitty::new();
        let text = out_of(|w| kitty.hide(w, Rect::new(0, 0, 2, 2), None).expect("hide"));
        assert!(!text.contains("a=d"), "nothing to free: {text}");
    }

    /// The payload cache is bounded: the terminal keeps at most
    /// `TRANSMIT_CACHE` art images, and a new one evicts the oldest.
    #[test]
    fn the_payload_cache_evicts_the_oldest_image() {
        let mut kitty = Kitty::new();
        for n in 0..=TRANSMIT_CACHE {
            let id = 5000 + n as u32;
            let data = payload(id, 32);
            let _ = out_of(|w| kitty.transmit_once(w, &data).expect("transmit"));
        }
        assert_eq!(kitty.transmitted.len(), TRANSMIT_CACHE, "cache is bounded");
        let evicted = 5000;
        assert!(
            !kitty.transmitted.contains(&evicted),
            "the oldest payload was freed"
        );
        let text = out_of(|w| delete_image(w, evicted).expect("delete"));
        assert!(text.contains("a=d,d=I,i=5000"), "{text}");
    }

    /// The image currently on screen is never evicted, even when the cache is
    /// over its cap: freeing the placement the user is looking at would blank
    /// the pane.
    #[test]
    fn the_visible_image_survives_eviction() {
        let mut kitty = Kitty::new();
        let first = payload(6000, 32);
        let _ = out_of(|w| kitty.transmit_once(w, &first).expect("transmit"));
        kitty.current = Some(6000);
        for n in 1..=TRANSMIT_CACHE {
            let data = payload(6000 + n as u32, 32);
            let _ = out_of(|w| kitty.transmit_once(w, &data).expect("transmit"));
        }
        assert!(
            kitty.transmitted.contains(&6000),
            "the displayed image is still transmitted"
        );
    }

    // --- the reveal -------------------------------------------------------

    /// The wipe is linear and clips the picture: the fraction is how much of
    /// the image's width is visible, and cell (row, col) always maps to the
    /// same pixels.
    #[test]
    fn reveal_columns_is_a_clip_of_the_width() {
        assert_eq!(reveal_columns(80, None), 80, "no reveal = everything");
        assert_eq!(reveal_columns(80, Some(0.0)), 0);
        assert_eq!(reveal_columns(80, Some(0.5)), 40);
        assert_eq!(reveal_columns(80, Some(1.0)), 80);
        assert_eq!(reveal_columns(80, Some(2.0)), 80, "clamped");
        assert_eq!(reveal_columns(80, Some(-1.0)), 0, "clamped");
        assert_eq!(reveal_columns(80, Some(f32::NAN)), 80, "NaN shows the art");
        assert_eq!(reveal_columns(7, Some(0.5)), 4, "rounds to the nearest column");
        assert_eq!(reveal_columns(0, Some(0.5)), 0);
    }

    fn placeholder_runes(text: &str) -> usize {
        text.matches('\u{10EEEE}').count()
    }

    #[test]
    fn the_grid_paints_only_the_columns_it_is_given() {
        let area = Rect::new(2, 1, 8, 4);
        let full = out_of(|w| {
            create_unicode_placeholder_grid(w, None, area, 77, 8).expect("grid")
        });
        assert_eq!(placeholder_runes(&full), 32, "8x4 cells");
        assert!(
            full.contains("38;2;0;0;77"),
            "the placeholder carries the id as a 24-bit foreground: {full}"
        );

        let half = out_of(|w| {
            create_unicode_placeholder_grid(w, None, area, 77, 4).expect("grid")
        });
        assert_eq!(placeholder_runes(&half), 16, "4 columns x 4 rows");

        let none = out_of(|w| {
            create_unicode_placeholder_grid(w, None, area, 77, 0).expect("grid")
        });
        assert_eq!(placeholder_runes(&none), 0, "nothing is painted before the reveal starts");
        assert!(none.is_empty(), "and nothing is written: {none}");
    }

    fn test_ctx() -> Ctx {
        crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        )
    }

    /// A reveal draws once per frame: the first frame pays for the transfer,
    /// every later one is a placement plus a growing set of placeholder cells.
    #[test]
    fn a_re_place_skips_the_transfer_and_follows_the_reveal() {
        let ctx = test_ctx();
        let mut kitty = Kitty::new();
        let data = payload(9100, 64);
        let area = Rect::new(0, 0, 8, 4);

        let first = out_of(|w| {
            kitty.replace(w, area, None, Data::ImageData(data.clone()), &ctx, None)
                .expect("first draw")
        });
        assert!(first.contains("a=t,"), "the first draw transmits: {first}");
        assert_eq!(placeholder_runes(&first), 32, "and paints every cell");

        let half = out_of(|w| {
            kitty.replace(w, area, None, Data::ImageData(data.clone()), &ctx, Some(0.5))
                .expect("reveal frame")
        });
        assert!(!half.contains("a=t,"), "a re-place must not retransmit: {half}");
        assert_eq!(placeholder_runes(&half), 16, "half the columns");

        let done = out_of(|w| {
            kitty.replace(w, area, None, Data::ImageData(data), &ctx, None)
                .expect("final frame")
        });
        assert!(!done.contains("a=t,"), "{done}");
        assert_eq!(placeholder_runes(&done), 32, "the wipe finishes on the whole grid");
    }

    /// Same image, new box (a resize): the payload is kept, only the old cells
    /// are blanked.
    #[test]
    fn a_same_payload_at_a_new_size_keeps_its_image() {
        let ctx = test_ctx();
        let mut kitty = Kitty::new();
        let first = payload(9200, 64);
        let _ = out_of(|w| {
            kitty
                .replace(w, Rect::new(0, 0, 8, 4), None, Data::ImageData(first), &ctx, None)
                .expect("draw")
        });

        let mut moved = payload(9200, 64);
        moved.aligned_area = Rect::new(0, 2, 6, 3);
        let text = out_of(|w| {
            kitty
                .replace(w, Rect::new(0, 0, 8, 4), None, Data::ImageData(moved), &ctx, None)
                .expect("move")
        });
        assert!(!text.contains("a=t,"), "no transfer: {text}");
        assert!(!text.contains("d=I"), "and the payload is not freed: {text}");
    }

    /// A different picture retires the old payload and transmits the new one.
    #[test]
    fn a_new_payload_frees_the_old_image_and_transmits() {
        let ctx = test_ctx();
        let mut kitty = Kitty::new();
        let _ = out_of(|w| {
            kitty
                .replace(w, Rect::new(0, 0, 8, 4), None, Data::ImageData(payload(9300, 32)), &ctx, None)
                .expect("draw")
        });
        let text = out_of(|w| {
            kitty
                .replace(w, Rect::new(0, 0, 8, 4), None, Data::ImageData(payload(9301, 32)), &ctx, None)
                .expect("swap")
        });
        assert!(text.contains("a=d,d=I,i=9300"), "the old image is freed: {text}");
        assert!(text.contains("a=t,"), "the new one is transmitted: {text}");
        assert!(text.contains("i=9301"), "{text}");
    }

    /// The placeholder id is a 24-bit value in the cell's foreground colour;
    /// ids the 8-bit form could not express must round-trip.
    #[test]
    fn the_placeholder_foreground_carries_the_image_id_in_24_bits() {
        // Small ids (the wall's) keep working...
        assert_eq!(placeholder_foreground(7), "\x1b[38;2;0;0;7m");
        // ...and the art allocator's ids are expressible, byte by byte.
        assert_eq!(placeholder_foreground(65537), "\x1b[38;2;1;0;1m");
        assert_eq!(placeholder_foreground(0x0A_0B_0C), "\x1b[38;2;10;11;12m");
    }

    /// The allocator never leaves the 24-bit placeholder space, and never
    /// hands back an id the Home wall owns (2..).
    #[test]
    fn image_ids_stay_inside_the_placeholder_space() {
        let mut previous = 0;
        for _ in 0..1000 {
            let id = next_image_id();
            assert!(id >= IMAGE_ID_BASE, "id {id} collides with the wall's block");
            assert!(id < IMAGE_ID_MAX, "id {id} cannot be a placeholder colour");
            assert_ne!(id, previous, "ids are not reused back to back");
            previous = id;
        }
    }
}
