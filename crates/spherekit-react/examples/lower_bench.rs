//! Times the per-frame cost of lowering a large committed tree.
//!
//! A music player's playlist is a few thousand nodes and every one of them is
//! re-resolved on every frame. This measures where that time goes.

use std::time::Instant;

fn playlist(rows: usize) -> String {
    let mut children = String::new();
    for row in 0..rows {
        if row > 0 {
            children.push(',');
        }
        let base = row * 7 + 10;
        children.push_str(&format!(
            r##"{{"id":{base},"type":"view","props":{{"className":"track","pressable":true}},"children":[
                {{"id":{i1},"type":"text","props":{{"className":"track-index"}},"children":[{{"id":{i2},"type":"#text","text":"{row}","children":[]}}]}},
                {{"id":{i3},"type":"text","props":{{"className":"track-title"}},"children":[{{"id":{i4},"type":"#text","text":"Track {row}","children":[]}}]}},
                {{"id":{i5},"type":"text","props":{{"className":"track-album"}},"children":[{{"id":{i6},"type":"#text","text":"Album","children":[]}}]}}
            ]}}"##,
            i1 = base + 1, i2 = base + 2, i3 = base + 3,
            i4 = base + 4, i5 = base + 5, i6 = base + 6,
        ));
    }
    format!(
        r##"{{"revision":1,"children":[{{"id":1,"type":"view","props":{{"className":"app"}},"children":[{children}]}}]}}"##
    )
}

const SHEET: &str = r#"
:root { --bg: #0f1115; --muted: #8b93a5; }
.app { display: flex; flex-direction: column; flex: 1; background-color: var(--bg); }
.track { display: flex; flex-direction: row; align-items: center; gap: 10px; padding: 8px 10px; border-radius: 6px; cursor: pointer; }
.track:hover { background-color: #1e222b; }
.track.playing { background-color: #3f8f74; }
.track-index { width: 22px; font-size: 11px; color: var(--muted); flex-shrink: 0; }
.track-title { flex: 1; font-size: 13px; white-space: nowrap; }
.track-album { font-size: 11px; color: var(--muted); white-space: nowrap; flex-shrink: 0; }
"#;

fn main() {
    let rows: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(500);
    let frames: usize = 60;

    let json = playlist(rows);

    // The floor: no stylesheet at all, which takes the fast path in `lower`.
    let mut bare = spherekit_react::ReactHost::new();
    bare.commit_json(&json).expect("commit");
    let start = Instant::now();
    for _ in 0..frames {
        std::hint::black_box(bare.ui_element());
    }
    let bare_ms = start.elapsed().as_secs_f64() * 1000.0 / frames as f64;

    // The same sheet with the custom properties inlined, to price the
    // ancestor walk `var()` forces.
    let novars = SHEET
        .replace(":root { --bg: #0f1115; --muted: #8b93a5; }", "")
        .replace("var(--bg)", "#0f1115")
        .replace("var(--muted)", "#8b93a5");
    let mut plain = spherekit_react::ReactHost::new();
    plain.set_stylesheet(&novars).expect("stylesheet parses");
    plain.commit_json(&json).expect("commit");
    let start = Instant::now();
    for _ in 0..frames {
        std::hint::black_box(plain.ui_element());
    }
    let novars_ms = start.elapsed().as_secs_f64() * 1000.0 / frames as f64;

    let mut host = spherekit_react::ReactHost::new();
    host.set_stylesheet(SHEET).expect("stylesheet parses");
    host.commit_json(&json).expect("commit");
    println!("rows: {rows}, nodes: {}", host.node_count());
    println!("lower, no stylesheet {bare_ms:6.2} ms/frame   <- the floor");
    println!("lower, no var()     {novars_ms:6.2} ms/frame");

    // Lowering only.
    let start = Instant::now();
    for _ in 0..frames {
        std::hint::black_box(host.ui_element());
    }
    let lower_ms = start.elapsed().as_secs_f64() * 1000.0 / frames as f64;

    // Lowering plus reconcile.
    let mut tree = spherekit_ui::UiTree::new();
    let start = Instant::now();
    for _ in 0..frames {
        tree.build(host.ui_element());
    }
    let build_ms = start.elapsed().as_secs_f64() * 1000.0 / frames as f64;

    // Lowering, reconcile and layout.
    let viewport = spherekit_core::size(spherekit_core::px(1040.0), spherekit_core::px(680.0));
    let start = Instant::now();
    for _ in 0..frames {
        tree.build(host.ui_element());
        tree.compute_layout(viewport).expect("layout");
        tree.end_frame();
    }
    let full_ms = start.elapsed().as_secs_f64() * 1000.0 / frames as f64;

    println!("lower            {lower_ms:7.2} ms/frame");
    println!("lower + build    {build_ms:7.2} ms/frame  (+{:.2})", build_ms - lower_ms);
    println!("lower + build + layout {full_ms:5.2} ms/frame  (+{:.2})", full_ms - build_ms);
    println!("=> budget at 60 fps is 16.67 ms");
}
