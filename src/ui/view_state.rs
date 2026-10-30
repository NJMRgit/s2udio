//! View state a modal hands to the one that replaces it **in place**.
//!
//! A modal rebuilt on a background event (the torrent picker, refreshed on
//! every scan event) is a brand-new object: the successor would show its
//! builder's defaults instead of what the user did — ticks, cursor, focus.
//! The UI passes the state across as one opaque [`ModalVis`], so a generic
//! widget like `MenuModal` never has to learn about the flows built on it.
//!
//! The torrent picker's entries live in `Ctx.torrent_picker_ui`: its rebuild
//! closure can only reach `Ctx`, so that is where the state it reads back is
//! kept.

/// What a modal hands to its in-place successor (see
/// [`Modal::vis`](crate::ui::modals::Modal::vis)).
#[derive(Debug, Clone, Default)]
pub enum ModalVis {
    /// Nothing to carry (the successor starts clean).
    #[default]
    Nothing,
    /// The torrent picker: the cursor, the ticked files and the
    /// footer-button focus.
    TorrentPicker(crate::ui::modals::torrent_picker::PickerUi),
}
