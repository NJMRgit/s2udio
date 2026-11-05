//! The **cheap early name** of a pasted video link (2026-11-04).
//!
//! A resolved stream costs seconds of `yt-dlp` extraction (measured on this
//! machine: 2.6 s for the metadata shape, 12.0 s for the `-J` stream shape
//! with the wrapper's media-URL probe), and a playlist row used to sit in the
//! Video list showing the raw link until all of that finished. oEmbed is the
//! providers' own metadata endpoint: one small JSON document, measured at
//! 0.12 s for a YouTube link, that carries the title (and the channel). It is
//! enough to put the real name on the row while the resolve runs.
//!
//! The probe is deliberately **best effort**: an unknown provider, a timeout
//! or an error simply yields no title, and the row keeps its spinner until the
//! real resolve fills it in. Nothing depends on this call.

use std::time::Duration;

/// How long one oEmbed request may take. It is a fast endpoint, and a row's
/// spinner is already a fine fallback, so waiting longer is never right.
const TIMEOUT: Duration = Duration::from_millis(1500);
/// How many links one probe may ask about. A paste is a handful of links;
/// a huge one is a playlist import, whose rows carry their titles already,
/// so a bound only protects the work thread from a pathological paste.
pub const MAX_PROBES: usize = 12;

/// The whole probe's ceiling, whatever the link count. The probe runs on the
/// `work` thread, which also answers Jellyfin, radio and thumbnail requests,
/// so a network that stalls every request must not hold it for
/// `MAX_PROBES * TIMEOUT`: past this budget the remaining links are left to
/// the resolve.
const BUDGET: Duration = Duration::from_secs(3);

/// One provider's answer: the display title (and the channel, when it names
/// one — the row only shows the title, but the channel rides along for free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EarlyTitle {
    pub link: String,
    pub title: String,
    pub channel: Option<String>,
}

/// The oEmbed endpoint for a link, or `None` when the provider has none this
/// app knows how to ask. Only YouTube and SoundCloud are offered: NicoVideo
/// has no oEmbed endpoint, and asking an arbitrary host is not a metadata
/// source, it is a request to a stranger.
pub fn endpoint_for(link: &str) -> Option<String> {
    let parsed = url::Url::parse(link).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    let encoded: String =
        url::form_urlencoded::byte_serialize(link.as_bytes()).collect();
    let base = if is_youtube_host(&host) {
        "https://www.youtube.com/oembed"
    } else if is_soundcloud_host(&host) {
        "https://soundcloud.com/oembed"
    } else {
        return None;
    };
    Some(format!("{base}?url={encoded}&format=json"))
}

fn is_youtube_host(host: &str) -> bool {
    host == "youtube.com"
        || host == "m.youtube.com"
        || host == "music.youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtu.be"
}

fn is_soundcloud_host(host: &str) -> bool {
    host == "soundcloud.com" || host.ends_with(".soundcloud.com") || host == "snd.sc"
}

/// A parsed oEmbed document: only the two fields the row wants.
#[derive(Debug, serde::Deserialize)]
struct OEmbed {
    #[serde(default)]
    title: String,
    #[serde(default)]
    author_name: String,
}

/// Ask the providers for the early titles of `links`, in order.
///
/// A link with no endpoint, a failed request or an empty title is simply
/// absent from the answer — the caller fills what it gets and leaves the rest
/// to the real resolve.
pub fn probe_titles(links: &[String]) -> Vec<EarlyTitle> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(TIMEOUT)
        .timeout(TIMEOUT)
        .user_agent(concat!("s2udio/", env!("CARGO_PKG_VERSION")))
        .build();
    probe_with(links, Some(std::time::Instant::now() + BUDGET), |endpoint| {
        agent
            .get(endpoint)
            .call()
            .ok()?
            .into_json::<OEmbed>()
            .ok()
            .map(|doc| (doc.title, doc.author_name))
    })
}

