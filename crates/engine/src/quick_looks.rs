//! Quick looks: one-click layouts built from the effects FilmCraft already has, so every part
//! stays adjustable in Effect Controls afterwards.
//!
//! - `clip.quickLook.bubble`: the picture in a round, magnified bubble with a light rim that
//!   floats gently (Rounded Crop + Spherize + Wiggle, Motion sized to the frame).
//! - `clip.quickLook.pictureInPicture`: a small framed picture in a corner.
//! - `clip.quickLook.spinCard`: the picture turns like a card in 3D (3D Rotate keyframes).
//! - `clip.quickLook.grid` / `clip.quickLook.rotatingGrid`: 2 to 9 clips side by side in a grid,
//!   still or turning as one square. The clips are stacked on free video tracks at the start of
//!   the earliest one (their linked sound comes along, on free audio tracks), each is cropped
//!   to its cell, and every cell turns around the grid's centre (Motion's anchor point is the
//!   grid centre in each clip's own pixels).
//!
//! Each command is one undo step. Parameters are checked before anything changes.

use filmcraft_edit as edit;
use filmcraft_geom::Vec2;
use filmcraft_project::{ClipId, EffectInstance, Keyframe, ParamValue, Sequence, Track, TrackId, TrackItem, TrackKind};
use filmcraft_time::Tick;
use serde_json::{Value, json};

use crate::commands::{CommandSpec, bad, bool_p, clips_p, f64_p, has_selection, str_p};
use crate::{Result, Session};

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

const MENU: &[&str] = &["Clip", "Quick Looks"];
/// The most clips a grid lays out (3 × 3).
const MAX_GRID: usize = 9;

