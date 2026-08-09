use std::{cmp::Ordering, collections::HashMap};

use anyhow::Result;
use itertools::Itertools;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Flex, Layout, Rect},
    text::Line,
    widgets::{Row, Table},
};

use crate::{
    config::theme::properties::{Property, PropertyKindOrText, SongProperty},
    ctx::Ctx,
    mpd::{commands::Song, mpd_client::MpdCommand, proto_client::ProtoClient},
    shared::{
        cmp::StringCompare,
        keys::ActionEvent,
        mouse_event::{MouseEvent, MouseEventKind},
    },
    ui::{
        UiEvent,
        panes::{Pane, queue::QueuePane},
    },
};

#[derive(Debug)]
pub struct QueueHeaderPane {
    area: Rect,
    column_widths: Vec<Constraint>,
    column_formats: Vec<Property<SongProperty>>,
    song: Song,
    /// Album column sort mode, cycled on each click of the header:
    /// 0 = album track order, 1 = tracks a-z, 2 = tracks z-a.
    album_sort_mode: u8,
}

impl QueueHeaderPane {
    pub fn new(ctx: &Ctx) -> Self {
        let (column_widths, column_formats) = QueuePane::init(ctx);
        Self {
            area: Rect::default(),
            column_widths,
            column_formats,
            song: Song::default(),
            album_sort_mode: 0,
        }
    }

    /// Whether the column at `idx` is the Album column (gets the three-way
    /// sort cycle instead of the plain ascending sort).
    fn is_album_column(column_formats: &[Property<SongProperty>], idx: usize) -> bool {
        matches!(
            column_formats.get(idx).map(|v| &v.kind),
            Some(PropertyKindOrText::Property(SongProperty::Album))
        )
    }

    /// Sort key for the album cycle: album track order (disc + track number)
    /// or, for the a-z / z-a modes, the track title. z-a inverts the key so
    /// the ascending swap machinery produces descending order.
    fn album_sort_key(song: &Song, mode: u8) -> String {
        let album = song.metadata.get("album").map(|t| t.first()).unwrap_or_default();
        let title = song.metadata.get("title").map(|t| t.first()).unwrap_or_default();
        let disc: u32 =
            song.metadata.get("disc").and_then(|t| t.first().parse().ok()).unwrap_or(0);
        let track: u32 = song
            .metadata
            .get("track")
            .and_then(|t| t.first().split('/').next().and_then(|v| v.parse().ok()))
            .unwrap_or(0);
        let mut key = if mode == 0 {
            format!("{album}\u{0}{disc:02}\u{0}{track:03}\u{0}{title}")
        } else {
            format!("{album}\u{0}{title}")
        };
        if mode == 2 {
            key = key.bytes().map(|b| char::from(255 - b)).collect();
        }
        key
    }

    /// Sort the queue with the current album mode: group by album, tracks in
    /// disc/track order (mode 0) or title a-z / z-a (modes 1 / 2).
    fn sort_album(&self, ctx: &Ctx) -> Result<()> {
        let mode = self.album_sort_mode;
        let evald: Vec<(u32, String)> = ctx
            .queue
            .iter()
            .map(|song| (song.id, Self::album_sort_key(song, mode)))
            .collect();
        let swaps = Self::calculate_swaps_asc(evald, ctx)?;
        Self::apply_swaps(ctx, swaps);
        Ok(())
    }

    /// Always-ascending variant of `calculate_swaps` (no asc/desc toggle), so
    /// the album cycle controls the direction via its sort keys.
    fn calculate_swaps_asc<T: AsRef<str>>(
        mut desired: Vec<(u32, T)>,
        ctx: &Ctx,
    ) -> Result<Vec<(usize, usize)>> {
        let cmp = StringCompare::builder().fold_case(true).build();
        desired.sort_by(|(_, a), (_, b)| cmp.compare(a.as_ref(), b.as_ref()));

        let mut current: Vec<u32> = ctx.queue.iter().map(|s| s.id).collect();
        let mut index: HashMap<u32, usize> =
            current.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let mut swaps = Vec::new();

        for i in 0..current.len() {
            let target_id = desired[i].0;
            if current[i] == target_id {
                continue;
            }
            let j = *index
                .get(&target_id)
                .ok_or_else(|| anyhow::anyhow!("desired contains an ID not present in current"))?;
            swaps.push((i, j));
            let ai = current[i];
            current.swap(i, j);
            index.insert(ai, j);
            index.insert(target_id, i);
        }
        Ok(swaps)
    }

