# s2udio — rmpc fork

A heavily modified fork of [rmpc](https://github.com/mierak/rmpc) v0.11.0

a fully featured media center TUI built on rmpc that adds support for video via mpv and a bunch of other goodies!

## Added Features:
- synchronized lyrics + fetch
- Jellyfin!
- Online streams via yt-dlp
    * includes description and chapters for yt
- TUI maintained playlists (persist if MPV is closed)
- mpv/mpris helper - wraps mpris data and manages/tracks mpv playback.
    * allows tui to be closed during playback without interruptions
- Online radio - browse and listen to stations all over the world!
- copy + paste and drag n' drop support for audio and video files/links/.magnet/torrent
- play videos as audio
- download stream/torrent
- full mouse controls
- sensible and intuitive key binds

## Dependencies
- yt-dlp
- mpDris2
- rqbit (torrent streaming only — static binary or `cargo install rqbit`; not installed by setup.sh)
- ffmpeg
- cava
- mpv
- mpd
- kitty* (not a hard requirement, but development and testing is focused on kitty)
- [STTM](https://github.com/NJMRgit/STTM) - TUI supports auto theming if using my KDE theme tool.
    * planned support for following KDE accent color

## Settings & Config
Configuration is stored at ~/.config/s2udio and separate from rmpc
