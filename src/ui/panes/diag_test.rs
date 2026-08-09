#[test]
fn diag_mpv_info_layout() {
    use crate::{shared::ytdlp::YtStreamInfo, ui::panes::lyrics::LyricsPane, ui::panes::Pane};
    let (app_tx, _app_rx) = crossbeam::channel::unbounded();
    let mut ctx = crate::tests::fixtures::ctx(
        (app_tx, _app_rx),
        (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
    );
    ctx.status.state = crate::mpd::commands::State::Pause;
    ctx.mpv.active = true;
    ctx.mpv.duration = 710.0;
    let description = "This is a fairly long YouTube video description that keeps going and going and going and going so we can see exactly how the wrapping behaves inside the info box column. Another paragraph begins here with more words to wrap around the available width, testing the layout of the description body.";
    ctx.yt_info.borrow_mut().insert(
        "https://youtu.be/abc".to_owned(),
        YtStreamInfo {
            url: "https://rr4.example/audio.m4a".to_owned(),
            title: "A Long Scrolling Title".to_owned(),
            description: Some(description.to_owned()),
            ..Default::default()
        },
    );
    ctx.mpv.playlist.borrow_mut().push(crate::core::mpv::MpvPlaylistEntry::new(
        "A Long Scrolling Title",
        "https://youtu.be/abc",
        Some(710.0),
    ));
    ctx.mpv.playlist_pos.set(Some(0));
    let mut pane = LyricsPane::new(&ctx);
    let backend = ratatui::backend::TestBackend::new(46, 14);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 46, 14), &ctx).unwrap()).unwrap();
    let buf = terminal.backend().buffer();
    let out: Vec<String> = (0..14u16)
        .map(|y| (0..46u16).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
        .collect();
    println!("=== MPV INFO BOX (46x14) ===");
    for (i, l) in out.iter().enumerate() {
        println!("{:02}|{}|", i, l);
    }
}
