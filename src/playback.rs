use std::{collections::BTreeMap, io::Read, process::{Command, Stdio}, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};

use serde::Deserialize;
use gst::prelude::*;

/// Resample before the sink, which otherwise changes speed by skipping/repeating
/// samples. Matching pitch to the segment rate disables pitch preservation.
pub fn speed_filter() -> Result<gst::Bin, String> {
    let bin = gst::Bin::new();
    let input = gst::ElementFactory::make("audioconvert").build().map_err(|e| e.to_string())?;
    let pitch = gst::ElementFactory::make("pitch").build()
        .map_err(|_| "Playback requires the GStreamer SoundTouch (pitch) plugin".to_owned())?;
    let output = gst::ElementFactory::make("audioconvert").build().map_err(|e| e.to_string())?;
    bin.add_many([&input, &pitch, &output]).map_err(|e| e.to_string())?;
    gst::Element::link_many([&input, &pitch, &output]).map_err(|e| e.to_string())?;
    let weak = pitch.downgrade();
    pitch.static_pad("sink").unwrap().add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(event)) = info.data.as_ref() {
            if let gst::EventView::Segment(event) = event.view() {
                if let Some(pitch) = weak.upgrade() {
                    pitch.set_property("pitch", event.segment().rate().abs() as f32);
                }
            }
        }
        gst::PadProbeReturn::Ok
    });
    for (name, element) in [("sink", &input), ("src", &output)] {
        let pad = gst::GhostPad::builder_with_target(&element.static_pad(name).unwrap())
            .map_err(|e| e.to_string())?.name(name).build();
        bin.add_pad(&pad).map_err(|e| e.to_string())?;
    }
    Ok(bin)
}

#[cfg(test)]
mod speed_tests {
    use super::*;
    use std::sync::Mutex;

    fn render(frequency: f64, rate: f64) -> Vec<f32> {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let source = gst::ElementFactory::make("audiotestsrc").property("freq", frequency)
            .property("volume", 0.5_f64)
            .property("samplesperbuffer", 480_i32).build().unwrap();
        let caps = gst::ElementFactory::make("capsfilter").property("caps",
            gst::Caps::builder("audio/x-raw").field("format", "F32LE")
                .field("rate", 48000_i32).field("channels", 1_i32).build()).build().unwrap();
        let filter = speed_filter().unwrap();
        let sink = gst::ElementFactory::make("fakesink").property("sync", false).build().unwrap();
        pipeline.add_many([&source, &caps, filter.upcast_ref(), &sink]).unwrap();
        gst::Element::link_many([&source, &caps, filter.upcast_ref(), &sink]).unwrap();
        let samples = Arc::new(Mutex::new(Vec::new()));
        sink.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, {
            let samples = samples.clone();
            move |_, info| {
                if let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_ref() {
                    let map = buffer.map_readable().unwrap();
                    samples.lock().unwrap().extend(map.as_slice().chunks_exact(4)
                        .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap())));
                }
                gst::PadProbeReturn::Ok
            }
        });
        pipeline.set_state(gst::State::Paused).unwrap();
        assert_eq!(pipeline.state(gst::ClockTime::from_seconds(5)).1, gst::State::Paused);
        samples.lock().unwrap().clear();
        pipeline.seek(rate, gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set, gst::ClockTime::ZERO, gst::SeekType::Set, gst::ClockTime::from_seconds(1)).unwrap();
        pipeline.state(gst::ClockTime::from_seconds(5)).0.unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline.bus().unwrap().timed_pop_filtered(gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error]).unwrap();
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(matches!(message.view(), gst::MessageView::Eos(_)), "{message:?}");
        std::mem::take(&mut *samples.lock().unwrap())
    }

    #[test]
    fn speed_resampling_changes_pitch_and_rejects_aliasing() {
        for rate in [0.5, 1.0, 1.5, 2.0] {
            let samples = render(1000.0, rate);
            let middle = &samples[samples.len()/4..samples.len()*3/4];
            let crossings = middle.windows(2).filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0).count();
            let frequency = crossings as f64 * 48000.0 / middle.len() as f64;
            assert!((frequency - 1000.0 * rate).abs() < 10.0, "rate={rate}, frequency={frequency}");
            assert!((samples.len() as f64 / 48000.0 - 1.0/rate).abs() < 0.05,
                "rate={rate}, samples={}", samples.len());
        }
        // 20 kHz at 1.5× exceeds Nyquist: it must be filtered, not fold into a 18 kHz artifact.
        let samples = render(20000.0, 1.5);
        let middle = &samples[samples.len()/4..samples.len()*3/4];
        let rms = (middle.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / middle.len() as f64).sqrt();
        assert!(rms < 0.03, "aliased high-frequency audio was not suppressed: RMS={rms}");
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Stream {
    pub url: String,
    #[serde(default)]
    pub http_headers: BTreeMap<String, String>,
}

