//! Editing assistants that listen to the sound:
//!
//! - `sequence.removeSilences`: the quiet stretches of the chosen clips' sound (pauses in a
//!   talk, dead air before a take) are cut out of the sequence, everything after them moving up
//!   (one ripple delete across all tracks, so picture and sound stay in sync).
//! - `markers.addBeatMarkers`: a sequence marker on every beat (or every `every` beats) of a
//!   music clip.
//! - `clip.cutToBeat`: the selected video clips laid end to end on the beat of a music clip, each
//!   lasting `beats` beats (shorter clips the whole beats they can fill), their sound with them.
//!
//! The sound is read through the renderer (clip gain and clip effects, speed and reverse), a
//! chunk at a time: silence detection keeps one level per 10 ms, beat tracking a mono copy at
//! about 24 kHz of at most [`MAX_MUSIC_S`] seconds.

use filmcraft_edit as edit;
use filmcraft_edit::transcript as tx;
use filmcraft_project::{ClipId, Label, Marker, MarkerId, MarkerKind, TrackId, TrackItem};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, bad, clips_p, f64_p, has_seq, u64_p};
use crate::{EngineError, Result, Session};

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

/// Level analysis step (seconds).
const HOP_S: f64 = 0.01;
/// The longest stretch of sound silence detection reads (3 hours).
const MAX_SPAN_S: f64 = 3.0 * 3600.0;
/// The longest music beat tracking reads (10 minutes).
const MAX_MUSIC_S: f64 = 600.0;
/// Samples read per chunk.
const CHUNK: usize = 1 << 16;

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, enabled: Enabled, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled, run, journal: true, named: crate::commands::NamedKeys::NONE }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "sequence.removeSilences",
            "Remove Silences",
            &["Sequence"],
            r#"{"clips":[id]?,"threshold":dB?,"minSilence":f64=0.6,"padding":f64=0.12}"#,
            has_seq,
            remove_silences,
        ),
        spec("markers.addBeatMarkers", "Add Markers on the Beat", &["Markers"], r#"{"clip":id?,"every":n=1}"#, has_seq, beat_markers),
        spec("clip.cutToBeat", "Cut to the Beat", &["Clip"], r#"{"clips":[id]?,"music":id?,"beats":n=4}"#, has_seq, cut_to_beat),
    ]
}

/// A number parameter in `lo..=hi`, or `default` when absent; anything else is refused.
fn num(p: &Value, key: &str, default: f64, lo: f64, hi: f64, cmd: &str) -> Result<f64> {
    if p.get(key).is_none_or(Value::is_null) {
        return Ok(default);
    }
    match f64_p(p, key) {
        Some(v) if v.is_finite() && (lo..=hi).contains(&v) => Ok(v),
        _ => Err(bad(cmd, format!("`{key}` must be a number from {lo} to {hi}"))),
    }
}

/// A whole-number parameter in `lo..=hi`, or `default` when absent.
fn count(p: &Value, key: &str, default: u64, lo: u64, hi: u64, cmd: &str) -> Result<u64> {
    if p.get(key).is_none_or(Value::is_null) {
        return Ok(default);
    }
    match u64_p(p, key) {
        Some(v) if (lo..=hi).contains(&v) => Ok(v),
        _ => Err(bad(cmd, format!("`{key}` must be a whole number from {lo} to {hi}"))),
    }
}

fn secs(t: Tick) -> f64 {
    t.0 as f64 / TICKS_PER_SECOND as f64
}

fn tick(s: f64) -> Tick {
    Tick::from_seconds_f64(s)
}

/// The audio clips of the active sequence that are partners (same link) of `ids`, or `ids`
/// themselves when they are audio clips.
fn audio_of(s: &Session, ids: &[ClipId]) -> Vec<TrackItem> {
    let Some(q) = s.active_sequence() else { return Vec::new() };
    let links: Vec<u64> = q.all_tracks().flat_map(|t| &t.items).filter(|i| ids.contains(&i.id)).filter_map(|i| i.link).collect();
    q.audio_tracks.iter().flat_map(|t| &t.items).filter(|a| ids.contains(&a.id) || a.link.is_some_and(|l| links.contains(&l))).cloned().collect()
}

