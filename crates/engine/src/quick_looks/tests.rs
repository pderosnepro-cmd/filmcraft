use serde_json::json;

use crate::Session;
use filmcraft_project::{ClipId, TrackKind};

fn demo() -> Session {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    s
}

/// The ids of the first `n` clips of V1.
fn v1(s: &Session, n: usize) -> Vec<u64> {
    s.active_sequence().unwrap().video_tracks[0].items.iter().take(n).map(|i| i.id.0).collect()
}

/// Mean brightness of the pixels of `img` in the box (fractions of the frame).
fn mean(img: &filmcraft_render::Image, x0: f64, y0: f64, x1: f64, y1: f64) -> f32 {
    let (w, h) = (img.w as f64, img.h as f64);
    let (mut sum, mut n) = (0.0f32, 0usize);
    for y in (y0 * h) as usize..((y1 * h) as usize).min(img.h) {
        for x in (x0 * w) as usize..((x1 * w) as usize).min(img.w) {
            let p = img.get(x, y);
            sum += (p[0] + p[1] + p[2]) / 3.0;
            n += 1;
        }
    }
    sum / n.max(1) as f32
}

/// A frame of the active sequence at `seconds`, at 1/8 size.
fn frame(s: &mut Session, seconds: f64) -> filmcraft_render::Image {
    s.execute("playhead.set", json!({"seconds": seconds})).unwrap();
    s.render_program(0.125).unwrap()
}

#[test]
fn quick_looks_are_in_the_clip_menu() {
    let ids: Vec<&str> = crate::commands::command_specs().iter().filter(|c| c.menu == ["Clip", "Quick Looks"]).map(|c| c.id).collect();
    for id in ["clip.quickLook.bubble", "clip.quickLook.pictureInPicture", "clip.quickLook.spinCard", "clip.quickLook.grid", "clip.quickLook.rotatingGrid"] {
        assert!(ids.contains(&id), "{id} in {ids:?}");
    }
}

/// The bubble is a lit disc in the middle of the frame: the corners show what is under it
/// (nothing here: black), one undo step takes it all back.
#[test]
fn a_bubble_is_a_round_picture_in_the_middle() {
    let mut s = demo();
    // the dunes: bright from top to bottom
    let clip = v1(&s, 4)[3];
    let before = frame(&mut s, 15.0);
    let undo = s.history.undo.len();
    let r = s.execute("clip.quickLook.bubble", json!({"clips": [clip], "float": false})).unwrap();
    assert_eq!(r["clips"], 1, "{r}");
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    let img = frame(&mut s, 15.0);
    let (centre, corner) = (mean(&img, 0.45, 0.45, 0.55, 0.55), mean(&img, 0.0, 0.0, 0.08, 0.12));
    assert!(centre > 0.05, "the picture is in the middle ({centre})");
    assert!(corner < 0.02 && mean(&before, 0.0, 0.0, 0.08, 0.12) > corner, "the corners are outside the bubble ({corner})");
    // round, not square: the bubble's bounding box corners are empty too
    assert!(mean(&img, 0.31, 0.16, 0.33, 0.20) < centre / 2.0, "a disc, not a square");
    s.execute("edit.undo", json!({})).unwrap();
    let back = frame(&mut s, 15.0);
    assert!((mean(&back, 0.0, 0.0, 0.08, 0.12) - mean(&before, 0.0, 0.0, 0.08, 0.12)).abs() < 1e-4, "undo restores the clip");
}

/// Applying a look twice adjusts it instead of stacking a second copy of each effect.
#[test]
fn a_look_applied_twice_is_not_stacked() {
    let mut s = demo();
    let clip = v1(&s, 1)[0];
    s.execute("clip.quickLook.bubble", json!({"clips": [clip]})).unwrap();
    s.execute("clip.quickLook.bubble", json!({"clips": [clip], "size": 50})).unwrap();
    let q = s.active_sequence().unwrap();
    let (_, it) = q.find_item(ClipId(clip)).unwrap();
    for e in ["rounded_crop", "spherize", "wiggle"] {
        assert_eq!(it.effects.iter().filter(|x| x.effect == e).count(), 1, "{e}");
    }
}

/// Picture in Picture puts a smaller picture in the asked corner.
#[test]
fn picture_in_picture_goes_to_its_corner() {
    let mut s = demo();
    let clip = v1(&s, 4)[3];
    s.execute("clip.quickLook.pictureInPicture", json!({"clips": [clip], "corner": "topLeft", "size": 30})).unwrap();
    let img = frame(&mut s, 15.0);
    let (tl, br) = (mean(&img, 0.05, 0.08, 0.25, 0.25), mean(&img, 0.75, 0.75, 0.95, 0.92));
    assert!(tl > 0.05 && br < 0.02, "top left {tl}, bottom right {br}");
}

/// The spinning card is animated: the same clip looks different a quarter of the way through.
#[test]
fn a_spinning_card_turns_over_the_clip() {
    let mut s = demo();
    let clip = v1(&s, 3)[2];
    let q = s.active_sequence().unwrap();
    let (_, it) = q.find_item(ClipId(clip)).unwrap();
    let (start, dur) = (it.start.seconds(), it.duration.seconds());
    s.execute("clip.quickLook.spinCard", json!({"clips": [clip]})).unwrap();
    let at = |s: &mut Session, f: f64| mean(&frame(s, start + dur * f), 0.0, 0.0, 1.0, 1.0);
    let (a, b) = (at(&mut s, 0.05), at(&mut s, 0.25));
    assert!(b < a * 0.6, "edge-on at a quarter turn: {b} vs {a}");
}

