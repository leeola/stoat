//! Renders a stoat fixture headlessly and checks what its frames show.
//!
//! The scene runs the `stoat` binary beside this test executable, built with
//! stoat's own fixture feature, over a scratch home on the software adapter.

use std::{
    collections::HashSet,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};
use stoatty_bin::{
    config,
    headless::{script, HeadlessSession, Scene, Snapshot},
};
use stoatty_render::render;

#[path = "support/scene.rs"]
mod support;

/// The wait for the frame that follows a few keys.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// The scene's editor font size, which also sizes the tab bar rows the golden
/// comparison leaves out.
const FONT_SIZE: u32 = 14;

/// The grid rows of the tab bar's band at the frame's top, which the golden
/// comparison leaves out.
///
/// The bar leads with the workspace name, which stoat draws from the clock.
/// The band draws the bar 7 px down, so the name's ink reaches into the second
/// row.
const TAB_BAR_ROWS: u32 = 2;

/// The largest channel difference at which a pixel still matches its golden.
///
/// It and [`MISMATCH_LIMIT`] absorb the anti-aliasing drift that a mesa update
/// brings to lavapipe.
const CHANNEL_TOLERANCE: u8 = 8;

/// The largest share of the compared pixels that fail to match in a frame that
/// still matches its golden.
///
/// A cursor drawn one row lower changes about 0.10 percent of the compared
/// pixels in the after frame. The limit sits at half that, so a cursor on the
/// wrong row fails. A mesa update that moves more pixels than the limit fails
/// too, and a reviewer then accepts its frames as the new goldens.
const MISMATCH_LIMIT: f64 = 0.0005;

/// Two cursor moves in the basic-diff fixture change the frame, the first
/// frame draws text over the background, and both frames match their goldens.
///
/// Both frames are written into the scratch home, which a failing check keeps
/// and names, so the frames are there to open. A frame that does not match its
/// golden is written beside it as `.png.new` instead.
#[test]
fn the_basic_diff_frame_changes_when_the_cursor_moves() {
    let home = support::scratch_home();
    let mut session =
        HeadlessSession::spawn(basic_diff_scene(home.path())).unwrap_or_else(|error| {
            panic!(
                "{error}. With no wgpu adapter, enter the devshell with `nix develop`, which \
                 registers lavapipe"
            )
        });

    session
        .wait_settled(support::SETTLE, support::STARTUP_TIMEOUT)
        .expect("the first frame settles; a stoat built without its fixture feature exits at once");
    let before = session.snapshot().expect("a snapshot");
    session
        .type_keys(&script::parse("jj").expect("a key script"))
        .expect("the keys type");
    session
        .wait_settled(support::SETTLE, STEP_TIMEOUT)
        .expect("the frame settles after the keys");
    let after = session.snapshot().expect("a snapshot");
    // The child goes before its scratch home does.
    drop(session);

    support::save(&before, &home.path().join("before.png"));
    support::save(&after, &home.path().join("after.png"));
    let changed = before.rgba != after.rgba;
    let colors = distinct_colors(&before);
    let frames = match changed && colors > 2 {
        true => home.path().to_owned(),
        false => home.keep(),
    };
    assert!(
        changed,
        "two cursor moves change the frame; the frames are in {}",
        frames.display(),
    );
    assert!(
        colors > 2,
        "the first frame draws text over the background, not {colors} colors; the frames are \
         in {}",
        frames.display(),
    );

    let goldens = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goldens");
    let mismatches: Vec<String> = [
        (&before, "basic_diff_before.png"),
        (&after, "basic_diff_after.png"),
    ]
    .into_iter()
    .filter_map(|(shot, name)| golden_mismatch(shot, &goldens.join(name)))
    .collect();
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

/// Why `shot` does not match the golden PNG at `golden`, or `None` when it
/// does.
///
/// The comparison leaves out the top [`TAB_BAR_ROWS`] grid rows, the tab bar's
/// band, because the bar leads with the workspace name, which stoat draws from
/// the clock. A pixel matches when no channel differs by more than
/// [`CHANNEL_TOLERANCE`], and the frame matches when at most
/// [`MISMATCH_LIMIT`] of the compared pixels do not. A missing golden or a
/// frame that does not match writes the whole frame to `<golden>.new`, which a
/// reviewer moves over the golden to accept it.
fn golden_mismatch(shot: &Snapshot, golden: &Path) -> Option<String> {
    let fresh = {
        let mut fresh = OsString::from(golden.as_os_str());
        fresh.push(".new");
        PathBuf::from(fresh)
    };

    if !golden.is_file() {
        support::save(shot, &fresh);
        return Some(format!(
            "no golden at {}; review {} and move it over",
            golden.display(),
            fresh.display(),
        ));
    }

    let expected = image::open(golden).expect("decode the golden").to_rgba8();
    if expected.dimensions() != (shot.width, shot.height) {
        support::save(shot, &fresh);
        return Some(format!(
            "{} is {:?} and the frame is {:?}; review {} and move it over",
            golden.display(),
            expected.dimensions(),
            (shot.width, shot.height),
            fresh.display(),
        ));
    }

    let tab_bar =
        (shot.width * TAB_BAR_ROWS * render::cell_size(FONT_SIZE, 1.0)[1] as u32) as usize;
    let compared = (shot.width * shot.height) as usize - tab_bar;
    let differing = expected
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(shot.rgba.as_chunks::<4>().0)
        .skip(tab_bar)
        .filter(|(want, got)| {
            want.iter()
                .zip(got.iter())
                .any(|(want, got)| want.abs_diff(*got) > CHANNEL_TOLERANCE)
        })
        .count();
    if differing as f64 <= compared as f64 * MISMATCH_LIMIT {
        return None;
    }

    support::save(shot, &fresh);
    Some(format!(
        "{differing} of {compared} pixels below the tab bar differ from {} by more than \
         {CHANNEL_TOLERANCE}; review {} and move it over",
        golden.display(),
        fresh.display(),
    ))
}

/// The basic-diff fixture with its unstaged file open, at 100 by 30 cells on the
/// software adapter, with config, data, and state under `home`.
///
/// The file is named because a fixture opens no file of its own, and `jj` moves
/// nothing in the one-line scratch buffer stoat opens then.
fn basic_diff_scene(home: &Path) -> Scene {
    let config = config::embedded_default();
    Scene {
        program: support::stoat_binary().to_string_lossy().into_owned(),
        args: ["--fixture", "basic-diff", "unstaged.txt"]
            .map(String::from)
            .to_vec(),
        working_directory: Some(home.to_owned()),
        stoat_dir: None,
        cols: 100,
        rows: 30,
        font_size: FONT_SIZE,
        terminal_font_size: 12,
        scale_factor: 1.0,
        theme: config.resolve_theme(),
        theme_name: config.theme,
        font_family: config.font_family,
        ligatures: config.ligatures,
        cursor_animation: config.cursor_animation,
        software: true,
        extra_env: support::scratch_env(home),
    }
}

/// How many distinct RGB colors `shot`'s pixels hold.
fn distinct_colors(shot: &Snapshot) -> usize {
    shot.rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect::<HashSet<_>>()
        .len()
}