    /// Apply a list of (from, to) position swaps to the MPD queue.
    fn apply_swaps(ctx: &Ctx, swaps: Vec<(usize, usize)>) {
        ctx.command(move |client| {
            client.send_start_cmd_list()?;
            for swap in swaps {
                client.send_swap_position(swap.0, swap.1)?;
            }
            client.send_execute_cmd_list()?;
            client.read_ok()?;
            Ok(())
        });
    }

    fn calculate_swaps<T: AsRef<str>>(
        mut desired: Vec<(u32, T)>,
        ctx: &Ctx,
    ) -> Result<Vec<(usize, usize)>> {
        let cmp = StringCompare::builder().fold_case(true).build();
        let is_non_decreasing = desired.is_sorted_by(|(_, a), (_, b)| {
            matches!(cmp.compare(a.as_ref(), b.as_ref()), Ordering::Less | Ordering::Equal)
        });

        if is_non_decreasing {
            desired.sort_by(|(_, a), (_, b)| cmp.compare(a.as_ref(), b.as_ref()).reverse());
        } else {
            desired.sort_by(|(_, a), (_, b)| cmp.compare(a.as_ref(), b.as_ref()));
        }

        let mut current: Vec<u32> = ctx.queue.iter().map(|s| s.id).collect();
        let mut index: HashMap<u32, usize> =
            current.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let mut swaps = Vec::new();

        for i in 0..current.len() {
            let target_id = desired[i].0;
            if current[i] == target_id {
                continue; // already at the correct position
            }

            let j = *index
                .get(&target_id)
                .ok_or_else(|| anyhow::anyhow!("desired contains an ID not present in current"))?;

            swaps.push((i, j));

            let ai = current[i];
            current.swap(i, j);

            index.insert(ai, j);
            index.insert(target_id, i);
        }

        Ok(swaps)
    }

    pub fn sort_by_column(
        column_formats: &[Property<SongProperty>],
        idx: usize,
        ctx: &Ctx,
    ) -> Result<()> {
        let swaps = match column_formats.get(idx).as_ref().map(|v| &v.kind) {
            Some(PropertyKindOrText::Text(_)) => {
                // Do nothing, everything is a constant text
                Vec::new()
            }
            Some(PropertyKindOrText::Sticker(sticker_name)) => {
                let evald = ctx
                    .queue
                    .iter()
                    .map(|song| {
                        (
                            song.id,
                            ctx.song_stickers(&song.file)
                                .and_then(|s| s.get(sticker_name))
                                .map(|s| s.as_str())
                                .unwrap_or_default(),
                        )
                    })
                    .collect_vec();

                Self::calculate_swaps(evald, ctx)?
            }
            Some(PropertyKindOrText::Property(_))
            | Some(PropertyKindOrText::Group(_))
            | Some(PropertyKindOrText::Transform(_)) => {
                let evald = ctx
                    .queue
                    .iter()
                    .map(|song| {
                        (
                            song.id,
                            column_formats[idx]
                                .as_string(
                                    Some(song),
                                    "",
                                    ctx.config.theme.multiple_tag_resolution_strategy,
                                    ctx,
                                )
                                .unwrap_or_default(),
                        )
                    })
                    .collect_vec();

                Self::calculate_swaps(evald, ctx)?
            }
            None => {
                // Should not really ever happen. But no reason to handle this as a hard error.
                log::warn!("Tried to sort by non-existing column index {idx}");
                Vec::new()
            }
        };

        Self::apply_swaps(ctx, swaps);
        Ok(())
    }
}