/// Four clips of V1 go side by side in a still grid: stacked on four tracks from the first
/// clip's start, with their sound, and every quarter of the grid shows a picture.
#[test]
fn a_grid_stacks_the_clips_and_fills_its_cells() {
    let mut s = demo();
    let ids = v1(&s, 4);
    let start = s.active_sequence().unwrap().video_tracks[0].items[0].start;
    let undo = s.history.undo.len();
    let r = s.execute("clip.quickLook.grid", json!({"clips": ids, "size": 90})).unwrap();
    assert_eq!((r["columns"].as_u64(), r["rows"].as_u64()), (Some(2), Some(2)), "{r}");
    assert_eq!(s.history.undo.len(), undo + 1, "one undo step");
    let q = s.active_sequence().unwrap();
    q.check().unwrap();
    let placed: Vec<_> = ids.iter().map(|c| q.video_tracks.iter().position(|t| t.item(ClipId(*c)).is_some()).unwrap()).collect();
    let mut distinct = placed.clone();
    distinct.dedup();
    assert_eq!(distinct.len(), 4, "one track each: {placed:?}");
    for c in &ids {
        let (_, it) = q.find_item(ClipId(*c)).unwrap();
        assert_eq!(it.start, start, "all from the first clip's start");
        // the linked sound came along
        if let Some(l) = it.link {
            let a = q.audio_tracks.iter().flat_map(|t| &t.items).find(|a| a.link == Some(l)).unwrap();
            assert_eq!(a.start, it.start, "its sound with it");
        }
    }
    // after the first clip's opening fade
    let img = frame(&mut s, start.seconds() + 1.5);
    for (x, y) in [(0.3, 0.3), (0.7, 0.3), (0.3, 0.7), (0.7, 0.7)] {
        let m = mean(&img, x - 0.05, y - 0.05, x + 0.05, y + 0.05);
        assert!(m > 0.01, "cell at {x},{y}: {m}");
    }
}

/// The rotating square turns: an eighth of a turn later its corners have moved, and the
/// frame's corners stay outside it.
#[test]
fn a_rotating_square_turns_around_its_centre() {
    let mut s = demo();
    let ids = v1(&s, 4);
    let start = s.active_sequence().unwrap().video_tracks[0].items[0].start.seconds();
    s.execute("clip.quickLook.rotatingGrid", json!({"clips": ids, "turns": 1})).unwrap();
    let q = s.active_sequence().unwrap();
    let span = ids.iter().map(|c| q.find_item(ClipId(*c)).unwrap().1.duration.seconds()).fold(f64::MAX, f64::min);
    // a quarter turn brings the square back square; an eighth turns it into a diamond
    let still = frame(&mut s, start + span / 4.0);
    let turned = frame(&mut s, start + span / 8.0);
    // near the square's bottom-right corner: inside the square, outside the diamond
    let corner = |img: &filmcraft_render::Image| mean(img, 0.63, 0.77, 0.67, 0.83);
    assert!(corner(&still) > corner(&turned) + 0.02, "the square's corner moved: {} -> {}", corner(&still), corner(&turned));
    assert!(mean(&turned, 0.0, 0.0, 0.05, 0.05) < 0.02, "the frame's corner is outside the square");
    // the cells turn as one square (the clips are cut from different points of their sources):
    // a quarter turn later every cell is back in a cell of the square, around its centre
    for (x, y) in [(0.41, 0.33), (0.59, 0.33), (0.41, 0.67), (0.59, 0.67)] {
        let m = mean(&still, x - 0.03, y - 0.04, x + 0.03, y + 0.04);
        assert!(m > 0.01, "cell at {x},{y} after a quarter turn: {m}");
    }
    // and the gaps between them stay in place: a quarter turn of a rigid square maps its
    // middle cross onto itself
    assert!(mean(&still, 0.495, 0.2, 0.505, 0.8) < mean(&still, 0.40, 0.2, 0.45, 0.8), "the gap between the columns");
}

/// Hostile or impossible parameters are refused before anything changes.
#[test]
fn quick_looks_refuse_bad_parameters() {
    let mut s = demo();
    let ids = v1(&s, 4);
    let undo = s.history.undo.len();
    for (cmd, p) in [
        ("clip.quickLook.bubble", json!({"clips": [ids[0]], "size": -5})),
        ("clip.quickLook.bubble", json!({"clips": [ids[0]], "size": "big"})),
        ("clip.quickLook.bubble", json!({"clips": [999999]})),
        ("clip.quickLook.pictureInPicture", json!({"clips": [ids[0]], "corner": "middle"})),
        ("clip.quickLook.spinCard", json!({"clips": [ids[0]], "turns": 1e300})),
        ("clip.quickLook.spinCard", json!({"clips": [ids[0]], "axis": "z"})),
        ("clip.quickLook.grid", json!({"clips": [ids[0]]})),
        ("clip.quickLook.grid", json!({"clips": ids, "gap": 90})),
        ("clip.quickLook.rotatingGrid", json!({"clips": ids, "turns": "fast"})),
    ] {
        assert!(s.execute(cmd, p.clone()).is_err(), "{cmd} {p}");
    }
    assert_eq!(s.history.undo.len(), undo, "nothing changed");
    // audio-only selections have no picture to lay out
    let a = s.active_sequence().unwrap().tracks(TrackKind::Audio)[0].items[0].id.0;
    assert!(s.execute("clip.quickLook.bubble", json!({"clips": [a]})).is_err());
}
