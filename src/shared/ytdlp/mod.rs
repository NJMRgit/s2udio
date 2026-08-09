mod cli;
mod downloader;
mod error;
mod manager;
mod stream;
mod ytdlp_item;

pub use cli::{init_and_download, search_pick_cli};
pub use downloader::{YtDlp, YtDlpDownloadResult, YtDlpSearchItem};
pub use error::YtDlpDownloadError;
pub use manager::{DownloadId, DownloadState, ReplaceAction, StreamDownloadSpec, YtDlpManager};
pub use stream::{YtStreamInfo, resolve_audio_urls};
pub use ytdlp_item::{YtDlpContent, YtDlpHost, YtDlpItem, YtDlpPlaylist};