fn spec(id: &'static str, label: &'static str, params: &'static str, enabled: Enabled, run: Run) -> CommandSpec {
    CommandSpec { id, label, menu: MENU, shortcut: None, params, enabled, run, journal: true, named: crate::commands::NamedKeys::NONE }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![
        spec("clip.quickLook.bubble", "Bubble", r#"{"clips":[id]?,"size":f64=70,"float":bool=true}"#, has_selection, bubble),
        spec(
            "clip.quickLook.pictureInPicture",
            "Picture in Picture",
            r#"{"clips":[id]?,"corner":"topLeft|topRight|bottomLeft|bottomRight"="bottomRight","size":f64=30}"#,
            has_selection,
            picture_in_picture,
        ),
        spec("clip.quickLook.spinCard", "Spinning Card (3D)", r#"{"clips":[id]?,"turns":f64=1,"axis":"y|x"="y"}"#, has_selection, spin_card),
        spec("clip.quickLook.grid", "Video Grid (Split Screen)", r#"{"clips":[id]?,"size":f64=90,"gap":f64=2}"#, has_selection, grid_still),
        spec("clip.quickLook.rotatingGrid", "Rotating Video Square", r#"{"clips":[id]?,"turns":f64=1,"size":f64=70,"gap":f64=2}"#, has_selection, grid_turning),
    ]
}

// ---------------------------------------------------------------------------------------------
// parameters

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

/// The video clips among the selection (or `clips`), in timeline order: earliest first, then
/// lowest track. Each with its track index, item and source picture size.
fn video_clips(s: &Session, p: &Value, cmd: &str) -> Result<Vec<(usize, TrackItem, (u32, u32))>> {
    let wanted = clips_p(s, p);
    let q = s.active_sequence().ok_or(crate::EngineError::NoSequence)?;
    let (fw, fh) = (q.settings.width.max(1), q.settings.height.max(1));
    let mut out: Vec<(usize, TrackItem, (u32, u32))> = Vec::new();
    for (ti, t) in q.video_tracks.iter().enumerate() {
        for it in t.items.iter().filter(|i| wanted.contains(&i.id)) {
            let size = s.project.source_size(it.item).filter(|(w, h)| *w > 0 && *h > 0).unwrap_or((fw, fh));
            out.push((ti, it.clone(), size));
        }
    }
    out.sort_by_key(|(ti, it, _)| (it.start, *ti));
    if out.is_empty() {
        return Err(bad(cmd, "select video clips first"));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// effect helpers

fn set(e: &mut EffectInstance, name: &str, v: ParamValue) {
    if let Some(p) = e.params.get_mut(name) {
        p.value = v;
        p.keyframes.clear();
    }
}

fn point(x: f64, y: f64) -> ParamValue {
    ParamValue::Vec2(Vec2 { x, y })
}

/// A standard effect for a clip of source size `size` in a frame of `frame`.
fn effect(id: &str, frame: (u32, u32), size: (u32, u32)) -> Result<EffectInstance> {
    let def = filmcraft_project::find_effect(id).ok_or_else(|| crate::EngineError::Other(format!("the {id} effect is missing")))?;
    let mut inst = def.instance();
    filmcraft_project::resolve_auto_points(&mut inst, frame, size);
    Ok(inst)
}

/// The clip's Motion, added at its defaults if the clip has none.
fn motion(it: &mut TrackItem, frame: (u32, u32)) -> Result<&mut EffectInstance> {
    if !it.effects.iter().any(|e| e.effect == "motion") {
        let inst = effect("motion", frame, frame)?;
        it.effects.insert(0, inst);
    }
    it.effects.iter_mut().find(|e| e.effect == "motion").ok_or_else(|| crate::EngineError::Other("Motion is missing".into()))
}

/// Replace an effect the same quick look added before (re-applying adjusts instead of stacking).
fn put(it: &mut TrackItem, inst: EffectInstance) {
    match it.effects.iter_mut().find(|e| e.effect == inst.effect) {
        Some(e) => *e = inst,
        None => it.effects.push(inst),
    }
}

/// Rounded Crop that keeps a `keep_w` × `keep_h` window (source pixels) in the middle of a
/// `size` picture, with `radius` rounding and a `border` rim of `color`.
fn crop(frame: (u32, u32), size: (u32, u32), keep: (f64, f64), radius: f64, border: f64, color: [f32; 4]) -> Result<EffectInstance> {
    let (sw, sh) = (f64::from(size.0), f64::from(size.1));
    let side = |keep: f64, full: f64| ((1.0 - (keep / full).clamp(0.0, 1.0)) / 2.0 * 100.0).clamp(0.0, 50.0);
    let mut c = effect("rounded_crop", frame, size)?;
    set(&mut c, "left", ParamValue::Float(side(keep.0, sw)));
    set(&mut c, "right", ParamValue::Float(side(keep.0, sw)));
    set(&mut c, "top", ParamValue::Float(side(keep.1, sh)));
    set(&mut c, "bottom", ParamValue::Float(side(keep.1, sh)));
    set(&mut c, "radius", ParamValue::Float(radius.clamp(0.0, 2000.0)));
    set(&mut c, "feather", ParamValue::Float(1.5));
    set(&mut c, "border", ParamValue::Float(border.clamp(0.0, 500.0)));
    set(&mut c, "border_color", ParamValue::Color(color));
    Ok(c)
}

/// Keyframes taking a parameter from `from` to `to` between timeline times `t0` and `t1` of
/// clip `it`. Keyframes are in media time (what the clip shows at that moment, with its speed,
/// reverse and holds), so clips cut from different points of their sources still move together.
fn ramp(it: &TrackItem, t0: Tick, t1: Tick, from: f64, to: f64) -> Vec<Keyframe> {
    let (m0, m1) = (it.effect_time_at(t0), it.effect_time_at(t1));
    if m0 == m1 {
        return Vec::new();
    }
    let mut keys = vec![Keyframe::new(m0, ParamValue::Float(from)), Keyframe::new(m1, ParamValue::Float(to))];
    keys.sort_by_key(|k| k.time);
    keys
}

/// Write `f` into the clips `ids` of the active sequence, as one undo step named `label`.
fn edit_clips(s: &mut Session, label: &str, ids: &[ClipId], f: impl Fn(&mut TrackItem) -> Result<()>) -> Result<usize> {
    s.edit_sequence(label, |q, _, _| {
        let mut n = 0;
        for t in q.video_tracks.iter_mut() {
            for it in t.items.iter_mut().filter(|i| ids.contains(&i.id)) {
                f(it)?;
                n += 1;
            }
        }
        Ok(n)
    })
}

// ---------------------------------------------------------------------------------------------
// Bubble

fn bubble(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "clip.quickLook.bubble";
    let size = num(p, "size", 70.0, 10.0, 100.0, CMD)?;
    let float = bool_p(p, "float").unwrap_or(true);
    let clips = video_clips(s, p, CMD)?;
    let frame = s.active_sequence().map(|q| (q.settings.width.max(1), q.settings.height.max(1))).ok_or(crate::EngineError::NoSequence)?;
    let sizes: Vec<(ClipId, (u32, u32))> = clips.iter().map(|(_, it, sz)| (it.id, *sz)).collect();
    let ids: Vec<ClipId> = sizes.iter().map(|c| c.0).collect();
    let (fw, fh) = (f64::from(frame.0), f64::from(frame.1));
    let n = edit_clips(s, "Bubble", &ids, |it| {
        let sz = sizes.iter().find(|c| c.0 == it.id).map_or(frame, |c| c.1);
        let side = f64::from(sz.0.min(sz.1));
        // a disc: a square window rounded to its half side, a thin pale rim
        let rim = (side * 0.008).max(2.0);
        put(it, crop(frame, sz, (side, side), side / 2.0, rim, [0.82, 0.94, 1.0, 0.75])?);
        // the bulge of a drop of water
        let mut sph = effect("spherize", frame, sz)?;
        set(&mut sph, "radius", ParamValue::Float((side / 2.0).clamp(0.0, 2500.0)));
        put(it, sph);
        if float {
            let mut w = effect("wiggle", frame, sz)?;
            set(&mut w, "frequency", ParamValue::Float(0.35));
            set(&mut w, "amount", ParamValue::Float((fh * 0.02).clamp(0.0, 2000.0)));
            set(&mut w, "rotation", ParamValue::Float(3.0));
            set(&mut w, "scale", ParamValue::Float(2.0));
            put(it, w);
        }
        // the bubble's diameter is `size` % of the frame's short side, in the middle
        it.scale_to_frame = false;
        let k = size / 100.0 * fw.min(fh) / side;
        let m = motion(it, frame)?;
        set(m, "scale", ParamValue::Float(k * 100.0));
        set(m, "scale_width", ParamValue::Float(k * 100.0));
        set(m, "position", point(fw / 2.0, fh / 2.0));
        set(m, "anchor", point(f64::from(sz.0) / 2.0, f64::from(sz.1) / 2.0));
        Ok(())
    })?;
    Ok(json!({"clips": n}))
}

// ---------------------------------------------------------------------------------------------
// Picture in Picture

fn picture_in_picture(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "clip.quickLook.pictureInPicture";
    let size = num(p, "size", 30.0, 5.0, 90.0, CMD)?;
    let corner = str_p(p, "corner").unwrap_or("bottomRight");
    let (right, bottom) = match corner {
        "topLeft" => (false, false),
        "topRight" => (true, false),
        "bottomLeft" => (false, true),
        "bottomRight" => (true, true),
        other => return Err(bad(CMD, format!("unknown corner `{other}` (topLeft, topRight, bottomLeft or bottomRight)"))),
    };
    let clips = video_clips(s, p, CMD)?;
    let frame = s.active_sequence().map(|q| (q.settings.width.max(1), q.settings.height.max(1))).ok_or(crate::EngineError::NoSequence)?;
    let sizes: Vec<(ClipId, (u32, u32))> = clips.iter().map(|(_, it, sz)| (it.id, *sz)).collect();
    let ids: Vec<ClipId> = sizes.iter().map(|c| c.0).collect();
    let (fw, fh) = (f64::from(frame.0), f64::from(frame.1));
    let n = edit_clips(s, "Picture in Picture", &ids, |it| {
        let sz = sizes.iter().find(|c| c.0 == it.id).map_or(frame, |c| c.1);
        let (sw, sh) = (f64::from(sz.0), f64::from(sz.1));
        // `size` % of the frame width, the whole picture, a rounded white frame
        let k = size / 100.0 * fw / sw;
        put(it, crop(frame, sz, (sw, sh), (sw.min(sh) * 0.04).max(4.0), (4.0 / k).max(1.0), [1.0, 1.0, 1.0, 1.0])?);
        let (dw, dh) = (sw * k, sh * k);
        let margin = fh * 0.04;
        let x = if right { fw - margin - dw / 2.0 } else { margin + dw / 2.0 };
        let y = if bottom { fh - margin - dh / 2.0 } else { margin + dh / 2.0 };
        it.scale_to_frame = false;
        let m = motion(it, frame)?;
        set(m, "scale", ParamValue::Float(k * 100.0));
        set(m, "scale_width", ParamValue::Float(k * 100.0));
        set(m, "position", point(x, y));
        set(m, "anchor", point(sw / 2.0, sh / 2.0));
        Ok(())
    })?;
    Ok(json!({"clips": n, "corner": corner}))
}

// ---------------------------------------------------------------------------------------------
// Spinning card

fn spin_card(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "clip.quickLook.spinCard";
    let turns = num(p, "turns", 1.0, -20.0, 20.0, CMD)?;
    let param = match str_p(p, "axis").unwrap_or("y") {
        "y" => "rot_y",
        "x" => "rot_x",
        other => return Err(bad(CMD, format!("unknown axis `{other}` (x or y)"))),
    };
    let clips = video_clips(s, p, CMD)?;
    let frame = s.active_sequence().map(|q| (q.settings.width.max(1), q.settings.height.max(1))).ok_or(crate::EngineError::NoSequence)?;
    let sizes: Vec<(ClipId, (u32, u32))> = clips.iter().map(|(_, it, sz)| (it.id, *sz)).collect();
    let ids: Vec<ClipId> = sizes.iter().map(|c| c.0).collect();
    let n = edit_clips(s, "Spinning Card", &ids, |it| {
        let sz = sizes.iter().find(|c| c.0 == it.id).map_or(frame, |c| c.1);
        let mut r = effect("rotate_3d", frame, sz)?;
        if let Some(v) = r.params.get_mut(param) {
            v.value = ParamValue::Float(0.0);
            v.keyframes = ramp(it, it.start, it.end(), 0.0, 360.0 * turns);
        }
        put(it, r);
        Ok(())
    })?;
    Ok(json!({"clips": n, "turns": turns}))
}

// ---------------------------------------------------------------------------------------------
// Grids

fn grid_still(s: &mut Session, p: &Value) -> Result<Value> {
    grid(s, p, "clip.quickLook.grid", 0.0, 90.0)
}

fn grid_turning(s: &mut Session, p: &Value) -> Result<Value> {
    grid(s, p, "clip.quickLook.rotatingGrid", 1.0, 70.0)
}

/// The first track of `kind` from index `from` up whose span `start .. start + len` is free of
/// clips other than `moving`, among `tracks` (or the index one past the end: a new track).
fn free_track(tracks: &[Track], from: usize, start: Tick, len: Tick, moving: &[ClipId], taken: &[usize]) -> usize {
    let end = start.saturating_add(len);
    (from..)
        .find(|&i| {
            !taken.contains(&i)
                && tracks.get(i).is_none_or(|t| !t.locked && t.items.iter().all(|it| moving.contains(&it.id) || it.end() <= start || it.start >= end))
        })
        .unwrap_or(from)
}

/// The grid layout of `n` cells: columns, rows.
fn grid_shape(n: usize) -> (usize, usize) {
    let cols = (1..=n).find(|c| c * c >= n).unwrap_or(1);
    (cols, n.div_ceil(cols))
}

fn grid(s: &mut Session, p: &Value, cmd: &'static str, default_turns: f64, default_size: f64) -> Result<Value> {
    let turns = num(p, "turns", default_turns, -20.0, 20.0, cmd)?;
    let size = num(p, "size", default_size, 10.0, 100.0, cmd)?;
    let gap = num(p, "gap", 2.0, 0.0, 30.0, cmd)?;
    let clips = video_clips(s, p, cmd)?;
    if !(2..=MAX_GRID).contains(&clips.len()) {
        return Err(bad(cmd, format!("select 2 to {MAX_GRID} video clips ({} selected)", clips.len())));
    }
    let q = s.active_sequence().ok_or(crate::EngineError::NoSequence)?.clone();
    let frame = (q.settings.width.max(1), q.settings.height.max(1));
    let (fw, fh) = (f64::from(frame.0), f64::from(frame.1));
    let start = clips.iter().map(|c| c.1.start).min().unwrap_or(Tick::ZERO);
    let span = clips.iter().map(|c| c.1.duration).min().unwrap_or(Tick::ZERO);
    let base = clips.first().map_or(0, |c| c.0);

    // where every clip goes: picture k on the first free video track from the first clip's up,
    // its linked sound on free audio tracks, all from `start`
    let video_ids: Vec<ClipId> = clips.iter().map(|c| c.1.id).collect();
    let partners: Vec<(ClipId, Vec<(usize, TrackItem)>)> = clips
        .iter()
        .map(|(_, it, _)| {
            let linked = it.link.map_or_else(Vec::new, |l| {
                q.audio_tracks.iter().enumerate().flat_map(|(ai, t)| t.items.iter().filter(move |a| a.link == Some(l)).map(move |a| (ai, a.clone()))).collect()
            });
            (it.id, linked)
        })
        .collect();
    let moving: Vec<ClipId> = video_ids.iter().copied().chain(partners.iter().flat_map(|(_, v)| v.iter().map(|(_, a)| a.id))).collect();
    let mut v_taken = Vec::new();
    let mut a_taken = Vec::new();
    let mut plan: Vec<(ClipId, TrackKind, usize, Tick)> = Vec::new();
    for (_, it, _) in &clips {
        let vt = free_track(&q.video_tracks, base, start, it.duration, &moving, &v_taken);
        v_taken.push(vt);
        plan.push((it.id, TrackKind::Video, vt, start));
        for (ai, a) in partners.iter().find(|p| p.0 == it.id).map(|p| p.1.as_slice()).unwrap_or(&[]) {
            let at = start.saturating_add(a.start.saturating_sub(it.start).max(Tick::ZERO));
            let t = free_track(&q.audio_tracks, *ai, at, a.duration, &moving, &a_taken);
            a_taken.push(t);
            plan.push((a.id, TrackKind::Audio, t, at));
        }
    }
    let need_v = v_taken.iter().max().map_or(0, |m| m + 1).saturating_sub(q.video_tracks.len());
    let need_a = a_taken.iter().max().map_or(0, |m| m + 1).saturating_sub(q.audio_tracks.len());
    if q.video_tracks.len() + need_v > crate::sequence_tools::MAX_TRACKS || q.audio_tracks.len() + need_a > crate::sequence_tools::MAX_TRACKS {
        return Err(bad(cmd, "the grid needs more tracks than a sequence can hold"));
    }

    // the cells: `size` % of the frame's short side for the whole grid
    let (cols, rows) = grid_shape(clips.len());
    let cell = size / 100.0 * fw.min(fh) / cols.max(rows) as f64;
    let inner = cell * (1.0 - gap / 100.0);
    let layout: Vec<(ClipId, (u32, u32), f64, f64)> = clips
        .iter()
        .enumerate()
        .map(|(k, (_, it, sz))| {
            let (col, row) = (k % cols, k / cols);
            let dx = (col as f64 + 0.5) * cell - cols as f64 * cell / 2.0;
            let dy = (row as f64 + 0.5) * cell - rows as f64 * cell / 2.0;
            (it.id, *sz, dx, dy)
        })
        .collect();

    let label = if turns == 0.0 { "Video Grid" } else { "Rotating Video Square" };
    let added = s.edit_sequence(label, |q: &mut Sequence, ctx, st| {
        let mut added = 0usize;
        for (kind, need) in [(TrackKind::Video, need_v), (TrackKind::Audio, need_a)] {
            for _ in 0..need {
                let tracks = q.tracks_mut(kind);
                let name = format!("{} {}", if kind == TrackKind::Video { "Video" } else { "Audio" }, tracks.len() + 1);
                let mut t = Track::new(TrackId(ctx.alloc()), kind, name);
                if kind == TrackKind::Audio {
                    t.channels = tracks.first().map_or(t.channels, |f| f.channels);
                }
                q.tracks_mut(kind).push(t);
                added += 1;
            }
        }
        let moves: Vec<(ClipId, TrackId, Tick)> = plan
            .iter()
            .map(|(c, kind, ti, at)| q.tracks(*kind).get(*ti).map(|t| (*c, t.id, *at)).ok_or(crate::EngineError::Other("a grid track is missing".into())))
            .collect::<Result<_>>()?;
        edit::move_items(q, &moves, false, ctx)?;
        for (id, sz, dx, dy) in &layout {
            let Some(it) = q.video_tracks.iter_mut().flat_map(|t| t.items.iter_mut()).find(|i| i.id == *id) else { continue };
            let (sw, sh) = (f64::from(sz.0), f64::from(sz.1));
            // cover the cell, keep a square window of the picture, rounded a little
            let k = (inner / sw).max(inner / sh);
            let keep = inner / k;
            put(it, crop(frame, *sz, (keep, keep), keep * 0.06, (3.0 / k).max(1.0), [1.0, 1.0, 1.0, 0.9])?);
            it.scale_to_frame = false;
            let turn = if turns == 0.0 || span <= Tick::ZERO { Vec::new() } else { ramp(it, start, start.saturating_add(span), 0.0, 360.0 * turns) };
            let m = motion(it, frame)?;
            set(m, "scale", ParamValue::Float(k * 100.0));
            set(m, "scale_width", ParamValue::Float(k * 100.0));
            // turn around the grid's centre: the anchor is that centre in the clip's own pixels
            set(m, "anchor", point(sw / 2.0 - dx / k, sh / 2.0 - dy / k));
            set(m, "position", point(fw / 2.0, fh / 2.0));
            if let Some(r) = m.params.get_mut("rotation") {
                r.value = ParamValue::Float(0.0);
                r.keyframes = turn;
            }
        }
        st.selection = video_ids.clone();
        Ok(added)
    })?;
    Ok(json!({"clips": clips.len(), "columns": cols, "rows": rows, "tracksAdded": added, "turns": turns}))
}

#[cfg(test)]
mod tests;
