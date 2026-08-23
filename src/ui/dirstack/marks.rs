use std::collections::BTreeSet;
/// Multi-selection state: the marked (multi-selected) rows, the anchor for
/// alt-click / shift+arrow range selection and the last range that was
/// marked. Shared by the panes that support the queue tab's selection
/// interactions (ctrl+click toggles, alt+click / shift+up/down mark a
/// range, a plain click clears the marks).
#[derive(Debug, Default, Clone)]
pub(crate) struct MarkState {
    marked: BTreeSet<usize>,
    anchor: Option<usize>,
    /// The range last marked by shift/alt selection, so contracting the
    /// range can unmark the rows the cursor moved past.
    range: Option<(usize, usize)>,
}
impl MarkState {
    pub fn is_empty(&self) -> bool {
        self.marked.is_empty()
    }
    pub fn contains(&self, idx: usize) -> bool {
        self.marked.contains(&idx)
    }
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.marked.iter().copied()
    }
    pub fn clear(&mut self) {
        self.marked.clear();
    }
    /// Drop the anchor and the last range (called when the selection is
    /// cleared, e.g. Esc): the next shift/alt range-select starts fresh
    /// from the cursor - a stale anchor must not survive.
    pub fn clear_anchor(&mut self) {
        self.anchor = None;
        self.range = None;
    }
    /// Drop every mark at or beyond `len` (the list shrank).
    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.marked.clear();
            return;
        }
        self.marked.retain(|&i| i < len);
    }
    /// Add `idx` to the marked set without ever removing it (the ctrl+click
    /// semantics: clicking more rows grows the selection, it never drops the
    /// rows that are already marked).
    pub fn add(&mut self, idx: usize) {
        self.marked.insert(idx);
    }
    /// Mark every index in `0..len` (the whole list, ctrl+a).
    pub fn mark_all(&mut self, len: usize) {
        if len == 0 {
            self.marked.clear();
            return;
        }
        self.mark_range(0, len - 1);
    }
    /// The anchor for alt-click / shift+arrow range selection; set by plain
    /// clicks (and the first shift/alt press).
    pub fn set_anchor(&mut self, idx: usize) {
        self.anchor = Some(idx);
    }
    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }
    /// The range last marked by shift/alt selection (if any).
    pub fn take_range(&mut self) -> Option<(usize, usize)> {
        self.range.take()
    }
    pub fn set_range(&mut self, lo: usize, hi: usize) {
        self.range = Some((lo, hi));
    }
    /// Forget the previous shift/alt range (called on plain clicks).
    pub fn clear_range(&mut self) {
        self.range = None;
    }
    /// Mark every index between `from` and `to` (both inclusive), keeping
    /// the existing marks.
    pub fn mark_range(&mut self, from: usize, to: usize) {
        let (lo, hi) = (from.min(to), from.max(to));
        for i in lo..=hi {
            self.marked.insert(i);
        }
    }
    /// Replace the previous shift/alt range with the range from the anchor
    /// to `idx`: the old range is unmarked first, so alt+clicking / moving
    /// with Shift closer to the anchor deselects the rows beyond it. When
    /// `idx` is the anchor itself the old range was already unmarked and
    /// everything (including the anchor) ends up deselected.
    pub fn select_range(&mut self, idx: usize) {
        if let Some((lo, hi)) = self.take_range() {
            for i in lo..=hi {
                self.marked.remove(&i);
            }
        }
        let anchor = self.anchor.unwrap_or(idx);
        let (lo, hi) = (anchor.min(idx), anchor.max(idx));
        if lo < hi {
            self.mark_range(lo, hi);
            self.set_range(lo, hi);
        }
    }
}
