//! Opens two windows and prints what the platform layer reports.
//!
//! Run with `cargo run -p sphere-platform --example window_smoke`.
//!
//! This is the manual counterpart to the unit tests: it exercises the parts
//! that cannot run headless — real window creation, real DPI, real input — and
//! demonstrates the idle behaviour the frame scheduler exists for. Nothing is
//! drawn: there is no renderer at this layer.
//!
//! Things worth watching while it runs:
//!
//! * Move the mouse: positions are printed in **logical** pixels. Drag the
//!   window to a display with a different scale factor and the numbers stay in
//!   the same coordinate system.
//! * Hold <kbd>Space</kbd>: the window switches to realtime redraw and the
//!   frame counter climbs. Release it and the loop goes back to sleep — check
//!   the process's CPU usage before and after.
//! * Press <kbd>N</kbd> to open another window; each has its own size, scale
//!   factor and focus state.
//! * Press <kbd>C</kbd> to round-trip the clipboard.

use std::time::Instant;

use sphere_core::{px, size};
use sphere_platform::prelude::*;
use sphere_platform::{App, AppContext, AppHandler, ElementState, Key, NamedKey, WindowId};

struct Smoke {
    /// Frames drawn since start-up, to make the redraw policy visible.
    frames: u64,
    /// When the last report was printed.
    last_report: Instant,
}

impl Smoke {
    fn new() -> Self {
        Self { frames: 0, last_report: Instant::now() }
    }

    fn open_window(&mut self, cx: &mut AppContext<'_>, title: &str) {
        let attrs = WindowAttributes::new(title)
            .with_inner_size(size(px(720.0), px(480.0)))
            .with_min_inner_size(size(px(320.0), px(240.0)));
        match cx.create_window(&attrs) {
            Ok(window) => {
                println!(
                    "opened {:?}: {:?} physical, {:?} logical, scale {}",
                    window.id(),
                    window.physical_size(),
                    window.inner_size(),
                    window.scale_factor().get()
                );
                window.set_cursor(Cursor::Default);
            }
            Err(e) => eprintln!("failed to open a window: {e}"),
        }
    }
}

impl AppHandler for Smoke {
    fn resumed(&mut self, cx: &mut AppContext<'_>) {
        for monitor in cx.monitors().iter() {
            println!(
                "monitor {:?}: {:?} at {:?}, scale {}, {}",
                monitor.name.as_deref().unwrap_or("<unnamed>"),
                monitor.size,
                monitor.position,
                monitor.scale_factor.get(),
                monitor
                    .refresh_rate
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "unknown rate".into()),
            );
        }
        // Pace animation against the fastest attached display rather than
        // assuming 60 Hz.
        if let Some(rate) = cx.monitors().highest_refresh_rate() {
            cx.scheduler_mut().set_refresh_rate(rate);
            println!("pacing at {rate}");
        }
        self.open_window(cx, "Sphere platform smoke test");
    }

    fn window_event(&mut self, cx: &mut AppContext<'_>, id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                cx.close_window(id);
                if cx.windows().is_empty() {
                    cx.exit();
                }
            }
            WindowEvent::RedrawRequested => {
                self.frames += 1;
                if self.last_report.elapsed().as_secs_f32() >= 1.0 {
                    println!("{} frames total", self.frames);
                    self.last_report = Instant::now();
                }
            }
            WindowEvent::Resized(size) => {
                let logical = cx.windows().state(id).map(|s| s.logical_size());
                println!("{id:?} resized to {size:?} physical / {logical:?} logical");
            }
            WindowEvent::ScaleFactorChanged(scale) => {
                println!("{id:?} scale factor is now {}", scale.get());
            }
            WindowEvent::CursorMoved(p) => {
                // Logical pixels: the same numbers at 100 % and 200 %.
                print!("\r{id:?} pointer {:>7.1}, {:>7.1}   ", p.x.get(), p.y.get());
            }
            WindowEvent::KeyboardInput { key, state, repeat, modifiers, .. } => {
                if repeat {
                    return;
                }
                match (&key, state) {
                    (Key::Named(NamedKey::Escape), ElementState::Pressed) => cx.exit(),
                    (Key::Named(NamedKey::Space), ElementState::Pressed) => {
                        cx.scheduler_mut().begin_realtime();
                        println!("\nrealtime redraw on");
                    }
                    (Key::Named(NamedKey::Space), ElementState::Released) => {
                        cx.scheduler_mut().end_realtime();
                        println!("realtime redraw off, going idle");
                    }
                    (Key::Character(c), ElementState::Pressed) if c == "n" => {
                        self.open_window(cx, "Another window");
                    }
                    (Key::Character(c), ElementState::Pressed) if c == "c" => {
                        let clipboard = cx.clipboard();
                        match clipboard.set_text("sphere-platform smoke test") {
                            Ok(()) => println!("\nclipboard now: {:?}", clipboard.get_text()),
                            Err(e) => println!("\nclipboard unavailable: {e}"),
                        }
                    }
                    (key, ElementState::Pressed) if modifiers.command() => {
                        println!("\naccelerator: command+{key}");
                    }
                    _ => {}
                }
            }
            WindowEvent::TextInput(text) => println!("\ntext: {text:?}"),
            WindowEvent::Focused(focused) => println!("\n{id:?} focused: {focused}"),
            WindowEvent::Occluded(occluded) => println!("\n{id:?} occluded: {occluded}"),
            WindowEvent::Destroyed => println!("\n{id:?} destroyed"),
            _ => {}
        }
    }

    fn exiting(&mut self, _cx: &mut AppContext<'_>) {
        println!("\ndrew {} frames", self.frames);
    }
}

fn main() {
    if let Err(e) = App::new(Smoke::new()).run() {
        eprintln!("platform error: {e}");
        std::process::exit(1);
    }
}