/// The mixed (mono) sound of `items` for timeline samples `[a0, a1)` at `sr`, handed to `f` one
/// chunk at a time.
fn read_mono(s: &Session, items: &[TrackItem], a0: i64, a1: i64, sr: u32, mut f: impl FnMut(&[f32])) {
    let provider = s.media.provider(s.project.clone(), s.services.clone());
    let mut pos = a0;
    while pos < a1 {
        let n = usize::try_from(a1 - pos).unwrap_or(CHUNK).min(CHUNK);
        let mut mono = vec![0.0f32; n];
        for it in items {
            if let Some(b) = filmcraft_render::audio::clip_signal(it, pos, n, sr, &provider) {
                for (k, m) in mono.iter_mut().enumerate() {
                    let l = b[0].get(k).copied().unwrap_or(0.0);
                    let r = b[1].get(k).copied().unwrap_or(0.0);
                    *m += 0.5 * (l + r);
                }
            }
        }
        f(&mono);
        pos += n as i64;
    }
}

// ---------------------------------------------------------------------------------------------
// Remove Silences

/// Level (dBFS) per [`HOP_S`] of the mixed sound of `items` over `[t0, t1)`.
fn levels(s: &Session, items: &[TrackItem], t0: Tick, t1: Tick, sr: u32) -> Vec<f32> {
    let (a0, a1) = (t0.to_units_floor(i64::from(sr)), t1.to_units_floor(i64::from(sr)));
    let hop = ((f64::from(sr) * HOP_S).round() as usize).max(1);
    let mut out = Vec::new();
    let (mut acc, mut n) = (0.0f64, 0usize);
    read_mono(s, items, a0, a1, sr, |chunk| {
        for x in chunk {
            acc += f64::from(*x) * f64::from(*x);
            n += 1;
            if n == hop {
                out.push((10.0 * (acc / n as f64).max(1e-12).log10()) as f32);
                (acc, n) = (0.0, 0);
            }
        }
    });
    if n > 0 {
        out.push((10.0 * (acc / n as f64).max(1e-12).log10()) as f32);
    }
    out
}

/// The silent stretches of `levels` (seconds from their start): at least `min_s` below
/// `threshold`, shrunk by `pad_s` at each end so the words keep their attack and tail.
fn silences(levels: &[f32], threshold: f32, min_s: f64, pad_s: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    let mut run: Option<usize> = None;
    for i in 0..=levels.len() {
        let quiet = levels.get(i).is_some_and(|l| *l < threshold);
        match (quiet, run) {
            (true, None) => run = Some(i),
            (false, Some(b)) => {
                let (s, e) = (b as f64 * HOP_S, i as f64 * HOP_S);
                if e - s >= min_s && e - s > 2.0 * pad_s {
                    out.push((s + pad_s, e - pad_s));
                }
                run = None;
            }
            _ => {}
        }
    }
    out
}

/// The default silence threshold: 30 dB under the loud parts of the sound (its 95th
/// percentile level), between -60 and -25 dBFS.
fn auto_threshold(levels: &[f32]) -> f32 {
    let mut v: Vec<f32> = levels.iter().copied().filter(|l| l.is_finite()).collect();
    v.sort_by(f32::total_cmp);
    let loud = v.get(v.len().saturating_mul(95) / 100).copied().unwrap_or(-20.0);
    (loud - 30.0).clamp(-60.0, -25.0)
}

