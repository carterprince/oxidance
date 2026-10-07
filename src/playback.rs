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

#[derive(Debug, Deserialize)]
pub struct Stream {
    pub url: String,
    #[serde(default)]
    pub http_headers: BTreeMap<String, String>,
}

/// Resolve fresh audio URLs without writing media to disk. Cancel obsolete requests.
pub fn resolve(video_id: &str, cancelled: Arc<AtomicBool>) -> Result<Stream, String> {
    let args: Vec<String> = [
        "--ignore-config", "--no-playlist", "--skip-download", "--dump-single-json",
        "--no-warnings", "--js-runtimes", "node", "--socket-timeout", "15",
        "--retries", "1", "--extractor-retries", "1", "-f", "bestaudio", "--",
    ].into_iter().map(str::to_owned).chain([format!("https://music.youtube.com/watch?v={video_id}")]).collect();
    let output = run(&args, cancelled, Duration::from_secs(60))?;
    let stream: Stream = serde_json::from_slice(&output).map_err(|error| format!("Invalid yt-dlp response: {error}"))?;
    if !stream.url.starts_with("https://") && !stream.url.starts_with("http://") {
        return Err("yt-dlp returned an unsupported stream URL".into());
    }
    Ok(stream)
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