impl Pane for QueueHeaderPane {
    fn render(&mut self, frame: &mut Frame, mut area: Rect, ctx: &Ctx) -> Result<()> {
        // The divider under the labels spans the box's full inner width
        // (the pane's own row plus the empty columns beside it).
        let divider_area = Rect {
            x: area.x.saturating_sub(1),
            y: area.y + 1,
            width: area.width + 1,
            height: 1,
        };
        // Reserve space for the queue scrollbar and padding on the right
        area.width = area.width.saturating_sub(2);
        self.area = area;
        // Hovering a clickable column label lightens it (lighter, less
        // saturated).
        let mouse = ctx.mouse_pos();

        // Chapters mode: the queue is replaced by the chapter list.
        let chapters_active = ctx.queue_tab.get() == crate::ctx::QueueTabMode::Chapters
            && ctx.has_current_chapters();
        // Video mode: the queue is replaced by the mpv playlist.
        let video_active = ctx.queue_tab.get() == crate::ctx::QueueTabMode::Video;
        if video_active {
            // Title (flexible) | Duration (right-aligned at the right edge,
            // like the queue's Duration column).
            let header_width = ctx.queue_table_width.get().unwrap_or(area.width);
            let header_area = Rect { x: area.x, y: area.y, width: header_width, height: 1 };
            let labels = [
                ("Title", Alignment::Left),
                ("Duration", Alignment::Right),
            ];
            let header: Vec<Line> = labels
                .iter()
                .enumerate()
                .map(|(idx, (label, align))| {
                    let mut line = Line::from(label.to_string()).alignment(*align);
                    let column = Layout::horizontal([
                        Constraint::Min(0),
                        Constraint::Length(crate::ui::panes::queue::CHAPTER_DURATION_COL),
                    ])
                    .split(header_area)[idx];
                    if mouse.is_some_and(|p| column.contains(p)) {
                        crate::config::hover_line(&mut line);
                    }
                    line
                })
                .collect();
            let header_table = Table::new(
                std::iter::once(Row::new(header)),
                [
                    Constraint::Min(0),
                    Constraint::Length(crate::ui::panes::queue::CHAPTER_DURATION_COL),
                ],
            );
            frame.render_widget(header_table, header_area);
        } else if chapters_active {
            // Render into the same width as the queue table (stashed by the
            // QueuePane's layout pass) so the labels sit exactly above the
            // chapter list values, using the chapters table's own columns
            // (Chapter | Time centered | Duration right-aligned).
            let header_width = ctx.queue_table_width.get().unwrap_or(area.width);
            let header_area = Rect { x: area.x, y: area.y, width: header_width, height: 1 };
            let labels = [
                ("Chapter", Alignment::Left),
                ("Time", Alignment::Center),
                ("Duration", Alignment::Right),
            ];
            let header: Vec<Line> = labels
                .iter()
                .enumerate()
                .map(|(idx, (label, align))| {
                    let mut line = Line::from(label.to_string()).alignment(*align);
                    let column = Layout::horizontal([
                        Constraint::Min(0),
                        Constraint::Length(crate::ui::panes::queue::CHAPTER_TIME_COL),
                        Constraint::Length(crate::ui::panes::queue::CHAPTER_DURATION_COL),
                    ])
                    .split(header_area)[idx];
                    if mouse.is_some_and(|p| column.contains(p)) {
                        crate::config::hover_line(&mut line);
                    }
                    line
                })
                .collect();
            let header_table = Table::new(
                std::iter::once(Row::new(header)),
                [
                    Constraint::Min(0),
                    Constraint::Length(crate::ui::panes::queue::CHAPTER_TIME_COL),
                    Constraint::Length(crate::ui::panes::queue::CHAPTER_DURATION_COL),
                ],
            );
            frame.render_widget(header_table, header_area);
        } else {
            let widths = Layout::horizontal(self.column_widths.as_slice())
                .flex(Flex::Start)
                .spacing(1)
                .split(self.area);

            let header = ctx
                .config
                .theme
                .song_table_format
                .iter()
                .enumerate()
                .map(|(idx, format)| {
                    let max_len: usize = widths[idx].width.into();
                    let mut line = self
                        .song
                        .as_line_ellipsized(
                            &format.label,
                            max_len,
                            &ctx.config.theme.symbols,
                            &ctx.config.theme.format_tag_separator,
                            ctx.config.theme.multiple_tag_resolution_strategy,
                            ctx,
                        )
                        .unwrap_or_default()
                        .alignment(format.alignment.into());
                    if mouse.is_some_and(|p| widths[idx].contains(p)) {
                        crate::config::hover_line(&mut line);
                    }
                    line
                })
                .collect_vec();
            let header_table = Table::new(std::iter::once(Row::new(header)), &self.column_widths);
            frame.render_widget(header_table, self.area);
        }

        // The divider row below the labels (the box's own border supplies
        // the │ at both ends).
        if area.height >= 2 && divider_area.width > 0 {
            let style = ctx.config.as_border_style();
            let buf = frame.buffer_mut();
            for x in divider_area.x..divider_area.right() {
                buf[(x, divider_area.y)].set_symbol("─").set_style(style);
            }
        }

        Ok(())
    }