fn remove_silences(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "sequence.removeSilences";
    let min_s = num(p, "minSilence", 0.6, 0.1, 60.0, CMD)?;
    let pad_s = num(p, "padding", 0.12, 0.0, 5.0, CMD)?;
    let threshold = if p.get("threshold").is_none_or(Value::is_null) { None } else { Some(num(p, "threshold", -40.0, -90.0, 0.0, CMD)? as f32) };
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let sr = q.settings.sample_rate.max(1);
    // the selection's sound, or every clip's when nothing is selected
    let chosen = clips_p(s, p);
    let items = if chosen.is_empty() { q.audio_tracks.iter().flat_map(|t| &t.items).cloned().collect() } else { audio_of(s, &chosen) };
    if items.is_empty() {
        return Err(bad(CMD, "select clips with sound (or none, for the whole sequence)"));
    }
    let t0 = items.iter().map(|i| i.start).min().unwrap_or(Tick::ZERO);
    let t1 = items.iter().map(|i| i.end()).max().unwrap_or(Tick::ZERO);
    if secs(t1 - t0) > MAX_SPAN_S {
        return Err(bad(CMD, "that is more than 3 hours of sound: select fewer clips"));
    }
    let lv = levels(s, &items, t0, t1, sr);
    let thr = threshold.unwrap_or_else(|| auto_threshold(&lv));
    let ranges: Vec<TimeRange> = silences(&lv, thr, min_s, pad_s)
        .into_iter()
        .map(|(a, b)| TimeRange::from_bounds(t0 + tick(a), (t0 + tick(b)).min(t1)))
        .filter(|r| r.duration > Tick::ZERO)
        .collect();
    let n = ranges.len();
    if n == 0 {
        return Ok(json!({"removed": 0, "seconds": 0.0, "threshold": thr}));
    }
    let total = s.edit_sequence("Remove Silences", |q, ctx, _| Ok(tx::ripple_delete_ranges(q, ranges, ctx)))?;
    Ok(json!({"removed": n, "seconds": secs(total), "threshold": thr}))
}

// ---------------------------------------------------------------------------------------------
// Beats

/// The music clip a beat command works from: `id`, else a selected audio clip not linked to a
/// selected video clip, else the longest audio clip of the sequence.
fn music_clip(s: &Session, id: Option<ClipId>, cmd: &str) -> Result<TrackItem> {
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let audio = || q.audio_tracks.iter().flat_map(|t| &t.items);
    if let Some(id) = id {
        return audio().find(|a| a.id == id).cloned().ok_or_else(|| bad(cmd, "`music` is not an audio clip of this sequence"));
    }
    let sel = &s.state.selection;
    let picture_links: Vec<u64> = q.video_tracks.iter().flat_map(|t| &t.items).filter(|v| sel.contains(&v.id)).filter_map(|v| v.link).collect();
    audio()
        .find(|a| sel.contains(&a.id) && a.link.is_none_or(|l| !picture_links.contains(&l)))
        .or_else(|| audio().max_by_key(|a| a.duration))
        .cloned()
        .ok_or_else(|| bad(cmd, "there is no music clip: add one to an audio track"))
}

/// The beats of `music` as timeline times, and its tempo.
fn beats(s: &Session, music: &TrackItem, cmd: &str) -> Result<(Vec<Tick>, f64)> {
    let sr = s.active_sequence().map_or(48_000, |q| q.settings.sample_rate.max(1));
    let len = music.duration.min(tick(MAX_MUSIC_S));
    let (a0, a1) = (music.start.to_units_floor(i64::from(sr)), (music.start + len).to_units_floor(i64::from(sr)));
    // decimate to about 24 kHz: enough for the onsets, half the memory
    let d = (sr / 24_000).max(1) as usize;
    let mut mono = Vec::with_capacity(usize::try_from((a1 - a0).max(0)).unwrap_or(0) / d + 1);
    let (mut acc, mut n) = (0.0f32, 0usize);
    read_mono(s, std::slice::from_ref(music), a0, a1, sr, |chunk| {
        for x in chunk {
            acc += *x;
            n += 1;
            if n == d {
                mono.push(acc / d as f32);
                (acc, n) = (0.0, 0);
            }
        }
    });
    let rate = sr / d as u32;
    let a = filmcraft_audio_dsp::remix::analyze(&[&mono], rate).map_err(|_| bad(cmd, "no steady beat found in the music clip"))?;
    // a "beat" in the last quarter beat of the clip is the sound stopping, not a beat (and no
    // cut could use it)
    let tail = tick(a.period / f64::from(rate) / 4.0);
    let end = music.start + len;
    let times = a.beats.iter().map(|b| music.start + Tick::from_units(*b, i64::from(rate))).filter(|t| *t + tail < end).collect();
    Ok((times, a.bpm))
}

