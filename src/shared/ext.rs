pub mod span {
    use std::borrow::Cow;
    use ratatui::text::Span;
    use unicode_segmentation::UnicodeSegmentation;
    pub trait SpanExt {
        /// Truncate the end of the span's content to the specified number of
        /// characters. Returns how many characters were consumed form
        /// the specified remaining length.
        fn truncate_end(&mut self, remaining_length: usize) -> usize;
        /// Truncate the start of the span's content to the specified number of
        /// characters. Returns how many characters were consumed form
        /// the specified remaining length.
        fn truncate_start(&mut self, remaining_length: usize) -> usize;
    }
    impl SpanExt for String {
        fn truncate_end(&mut self, remaining_length: usize) -> usize {
            if remaining_length == 0 {
                self.clear();
                return 0;
            }
            if let Some((idx, s)) = self
                .grapheme_indices(true)
                .nth(remaining_length.saturating_sub(1))
            {
                self.drain(idx + s.len()..);
            }
            remaining_length
        }
        fn truncate_start(&mut self, remaining_length: usize) -> usize {
            if remaining_length == 0 {
                self.clear();
                return 0;
            }
            if let Some((idx, _)) = self
                .grapheme_indices(true)
                .rev()
                .nth(remaining_length.saturating_sub(1))
            {
                self.drain(0..idx);
            }
            remaining_length
        }
    }
    impl SpanExt for Span<'_> {
        fn truncate_end(&mut self, remaining_length: usize) -> usize {
            let chars = self.content.graphemes(true).count();
            if chars <= remaining_length {
                return chars;
            }
            if remaining_length == 0 {
                self.content = Cow::Borrowed("");
                return 0;
            }
            match &mut self.content {
                Cow::Borrowed(content) => {
                    let mut strbuf = String::new();
                    for (i, c) in content.graphemes(true).enumerate() {
                        if i >= remaining_length {
                            break;
                        }
                        strbuf.push_str(c);
                    }
                    self.content = strbuf.into();
                    remaining_length
                }
                cow @ Cow::Owned(_) => {
                    cow.to_mut().truncate_end(remaining_length);
                    remaining_length
                }
            }
        }
        fn truncate_start(&mut self, remaining_length: usize) -> usize {
            let chars = self.content.graphemes(true).count();
            if chars <= remaining_length {
                return chars;
            }
            if remaining_length == 0 {
                self.content = Cow::Borrowed("");
                return 0;
            }
            match &mut self.content {
                Cow::Borrowed(content) => {
                    let mut strbuf = String::new();
                    for (i, c) in content.graphemes(true).rev().enumerate() {
                        if i >= remaining_length {
                            break;
                        }
                        strbuf.insert_str(0, c);
                    }
                    self.content = strbuf.into();
                    remaining_length
                }
                cow @ Cow::Owned(_) => {
                    cow.to_mut().truncate_start(remaining_length);
                    remaining_length
                }
            }
        }
    }
}
pub mod error {
    use itertools::Itertools;
    use crate::mpd::errors::MpdError;
    pub trait ErrorExt {
        fn to_status(&self) -> String;
    }
    impl ErrorExt for anyhow::Error {
        fn to_status(&self) -> String {
            self.chain().map(|e| e.to_string().replace('\n', "")).join(" ")
        }
    }
    impl ErrorExt for MpdError {
        fn to_status(&self) -> String {
            match self {
                MpdError::Parse(e) => format!("Failed to parse: {e}"),
                MpdError::UnknownCode(e) => format!("Unknown code: {e}"),
                MpdError::Generic(e) => format!("Generic error: {e}"),
                MpdError::ClientClosed => "Client closed".to_string(),
                MpdError::Mpd(e) => format!("MPD Error: {e}"),
                MpdError::ValueExpected(e) => format!("Expected Value but got '{e}'"),
                MpdError::UnsupportedMpdVersion(e) => {
                    format!("Unsupported MPD version: {e}")
                }
                MpdError::TimedOut(_) => "Request to MPD timed out".to_string(),
            }
        }
    }
}
pub mod duration {
    const SECONDS_IN_DAY: u64 = 60 * 60 * 24;
    const SECONDS_IN_HOUR: u64 = 60 * 60;
    const SECONDS_IN_MINUTE: u64 = 60;
    pub trait DurationExt {
        fn to_string(&self) -> String;
        fn format_to_duration(&self, unit_separator: &str) -> String;
    }
    impl DurationExt for std::time::Duration {
        fn to_string(&self) -> String {
            let secs = self.as_secs();
            let min = secs / 60;
            let frac_secs = secs - min * 60;
            let hours = min / 60;
            let frac_min = min - hours * 60;
            let days = hours / 24;
            let frac_hours = hours - days * 24;
            if hours == 0 {
                format!("{min}:{frac_secs:0>2}")
            } else if days == 0 {
                format!("{hours}:{frac_min:0>2}:{frac_secs:0>2}")
            } else {
                format!("{days}d {frac_hours:0>2}:{frac_min:0>2}:{frac_secs:0>2}")
            }
        }
        fn format_to_duration(&self, unit_separator: &str) -> String {
            let mut total_seconds = self.as_secs();
            if total_seconds == 0 {
                return "0s".to_string();
            }
            let mut buf = String::new();
            if total_seconds >= SECONDS_IN_DAY {
                let days = total_seconds / SECONDS_IN_DAY;
                total_seconds = total_seconds.saturating_sub(days * SECONDS_IN_DAY);
                buf.push_str(&days.to_string());
                buf.push('d');
                if total_seconds > 0 {
                    buf.push_str(unit_separator);
                }
            }
            if total_seconds >= SECONDS_IN_HOUR {
                let hours = total_seconds / SECONDS_IN_HOUR;
                total_seconds = total_seconds.saturating_sub(hours * SECONDS_IN_HOUR);
                buf.push_str(&hours.to_string());
                buf.push('h');
                if total_seconds > 0 {
                    buf.push_str(unit_separator);
                }
            }
            if total_seconds >= SECONDS_IN_MINUTE {
                let minutes = total_seconds / SECONDS_IN_MINUTE;
                total_seconds = total_seconds
                    .saturating_sub(minutes * SECONDS_IN_MINUTE);
                buf.push_str(&minutes.to_string());
                buf.push('m');
                if total_seconds > 0 {
                    buf.push_str(unit_separator);
                }
            }
            if total_seconds > 0 {
                buf.push_str(&total_seconds.to_string());
                buf.push('s');
            }
            buf
        }
    }
}
#[allow(unused)]
pub mod mpsc {
    use crossbeam::channel::{Receiver, RecvError, TryRecvError};
    pub trait RecvLast<T> {
        fn recv_last(&self) -> Result<T, RecvError>;
        fn try_recv_last(&self) -> Result<T, TryRecvError>;
    }
    impl<T> RecvLast<T> for Receiver<T> {
        /// recv the last message in the channel and drop all the other ones
        fn recv_last(&self) -> Result<T, RecvError> {
            self.recv()
                .map(|data| {
                    let mut result = data;
                    while let Ok(newer_data) = self.try_recv() {
                        result = newer_data;
                    }
                    result
                })
        }
        /// recv the last message in the channel in a non-blocking manner and
        /// drop all the other ones
        fn try_recv_last(&self) -> Result<T, TryRecvError> {
            self.try_recv()
                .map(|data| {
                    let mut result = data;
                    while let Ok(newer_data) = self.try_recv() {
                        result = newer_data;
                    }
                    result
                })
        }
    }
}
pub mod btreeset_ranges {
    use std::{
        collections::{BTreeSet, btree_set},
        ops::{Range, RangeInclusive},
    };
    pub trait BTreeSetRanges<'a, T: 'a> {
        fn ranges(&'a self) -> Ranges<'a, T, std::collections::btree_set::Iter<'a, T>>;
    }
    pub struct Ranges<'a, T: 'a, I: Iterator<Item = &'a T>> {
        iter: I,
        current_range: Option<Range<T>>,
    }
    impl<'a, T: Default + 'a> BTreeSetRanges<'a, T> for BTreeSet<T> {
        fn ranges(&'a self) -> Ranges<'a, T, btree_set::Iter<'a, T>> {
            Ranges {
                iter: self.iter(),
                current_range: None,
            }
        }
    }
    impl<'a, I: DoubleEndedIterator<Item = &'a usize>> DoubleEndedIterator
    for Ranges<'a, usize, I> {
        fn next_back(&mut self) -> Option<Self::Item> {
            match (self.iter.next_back(), self.current_range.take()) {
                (Some(current), None) => {
                    self.current_range = Some(*current..*current);
                    self.next_back()
                }
                (None, Some(current_range)) => {
                    self.current_range = None;
                    Some(current_range.start..=current_range.end)
                }
                (
                    Some(current),
                    Some(mut current_range),
                ) if *current == current_range.start - 1 => {
                    current_range.start = *current;
                    self.current_range = Some(current_range);
                    self.next_back()
                }
                (Some(current), Some(current_range)) => {
                    self.current_range = Some(*current..*current);
                    Some(current_range.start..=current_range.end)
                }
                (None, None) => None,
            }
        }
    }
    impl<'a, I: Iterator<Item = &'a usize>> Iterator for Ranges<'a, usize, I> {
        type Item = RangeInclusive<usize>;
        fn next(&mut self) -> Option<Self::Item> {
            match (self.iter.next(), self.current_range.take()) {
                (Some(current), None) => {
                    self.current_range = Some(*current..*current);
                    self.next()
                }
                (None, Some(current_range)) => {
                    self.current_range = None;
                    Some(current_range.start..=current_range.end)
                }
                (
                    Some(current),
                    Some(mut current_range),
                ) if *current == current_range.end + 1 => {
                    current_range.end = *current;
                    self.current_range = Some(current_range);
                    self.next()
                }
                (Some(current), Some(current_range)) => {
                    self.current_range = Some(*current..*current);
                    Some(current_range.start..=current_range.end)
                }
                (None, None) => None,
            }
        }
    }
}
pub mod rect {
    use ratatui::layout::Rect;
    #[allow(unused)]
    pub trait RectExt {
        fn shrink_from_top(self, amount: u16) -> Rect;
        fn shrink_horizontally(self, amount: u16) -> Rect;
        fn overlaps_in_y(&self, other: &Self) -> bool;
        fn overlaps_in_x(&self, other: &Self) -> bool;
    }
    impl RectExt for Rect {
        fn shrink_from_top(mut self, amount: u16) -> Rect {
            self.height = self.height.saturating_sub(amount);
            self.y = self.y.saturating_add(amount);
            self
        }
        fn shrink_horizontally(mut self, amount: u16) -> Rect {
            self.width = self.width.saturating_sub(amount * 2);
            self.x = self.x.saturating_add(amount);
            self
        }
        fn overlaps_in_y(&self, other: &Self) -> bool {
            !(self.bottom() <= other.top() || self.top() >= other.bottom())
        }
        fn overlaps_in_x(&self, other: &Self) -> bool {
            !(self.right() <= other.left() || self.left() >= other.right())
        }
    }
}
pub mod vec {
    pub trait VecExt<T> {
        fn or_else_if_empty(self, cb: impl Fn() -> Vec<T>) -> Vec<T>;
        fn get_or_last(&self, idx: usize) -> Option<&T>;
    }
    impl<T> VecExt<T> for Vec<T> {
        fn or_else_if_empty(self, cb: impl Fn() -> Vec<T>) -> Vec<T> {
            if self.is_empty() { cb() } else { self }
        }
        fn get_or_last(&self, idx: usize) -> Option<&T> {
            self.get(idx).or_else(|| self.last())
        }
    }
}
pub mod num {
    pub trait NumExt {
        fn with_thousands_separator(self, separator: &str) -> String;
    }
    impl NumExt for usize {
        fn with_thousands_separator(self, separator: &str) -> String {
            let mut buf = String::new();
            for (idx, c) in self.to_string().chars().rev().enumerate() {
                if idx % 3 == 0 && idx != 0 {
                    buf.insert_str(0, separator);
                }
                buf.insert(0, c);
            }
            buf
        }
    }
}
