//! Lyrics for the track that is playing, from LRCLIB (https://lrclib.net), a
//! free community database that needs no account or key.
//!
//! Nothing is sent until the Media page is opened: then the artist, title,
//! album and length of the current track go to lrclib.net, once per track.
//! Synced lyrics (`[mm:ss.xx] line`) are followed along with the music; where
//! only plain text exists it is shown as a scrolling list.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

const API: &str = "https://lrclib.net/api";

#[derive(Clone, Debug, PartialEq)]
pub enum Lyrics {
    /// (start second, text); an empty text is a pause between lines.
    Synced(Vec<(f64, String)>),
    Plain(Vec<String>),
    Instrumental,
    NotFound,
}

/// What identifies a track for a lookup.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TrackKey {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// Whole seconds; 0 when the player reports no length.
    pub duration: u32,
}

pub enum Status<'a> {
    /// No track asked for yet.
    Idle,
    Loading,
    Ready(&'a Lyrics),
    /// The service could not be reached.
    Failed,
}

#[derive(Default)]
pub struct LyricsLoader {
    key: Option<TrackKey>,
    pending: Option<Receiver<Result<Lyrics, String>>>,
    current: Option<Lyrics>,
    failed: bool,
    cache: HashMap<TrackKey, Lyrics>,
}