fn dump_args(url: &str) -> Vec<String> {
    [
        "--ignore-config", "--no-playlist", "--skip-download", "--dump-single-json",
        "--no-warnings", "--js-runtimes", "node", "--socket-timeout", "15",
        "--retries", "1", "--extractor-retries", "1", "-f", "bestaudio/best", "--",
    ].into_iter().map(str::to_owned).chain([url.to_owned()]).collect()
}

fn valid(stream: Stream) -> Result<Stream, String> {
    if !stream.url.starts_with("https://") && !stream.url.starts_with("http://") {
        return Err("yt-dlp returned an unsupported stream URL".into());
    }
    Ok(stream)
}

/// Resolve fresh audio URLs for a song's page without writing media to disk. Cancel obsolete requests.
pub fn resolve(page_url: &str, cancelled: Arc<AtomicBool>) -> Result<Stream, String> {
    let output = run(&dump_args(page_url), cancelled, Duration::from_secs(60))?;
    valid(serde_json::from_slice(&output).map_err(|error| format!("Invalid yt-dlp response: {error}"))?)
}

/// A song found at a link, with whatever details yt-dlp could detect.
#[derive(Debug)]
pub struct Detected {
    pub song: oxidance::Song,
    pub stream: Stream,
}

#[derive(Deserialize)]
struct Info {
    #[serde(rename = "_type")]
    kind: Option<String>,
    id: String,
    extractor_key: String,
    webpage_url: Option<String>,
    title: Option<String>,
    track: Option<String>,
    artist: Option<String>,
    creator: Option<String>,
    uploader: Option<String>,
    channel: Option<String>,
    thumbnail: Option<String>,
    #[serde(flatten)]
    stream: Option<Stream>,
}

fn nonempty(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_owned()).filter(|text| !text.is_empty())
}

/// Turns yt-dlp's description of a link into a song. YouTube links become ordinary
/// YouTube songs; other sites keep their page and get an ID prefixed by the site.
fn detected(info: Info, url: &str) -> Result<Detected, String> {
    if info.kind.as_deref().is_some_and(|kind| kind != "video") {
        return Err("This link is an album or playlist. Use a link to a single song.".into());
    }
    let stream = valid(info.stream.ok_or("yt-dlp found no audio at this link")?)?;
    let youtube = info.extractor_key == "Youtube";
    let id: String = if youtube { info.id.clone() } else { format!("{}-{}", info.extractor_key, info.id) }
        .chars().map(|character| if character.is_ascii_alphanumeric() || character == '-' || character == '_' { character } else { '_' }).collect();
    let title = nonempty(info.track).or(nonempty(info.title)).unwrap_or_default();
    let artist = nonempty(info.artist).or(nonempty(info.creator)).or(nonempty(info.uploader)).or(nonempty(info.channel));
    let song = oxidance::Song {
        video_id: id, title, artist, album_art_url: nonempty(info.thumbnail), artists: vec![],
        source_url: (!youtube).then(|| nonempty(info.webpage_url).unwrap_or_else(|| url.to_owned())),
    };
    Ok(Detected { song, stream })
}

/// Detects the song at a link to any site yt-dlp supports.
pub fn detect(url: &str, cancelled: Arc<AtomicBool>) -> Result<Detected, String> {
    let parsed = reqwest::Url::parse(url.trim()).map_err(|_| "Enter a link starting with https://".to_owned())?;
    if !matches!(parsed.scheme(), "http" | "https") { return Err("Enter a link starting with https://".into()); }
    let output = run(&dump_args(parsed.as_str()), cancelled, Duration::from_secs(60))?;
    detected(serde_json::from_slice(&output).map_err(|error| format!("Invalid yt-dlp response: {error}"))?, parsed.as_str())
}

/// Sends the headers yt-dlp requires with each stream request.
pub fn send_headers(player: &gst::Element, headers: BTreeMap<String, String>) {
    player.connect("source-setup", false, move |values| {
        if let Ok(source) = values[1].get::<gst::Element>() {
            if source.find_property("user-agent").is_some() {
                if let Some(agent) = headers.get("User-Agent") { source.set_property("user-agent", agent); }
            }
            if source.find_property("extra-headers").is_some() {
                let mut structure = gst::Structure::builder("headers");
                for (name, value) in &headers { structure = structure.field(name, value); }
                source.set_property("extra-headers", structure.build());
            }
        }
        None
    });
}

