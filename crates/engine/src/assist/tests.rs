use serde_json::json;

use crate::Session;
use filmcraft_project::{ClipId, TrackKind};
use filmcraft_time::TICKS_PER_SECOND;

const SR: u32 = 48_000;

/// A 16-bit stereo WAV of `samples` (the same in both channels) in a fresh temporary directory.
fn wav(name: &str, samples: &[f32]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("filmcraft-assist-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.wav"));
    let data_len = samples.len() as u32 * 4;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&SR.to_le_bytes());
    b.extend_from_slice(&(SR * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for x in samples {
        let v = (x.clamp(-1.0, 1.0) * 32767.0) as i16;
        b.extend_from_slice(&v.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(&path, b).unwrap();
    path
}

/// `seconds` of a 220 Hz tone (`true`) or silence (`false`), one after the other.
fn speech(parts: &[(bool, f64)]) -> Vec<f32> {
    let mut out = Vec::new();
    for (on, secs) in parts {
        let n = (secs * f64::from(SR)) as usize;
        for i in 0..n {
            let t = (out.len() + i) as f32 / SR as f32;
            out.push(if *on { 0.3 * (t * 220.0 * std::f32::consts::TAU).sin() } else { 0.0 });
        }
    }
    out
}

/// A session with `path` imported and a sequence made from it, active.
fn sequence_of(path: &std::path::Path) -> Session {
    let mut s = Session::default();
    let r = s.execute("file.import", json!({"paths": [path]})).unwrap();
    let item = r["items"][0].as_u64().unwrap_or_else(|| panic!("{r}"));
    s.execute("file.newSequenceFromClip", json!({"items": [item]})).unwrap();
    s
}

fn duration_s(s: &Session) -> f64 {
    s.active_sequence().unwrap().duration().0 as f64 / TICKS_PER_SECOND as f64
}

#[test]
fn the_assistants_are_in_their_menus() {
    let specs = crate::commands::command_specs();
    let menu = |id: &str| specs.iter().find(|c| c.id == id).map(|c| c.menu.to_vec()).unwrap_or_default();
    assert_eq!(menu("sequence.removeSilences"), ["Sequence"]);
    assert_eq!(menu("markers.addBeatMarkers"), ["Markers"]);
    assert_eq!(menu("clip.cutToBeat"), ["Clip"]);
}

/// Two 1.5 s pauses between three phrases go; the phrases keep a short margin; one undo step
/// brings them back.
#[test]
fn silences_are_cut_out_and_the_rest_moves_up() {
    let path = wav("talk", &speech(&[(true, 1.0), (false, 1.5), (true, 1.0), (false, 1.5), (true, 1.0)]));
    let mut s = sequence_of(&path);
    assert!((duration_s(&s) - 6.0).abs() < 0.05, "{}", duration_s(&s));
    let undo = s.history.undo.len();
    let r = s.execute("sequence.removeSilences", json!({"padding": 0.1})).unwrap();
    assert_eq!(r["removed"], 2, "{r}");
    // 3 s of phrases and 2 × 2 × 0.1 s of margin are left
    assert!((duration_s(&s) - 3.4).abs() < 0.06, "left {} s ({r})", duration_s(&s));
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    s.active_sequence().unwrap().check().unwrap();
    s.execute("edit.undo", json!({})).unwrap();
    assert!((duration_s(&s) - 6.0).abs() < 0.05);
}

/// Short pauses (between words) stay: only silences at least `minSilence` long go.
#[test]
fn short_pauses_stay() {
    let path = wav("words", &speech(&[(true, 1.0), (false, 0.3), (true, 1.0), (false, 1.0), (true, 1.0)]));
    let mut s = sequence_of(&path);
    let r = s.execute("sequence.removeSilences", json!({"minSilence": 0.6, "padding": 0.0})).unwrap();
    assert_eq!(r["removed"], 1, "only the long pause: {r}");
    assert!((duration_s(&s) - 3.3).abs() < 0.06, "{}", duration_s(&s));
}

/// Sound without pauses is left as it is, and no undo step is made for nothing.
#[test]
fn nothing_to_remove_changes_nothing() {
    let path = wav("tone", &speech(&[(true, 3.0)]));
    let mut s = sequence_of(&path);
    let undo = s.history.undo.len();
    let r = s.execute("sequence.removeSilences", json!({})).unwrap();
    assert_eq!(r["removed"], 0);
    assert_eq!(s.history.undo.len(), undo);
}

/// A music clip at 120 BPM gets a marker every half second, on its beats.
#[test]
fn beat_markers_fall_on_the_beat() {
    let (music, truth) = filmcraft_audio_dsp::remix::test_music(SR, 120.0, 8, 7);
    let path = wav("music", music.first().unwrap());
    let mut s = sequence_of(&path);
    let r = s.execute("markers.addBeatMarkers", json!({})).unwrap();
    let bpm = r["bpm"].as_f64().unwrap();
    assert!((bpm - 120.0).abs() < 2.0, "{r}");
    let marks: Vec<f64> = s.active_sequence().unwrap().markers.iter().map(|m| m.start.0 as f64 / TICKS_PER_SECOND as f64).collect();
    assert!(marks.len() + 4 >= truth.len(), "{} markers for {} beats", marks.len(), truth.len());
    // every marker within 30 ms of a true beat
    for m in &marks {
        let near = truth.iter().map(|b| (*b as f64 / f64::from(SR) - m).abs()).fold(f64::MAX, f64::min);
        assert!(near < 0.03, "marker at {m} s is {near} s off the beat");
    }
    // every other beat with `every: 2`
    let mut s = sequence_of(&path);
    let r2 = s.execute("markers.addBeatMarkers", json!({"every": 2})).unwrap();
    assert!(r2["markers"].as_u64().unwrap() * 2 <= r["markers"].as_u64().unwrap() + 1);
}

/// Demo clips cut to a 120 BPM track: each starts on a beat and lasts 4 beats (2 s), one after
/// the other, with its sound.
#[test]
fn clips_are_cut_to_the_beat() {
    let (music, _) = filmcraft_audio_dsp::remix::test_music(SR, 120.0, 16, 3);
    let path = wav("song", music.first().unwrap());
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let r = s.execute("file.import", json!({"paths": [path]})).unwrap();
    let song = r["items"][0].as_u64().unwrap();
    // the song on a new audio track from the start
    s.execute("timeline.place", json!({"item": song, "seconds": 0.0, "audioTrack": "new", "video": false})).unwrap();
    let q = s.active_sequence().unwrap();
    let music = q.audio_tracks.last().unwrap().items[0].id.0;
    let clips: Vec<u64> = q.video_tracks[0].items.iter().take(3).map(|i| i.id.0).collect();
    let undo = s.history.undo.len();
    let r = s.execute("clip.cutToBeat", json!({"clips": clips, "music": music, "beats": 4})).unwrap();
    assert_eq!(r["clips"], 3, "{r}");
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    let q = s.active_sequence().unwrap();
    q.check().unwrap();
    let placed: Vec<(f64, f64)> = clips
        .iter()
        .map(|c| {
            let (_, it) = q.find_item(ClipId(*c)).unwrap();
            (it.start.0 as f64 / TICKS_PER_SECOND as f64, it.duration.0 as f64 / TICKS_PER_SECOND as f64)
        })
        .collect();
    for (k, (start, dur)) in placed.iter().enumerate() {
        assert!((dur - 2.0).abs() < 0.06, "clip {k} lasts 4 beats: {placed:?}");
        if k > 0 {
            assert!((start - (placed[k - 1].0 + placed[k - 1].1)).abs() < 0.001, "end to end: {placed:?}");
        }
        // its sound, cut and moved the same
        let (_, it) = q.find_item(ClipId(clips[k])).unwrap();
        if let Some(l) = it.link {
            let a = q.tracks(TrackKind::Audio).iter().flat_map(|t| &t.items).find(|a| a.link == Some(l)).unwrap();
            assert_eq!((a.start, a.duration), (it.start, it.duration), "clip {k}'s sound");
        }
    }
}

/// Bad or impossible requests are refused before anything changes.
#[test]
fn the_assistants_refuse_bad_parameters() {
    let path = wav("bad", &speech(&[(true, 1.0), (false, 1.0), (true, 1.0)]));
    let mut s = sequence_of(&path);
    let undo = s.history.undo.len();
    for (cmd, p) in [
        ("sequence.removeSilences", json!({"minSilence": 0})),
        ("sequence.removeSilences", json!({"minSilence": "long"})),
        ("sequence.removeSilences", json!({"padding": -1})),
        ("sequence.removeSilences", json!({"threshold": 12})),
        ("sequence.removeSilences", json!({"clips": [999999]})),
        ("markers.addBeatMarkers", json!({"every": 0})),
        ("markers.addBeatMarkers", json!({"clip": 999999})),
        // a tone with pauses has no steady beat
        ("markers.addBeatMarkers", json!({})),
        ("clip.cutToBeat", json!({"beats": 1000})),
        ("clip.cutToBeat", json!({"clips": []})),
    ] {
        assert!(s.execute(cmd, p.clone()).is_err(), "{cmd} {p}");
    }
    assert_eq!(s.history.undo.len(), undo, "nothing changed");
}