impl LyricsLoader {
    /// Call every frame the page is shown with the track on screen. Starts a
    /// lookup when the track changed and collects a finished one.
    pub fn update(&mut self, key: Option<TrackKey>, ctx: &egui::Context) {
        if let Some(rx) = &self.pending
            && let Ok(result) = rx.try_recv()
        {
            self.pending = None;
            match result {
                Ok(lyrics) => {
                    if let Some(k) = &self.key {
                        if self.cache.len() >= 24 {
                            self.cache.clear();
                        }
                        self.cache.insert(k.clone(), lyrics.clone());
                    }
                    self.current = Some(lyrics);
                }
                Err(e) => {
                    tracing::debug!("lyrics: {e}");
                    self.failed = true;
                }
            }
        }
        let Some(key) = key else { return };
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.key = Some(key.clone());
        self.pending = None;
        self.current = None;
        self.failed = false;
        if let Some(hit) = self.cache.get(&key) {
            self.current = Some(hit.clone());
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("nook-lyrics".into())
            .spawn(move || {
                let _ = tx.send(fetch(&key));
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => self.pending = Some(rx),
            Err(_) => self.failed = true,
        }
    }

    pub fn status(&self) -> Status<'_> {
        if self.failed {
            Status::Failed
        } else if let Some(l) = &self.current {
            Status::Ready(l)
        } else if self.pending.is_some() {
            Status::Loading
        } else {
            Status::Idle
        }
    }

    /// Forgets the current track so the next `update` asks again.
    pub fn retry(&mut self) {
        self.key = None;
        self.failed = false;
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
    #[serde(default)]
    duration: f64,
}

impl Entry {
    fn into_lyrics(self) -> Lyrics {
        if self.instrumental {
            return Lyrics::Instrumental;
        }
        if let Some(synced) = &self.synced_lyrics {
            let lines = parse_lrc(synced);
            if !lines.is_empty() {
                return Lyrics::Synced(lines);
            }
        }
        match self.plain_lyrics {
            Some(text) if !text.trim().is_empty() => {
                Lyrics::Plain(text.lines().map(|l| l.trim_end().to_owned()).collect())
            }
            _ => Lyrics::NotFound,
        }
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(12))
        .user_agent(concat!("Nook/", env!("CARGO_PKG_VERSION"), " (lyrics)"))
        .build()
}

/// GET `path` with `params`; `Ok(None)` for "not found".
fn get<T: for<'de> Deserialize<'de>>(
    agent: &ureq::Agent,
    path: &str,
    params: &[(&str, &str)],
) -> Result<Option<T>, String> {
    let mut req = agent.get(&format!("{API}/{path}"));
    for (k, v) in params {
        req = req.query(k, v);
    }
    match req.call() {
        Ok(resp) => resp
            .into_json()
            .map(Some)
            .map_err(|e| format!("reading the answer: {e}")),
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// "Artist - Topic" is how YouTube Music names auto-generated channels.
fn clean_artist(artist: &str) -> &str {
    artist.strip_suffix(" - Topic").unwrap_or(artist).trim()
}

fn fetch(key: &TrackKey) -> Result<Lyrics, String> {
    let agent = agent();
    let artist = clean_artist(&key.artist);
    let duration = key.duration.to_string();

    // An exact match first (needs the length), then a looser search.
    if key.duration > 0 {
        let params = [
            ("track_name", key.title.as_str()),
            ("artist_name", artist),
            ("album_name", key.album.as_str()),
            ("duration", duration.as_str()),
        ];
        if let Some(entry) = get::<Entry>(&agent, "get", &params)? {
            let lyrics = entry.into_lyrics();
            if lyrics != Lyrics::NotFound {
                return Ok(lyrics);
            }
        }
    }
    let params = [("track_name", key.title.as_str()), ("artist_name", artist)];
    let found: Vec<Entry> = get(&agent, "search", &params)?.unwrap_or_default();
    let want = f64::from(key.duration);
    let best = found
        .into_iter()
        .filter(|e| key.duration == 0 || (e.duration - want).abs() <= 4.0)
        .map(Entry::into_lyrics)
        // Prefer lyrics that can be followed along.
        .max_by_key(|l| match l {
            Lyrics::Synced(_) => 3,
            Lyrics::Plain(_) => 2,
            Lyrics::Instrumental => 1,
            Lyrics::NotFound => 0,
        });
    Ok(best.unwrap_or(Lyrics::NotFound))
}

/// Parses LRC text: `[mm:ss.xx]line`, several stamps per line allowed;
/// metadata tags (`[ar:...]`) are skipped.
pub fn parse_lrc(text: &str) -> Vec<(f64, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut rest = line.trim();
        let mut stamps = Vec::new();
        while let Some(tag) = rest.strip_prefix('[') {
            let Some(end) = tag.find(']') else { break };
            match parse_stamp(&tag[..end]) {
                Some(t) => stamps.push(t),
                None => {
                    stamps.clear();
                    break;
                }
            }
            rest = &tag[end + 1..];
        }
        let words = rest.trim();
        for t in stamps {
            out.push((t, words.to_owned()));
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

fn parse_stamp(s: &str) -> Option<f64> {
    let (m, sec) = s.split_once(':')?;
    let m: u32 = m.trim().parse().ok()?;
    let sec: f64 = sec.trim().parse().ok()?;
    (sec >= 0.0).then(|| f64::from(m) * 60.0 + sec)
}

/// Index of the line being sung at `position` seconds.
pub fn current_line(lines: &[(f64, String)], position: f64) -> Option<usize> {
    lines.partition_point(|(t, _)| *t <= position).checked_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_synced_lines() {
        let lrc = "[ar:Someone]\n[00:12.50]Hello\n[01:02.00][01:30.25]Again\n[00:20.00]\n";
        let lines = parse_lrc(lrc);
        assert_eq!(
            lines,
            vec![
                (12.5, "Hello".to_owned()),
                (20.0, String::new()),
                (62.0, "Again".to_owned()),
                (90.25, "Again".to_owned()),
            ]
        );
    }

    #[test]
    fn finds_the_current_line() {
        let lines = parse_lrc("[00:05.00]a\n[00:10.00]b\n[00:15.00]c");
        assert_eq!(current_line(&lines, 1.0), None);
        assert_eq!(current_line(&lines, 5.0), Some(0));
        assert_eq!(current_line(&lines, 12.0), Some(1));
        assert_eq!(current_line(&lines, 99.0), Some(2));
    }

    #[test]
    fn entry_prefers_synced_then_plain() {
        let e = |synced: Option<&str>, plain: Option<&str>, instrumental| Entry {
            instrumental,
            plain_lyrics: plain.map(str::to_owned),
            synced_lyrics: synced.map(str::to_owned),
            duration: 0.0,
        };
        assert!(matches!(
            e(Some("[00:01.00]x"), Some("x"), false).into_lyrics(),
            Lyrics::Synced(_)
        ));
        assert!(matches!(
            e(Some(""), Some("one\ntwo"), false).into_lyrics(),
            Lyrics::Plain(l) if l.len() == 2
        ));
        assert_eq!(e(None, None, true).into_lyrics(), Lyrics::Instrumental);
        assert_eq!(e(None, None, false).into_lyrics(), Lyrics::NotFound);
    }

    #[test]
    fn topic_suffix_is_dropped() {
        assert_eq!(clean_artist("Adele - Topic"), "Adele");
        assert_eq!(clean_artist("Adele"), "Adele");
    }
}