pub fn run(args: &[String], cancelled: Arc<AtomicBool>, timeout: Duration) -> Result<Vec<u8>, String> {
    if cancelled.load(Ordering::Relaxed) { return Err("Request cancelled".into()); }
    let mut child = Command::new("yt-dlp").args(args)
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
        .map_err(|error| format!("Could not start yt-dlp: {error}"))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    // Drain both pipes while the subprocess runs, including large metadata responses.
    let output = std::thread::spawn(move || { let mut bytes = Vec::new(); stdout.read_to_end(&mut bytes).map(|_| bytes) });
    let errors = std::thread::spawn(move || { let mut bytes = Vec::new(); stderr.read_to_end(&mut bytes).map(|_| bytes) });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if cancelled.load(Ordering::Relaxed) || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = output.join();
            let _ = errors.join();
            return Err(if cancelled.load(Ordering::Relaxed) { "Request cancelled" } else { "YouTube request timed out. Try again." }.into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => { let _ = child.kill(); let _ = child.wait(); return Err(error.to_string()); }
        }
    };
    let output = output.join().map_err(|_| "Could not read yt-dlp output")?.map_err(|error| error.to_string())?;
    let errors = errors.join().map_err(|_| "Could not read yt-dlp errors")?.map_err(|error| error.to_string())?;
    if !status.success() {
        let message = String::from_utf8_lossy(&errors);
        return Err(message.lines().find(|line| line.starts_with("ERROR:")).unwrap_or("yt-dlp could not resolve this song.").to_owned());
    }
    Ok(output)
}

#[cfg(test)]
mod detect_tests {
    use super::*;

    fn info(value: serde_json::Value) -> Info { serde_json::from_value(value).unwrap() }

    #[test]
    fn links_become_songs_with_detected_details() {
        let bandcamp = info(serde_json::json!({
            "id": "2707922679", "extractor_key": "Bandcamp", "webpage_url": "https://example.bandcamp.com/track/song",
            "title": "Artist - Song", "track": "Song", "artist": "Artist", "uploader": "Uploader",
            "thumbnail": "https://f4.bcbits.com/img/a1_5.jpg", "url": "https://t4.bcbits.com/stream/1",
            "http_headers": {"User-Agent": "agent"}
        }));
        let detected = detected(bandcamp, "https://example.bandcamp.com/track/song?from=share").unwrap();
        assert_eq!(detected.song.video_id, "Bandcamp-2707922679");
        assert_eq!(detected.song.title, "Song");
        assert_eq!(detected.song.artist.as_deref(), Some("Artist"));
        assert_eq!(detected.song.album_art_url.as_deref(), Some("https://f4.bcbits.com/img/a1_5.jpg"));
        assert_eq!(detected.song.page_url(), "https://example.bandcamp.com/track/song");
        assert_eq!(detected.stream.http_headers["User-Agent"], "agent");

        let youtube = info(serde_json::json!({"id": "5NV6Rdv1a3I", "extractor_key": "Youtube", "title": "Video", "url": "https://example.com/a"}));
        let detected = super::detected(youtube, "https://youtu.be/5NV6Rdv1a3I").unwrap();
        assert_eq!(detected.song.video_id, "5NV6Rdv1a3I");
        assert!(detected.song.source_url.is_none(), "YouTube links are ordinary songs");
        assert!(detected.song.artist.is_none() && detected.song.album_art_url.is_none());

        let odd = info(serde_json::json!({"id": "a/b c", "extractor_key": "Generic", "title": " ", "url": "https://example.com/a.mp3"}));
        let detected = super::detected(odd, "https://example.com/a.mp3").unwrap();
        assert_eq!(detected.song.video_id, "Generic-a_b_c");
        assert_eq!(detected.song.title, "", "a missing title is left for the user");
        assert_eq!(detected.song.page_url(), "https://example.com/a.mp3");
    }

    #[test]
    fn albums_and_links_without_audio_are_rejected() {
        let album = info(serde_json::json!({"_type": "playlist", "id": "1", "extractor_key": "Bandcamp"}));
        assert!(detected(album, "https://example.com").unwrap_err().contains("album"));
        let silent = info(serde_json::json!({"id": "1", "extractor_key": "Generic"}));
        assert!(detected(silent, "https://example.com").is_err());
        assert!(detect("not a link", Arc::new(AtomicBool::new(false))).is_err());
    }
}