    fn handle_action(&mut self, _event: &mut ActionEvent, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let position = event.into();

        if !self.area.contains(position) {
            return Ok(());
        }

        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                if self.area.contains(event.into()) {
                    let widths = Layout::horizontal(self.column_widths.as_slice())
                        .flex(Flex::Start)
                        .spacing(1)
                        .split(self.area);
                    if let Some(header_idx) = widths.iter().position(|w| w.contains(position)) {
                        if Self::is_album_column(self.column_formats.as_slice(), header_idx) {
                            // Cycle: track order -> tracks a-z -> tracks z-a.
                            self.album_sort_mode = (self.album_sort_mode + 1) % 3;
                            self.sort_album(ctx)?;
                        } else {
                            Self::sort_by_column(
                                self.column_formats.as_slice(),
                                header_idx,
                                ctx,
                            )?;
                        }
                    }
                    ctx.render()?;
                }
            }
            MouseEventKind::MiddleClick => {}
            MouseEventKind::RightClick => {}
            MouseEventKind::ScrollDown => {}
            MouseEventKind::ScrollUp => {}
            MouseEventKind::Drag { .. } => {}
            MouseEventKind::LeftRelease => {}
            MouseEventKind::Moved => {}
        }

        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, _is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::ConfigChanged => {
                let (column_widths, column_formats) = QueuePane::init(ctx);
                self.column_formats = column_formats;
                self.column_widths = column_widths;
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::mpd::commands::Song;

    fn song(id: u32, album: &str, title: &str, disc: &str, track: &str) -> Song {
        let mut metadata = HashMap::new();
        if !album.is_empty() {
            metadata.insert("album".into(), album.into());
        }
        if !title.is_empty() {
            metadata.insert("title".into(), title.into());
        }
        if !disc.is_empty() {
            metadata.insert("disc".into(), disc.into());
        }
        if !track.is_empty() {
            metadata.insert("track".into(), track.into());
        }
        Song { id, metadata, ..Default::default() }
    }

    #[test]
    fn track_order_sorts_numerically_within_album() {
        // Track 2 must sort before track 10 in album track order.
        let a = song(1, "Album", "T", "1", "2");
        let b = song(2, "Album", "T", "1", "10");
        let ka = QueueHeaderPane::album_sort_key(&a, 0);
        let kb = QueueHeaderPane::album_sort_key(&b, 0);
        assert!(ka < kb, "track 2 should sort before track 10: {ka:?} vs {kb:?}");
    }

    #[test]
    fn a_z_sorts_by_title_within_album() {
        let early = song(1, "Album", "Apple", "1", "2");
        let late = song(2, "Album", "Zebra", "1", "1");
        let k_early = QueueHeaderPane::album_sort_key(&early, 1);
        let k_late = QueueHeaderPane::album_sort_key(&late, 1);
        assert!(k_early < k_late, "titles should sort a-z: {k_early:?} vs {k_late:?}");
    }

    #[test]
    fn z_a_reverses_the_title_order() {
        let early = song(1, "Album", "Apple", "1", "2");
        let late = song(2, "Album", "Zebra", "1", "1");
        let k_early = QueueHeaderPane::album_sort_key(&early, 2);
        let k_late = QueueHeaderPane::album_sort_key(&late, 2);
        assert!(k_early > k_late, "titles should sort z-a: {k_early:?} vs {k_late:?}");
    }

    #[test]
    fn album_column_detection() {
        use crate::config::theme::properties::{Property, SongProperty};
        let album = Property::<SongProperty> {
            kind: PropertyKindOrText::Property(SongProperty::Album),
            style: None,
            default: None,
        };
        let title = Property::<SongProperty> {
            kind: PropertyKindOrText::Property(SongProperty::Title),
            style: None,
            default: None,
        };
        let formats = vec![album, title];
        assert!(QueueHeaderPane::is_album_column(&formats, 0));
        assert!(!QueueHeaderPane::is_album_column(&formats, 1));
    }
}
