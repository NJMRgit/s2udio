mod cli;
mod downloader;
mod error;
mod manager;
mod stream;
mod ytdlp_item;

pub use cli::{init_and_download, search_pick_cli};
pub use downloader::{YtDlp, YtDlpDownloadResult, YtDlpSearchItem};
pub use error::YtDlpDownloadError;
pub use manager::{
    ChapterSection, DownloadId, DownloadState, ReplaceAction, StreamDownloadSpec, YtDlpManager,
};
pub use stream::{YtStreamInfo, resolve_audio_urls};
pub use ytdlp_item::{
    StreamIntent, YtDlpContent, YtDlpHost, YtDlpItem, YtDlpPlaylist, tagged_stream_entry,
    tagged_stream_link, untag_stream_link, yt_video_id,
};