/// The probe's shape, with the HTTP call and the deadline injected, so the
/// ordering, the endpoint filter, the empty-title handling and the budget are
/// testable without a network.
fn probe_with(
    links: &[String],
    deadline: Option<std::time::Instant>,
    mut fetch: impl FnMut(&str) -> Option<(String, String)>,
) -> Vec<EarlyTitle> {
    let mut titles = Vec::new();
    for link in links.iter().take(MAX_PROBES) {
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            log::debug!(links = links.len(), found = titles.len(); "The early-title probe spent its budget");
            break;
        }
        let Some(endpoint) = endpoint_for(link) else { continue };
        let Some((title, channel)) = fetch(&endpoint) else { continue };
        let title = title.trim().to_owned();
        if title.is_empty() {
            continue;
        }
        let channel = channel.trim().to_owned();
        titles.push(EarlyTitle {
            link: link.clone(),
            title,
            channel: (!channel.is_empty()).then_some(channel),
        });
    }
    titles
}

#[cfg(test)]
mod tests {
    use super::{EarlyTitle, MAX_PROBES, endpoint_for, probe_with};

    const YT: &str = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
    const YT_BE: &str = "https://youtu.be/abc123?t=90";
    const SC: &str = "https://soundcloud.com/artist/track";
    const NICO: &str = "https://www.nicovideo.jp/watch/sm9";

    #[test]
    fn known_providers_get_an_encoded_endpoint() {
        let youtube = endpoint_for(YT).expect("a YouTube watch link has an endpoint");
        assert!(youtube.starts_with("https://www.youtube.com/oembed?url="), "{youtube}");
        // The link rides as a single query value, so its own query cannot
        // split the request.
        assert!(youtube.contains("%3Fv%3DdQw4w9WgXcQ"), "{youtube}");
        assert!(youtube.ends_with("&format=json"), "{youtube}");
        assert!(endpoint_for(YT_BE).is_some(), "a youtu.be link resolves too");
        assert!(
            endpoint_for(SC).is_some_and(|e| e.starts_with("https://soundcloud.com/oembed?url="))
        );
    }

    #[test]
    fn unknown_providers_are_left_to_the_real_resolve() {
        // NicoVideo has no oEmbed endpoint, and a plain host must not be
        // asked about at all.
        assert_eq!(endpoint_for(NICO), None);
        assert_eq!(endpoint_for("https://example.com/video"), None);
        assert_eq!(endpoint_for("not a url"), None);
    }

    #[test]
    fn a_probe_answers_in_order_and_skips_what_it_cannot_get() {
        let links = vec![YT.to_owned(), NICO.to_owned(), SC.to_owned()];
        let titles = probe_with(&links, None, |endpoint| {
            if endpoint.contains("soundcloud") { None } else { Some(("Real Name".into(), "Chan".into())) }
        });
        assert_eq!(
            titles,
            vec![EarlyTitle {
                link: YT.to_owned(),
                title: "Real Name".to_owned(),
                channel: Some("Chan".to_owned()),
            }],
            "the unsupported provider is skipped and the failed request yields nothing"
        );
    }

    #[test]
    fn an_empty_title_is_no_name() {
        // A blank title would render as an empty row that never looks
        // pending; the spinner is the better answer.
        let links = vec![YT.to_owned()];
        assert!(probe_with(&links, None, |_| Some(("   ".into(), String::new()))).is_empty());
    }

    #[test]
    fn a_missing_channel_is_none_not_an_empty_string() {
        let links = vec![YT.to_owned()];
        let titles = probe_with(&links, None, |_| Some(("Name".into(), String::new())));
        assert_eq!(titles[0].channel, None);
    }

    /// A network that stalls every request must not hold the work thread: an
    /// exhausted budget stops the probe and leaves the rows to the resolve.
    #[test]
    fn an_exhausted_budget_asks_about_nothing() {
        let links = vec![YT.to_owned(), SC.to_owned()];
        let mut asked = 0;
        let titles = probe_with(
            &links,
            Some(std::time::Instant::now() - std::time::Duration::from_secs(1)),
            |_| {
                asked += 1;
                Some(("Name".into(), String::new()))
            },
        );
        assert!(titles.is_empty());
        assert_eq!(asked, 0, "no request is made once the budget is gone");
    }

    #[test]
    fn one_probe_asks_about_at_most_the_cap() {
        let links: Vec<String> = (0..(MAX_PROBES + 5)).map(|_| YT.to_owned()).collect();
        let mut asked = 0;
        let titles = probe_with(&links, None, |_| {
            asked += 1;
            Some(("Name".into(), String::new()))
        });
        assert_eq!(asked, MAX_PROBES);
        assert_eq!(titles.len(), MAX_PROBES);
    }
}