fn beat_markers(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "markers.addBeatMarkers";
    let every = count(p, "every", 1, 1, 64, CMD)? as usize;
    let music = music_clip(s, crate::commands::clip_p(p, "clip"), CMD)?;
    let (times, bpm) = beats(s, &music, CMD)?;
    let marks: Vec<Tick> = times.into_iter().step_by(every).collect();
    let n = marks.len();
    s.edit_sequence("Add Markers on the Beat", |q, ctx, _| {
        for (k, t) in marks.iter().enumerate() {
            q.markers.push(Marker {
                url: String::new(),
                author: String::new(),
                custom_color: None,
                id: MarkerId(ctx.alloc()),
                start: *t,
                duration: Tick::ZERO,
                name: format!("{}", k * every + 1),
                comment: String::new(),
                kind: MarkerKind::Comment,
                color: Label::Cerulean,
            });
        }
        q.markers.sort_by_key(|m| m.start);
        Ok(())
    })?;
    Ok(json!({"markers": n, "bpm": bpm}))
}

fn cut_to_beat(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "clip.cutToBeat";
    let per = count(p, "beats", 4, 1, 64, CMD)? as usize;
    let music = music_clip(s, crate::commands::clip_p(p, "music"), CMD)?;
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?.clone();
    let chosen = clips_p(s, p);
    let mut pictures: Vec<(TrackId, TrackItem)> =
        q.video_tracks.iter().flat_map(|t| t.items.iter().filter(|i| chosen.contains(&i.id)).map(move |i| (t.id, i.clone()))).collect();
    pictures.sort_by_key(|(_, i)| i.start);
    let Some(track) = pictures.first().map(|p| p.0) else { return Err(bad(CMD, "select the video clips to cut to the beat")) };
    let (times, bpm) = beats(s, &music, CMD)?;

    // each clip from beat `i`, as many whole beats (up to `per`) as it is long
    let mut plan: Vec<(ClipId, TrackId, Tick, Tick)> = Vec::new(); // clip, track, start, duration
    let mut i = 0usize;
    let mut placed = 0usize;
    for (_, it) in &pictures {
        let Some(&from) = times.get(i) else { break };
        let fits = (1..=per).rev().find(|m| times.get(i + m).is_some_and(|t| *t - from <= it.duration));
        let (dur, next) = match fits {
            Some(m) => (times.get(i + m).map_or(it.duration, |t| *t - from), i + m),
            // shorter than a beat: as it is, the next clip on the following beat
            None => (it.duration, times.iter().position(|t| *t >= from + it.duration).unwrap_or(times.len())),
        };
        plan.push((it.id, track, from, dur));
        // its sound comes along, cut the same
        if let Some(l) = it.link {
            for (tid, a) in q.audio_tracks.iter().flat_map(|t| t.items.iter().filter(move |a| a.link == Some(l)).map(move |a| (t.id, a))) {
                let offset = a.start - it.start;
                let adur = (dur - offset).min(a.duration);
                if adur > Tick::ZERO {
                    plan.push((a.id, tid, from + offset, adur));
                }
            }
        }
        placed += 1;
        i = next;
    }
    if placed == 0 {
        return Err(bad(CMD, "the music has no beat to start on"));
    }
    s.edit_sequence("Cut to the Beat", |q, ctx, st| {
        for (id, _, _, dur) in &plan {
            if let Some(it) = q.all_tracks_mut().flat_map(|t| t.items.iter_mut()).find(|i| i.id == *id) {
                it.duration = (*dur).max(Tick(1));
            }
        }
        let moves: Vec<(ClipId, TrackId, Tick)> = plan.iter().map(|(c, t, at, _)| (*c, *t, *at)).collect();
        edit::move_items(q, &moves, false, ctx)?;
        st.selection = plan.iter().map(|p| p.0).collect();
        Ok(())
    })?;
    Ok(json!({"clips": placed, "bpm": bpm, "beatsPerClip": per}))
}

#[cfg(test)]
mod tests;
