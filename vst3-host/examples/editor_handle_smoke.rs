//! Smoke check for `EditorHandle`: a plugin's editor driven on the main thread while the plugin
//! itself is owned by a `RealtimePluginRunner` processing on another thread, which is what a
//! real-time host does. Exits `0` on success, `1` on failure.
//!
//! ```bash
//! cargo run -p vst3-host --example editor_handle_smoke                    # test_plugins/Dexed.vst3
//! cargo run -p vst3-host --example editor_handle_smoke -- /path/to/Some.vst3
//! ```
//!
//! It checks that:
//!
//! - the editor opens and stays up while audio blocks keep being processed concurrently, and
//!   reports the longest block, which the editor no longer competes with for a lock;
//! - once the plugin is destroyed (through the runner's teardown, on this thread), a remaining
//!   handle reports it and refuses to open, instead of reaching into the unloaded module.
//!
//! Like `editor_smoke`, an example rather than a `#[test]`: AppKit only creates windows on the
//! process main thread, which libtest never hands to a test.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vst3_host::prelude::*;
use vst3_host::{EditorHandle, RealtimePluginRunner};

/// Frames of editor servicing to run while the editor is up, at roughly 60 Hz.
const FRAMES: u32 = 30;
const SAMPLE_RATE: f64 = 48000.0;
const BLOCK: usize = 256;

fn main() {
    env_logger::init();
    std::process::exit(match run() {
        Ok(()) => {
            println!("EDITOR HANDLE SMOKE OK");
            0
        }
        Err(message) => {
            eprintln!("EDITOR HANDLE SMOKE FAILED: {message}");
            1
        }
    });
}

fn run() -> Result<(), String> {
    let path = std::env::args().nth(1).unwrap_or_else(default_plugin_path);
    println!("=== editor handle smoke: {path} ===");
    if !std::path::Path::new(&path).exists() {
        return Err(format!("plugin not found at {path}"));
    }

    let mut host = Vst3Host::builder()
        .sample_rate(SAMPLE_RATE)
        .block_size(BLOCK)
        .build()
        .map_err(|e| format!("build host: {e}"))?;
    let plugin = host
        .load_plugin(&path)
        .map_err(|e| format!("load {path}: {e}"))?;
    println!("load: {}", plugin.info().name);
    let layout = plugin
        .audio_bus_layout()
        .map_err(|e| format!("bus layout: {e}"))?;
    let channels = |buses: &[AudioBusConfig]| buses.first().map_or(0, |bus| bus.channel_count);
    let (inputs, outputs) = (channels(&layout.inputs), channels(&layout.outputs));

    // Taken while the plugin is still here: one for the window, one to check teardown with.
    let editor = plugin
        .editor_handle()
        .map_err(|e| format!("editor handle: {e}"))?;
    let probe = plugin
        .editor_handle()
        .map_err(|e| format!("second editor handle: {e}"))?;
    if !editor.has_editor() {
        return Err("plugin reports no editor, nothing to smoke-test".to_string());
    }

    let (mut runner, mut control) = RealtimePluginRunner::new(plugin, 64);
    runner
        .start()
        .map_err(|e| format!("start processing: {e}"))?;

    // The audio side: process blocks back to back until told to stop.
    let stop = Arc::new(AtomicBool::new(false));
    let audio = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut buffers = AudioBuffers::new(inputs, outputs, BLOCK, SAMPLE_RATE);
            let (mut blocks, mut longest) = (0u32, Duration::ZERO);
            while !stop.load(Ordering::Relaxed) {
                let start = Instant::now();
                if runner.process(&mut buffers).is_ok() {
                    blocks += 1;
                }
                longest = longest.max(start.elapsed());
                std::thread::sleep(Duration::from_millis(2));
            }
            (runner, blocks, longest)
        })
    };

    let result = drive_editor(editor);
    stop.store(true, Ordering::Relaxed);
    let (mut runner, blocks, longest) = audio.join().map_err(|_| "audio thread panicked")?;
    result?;
    println!("audio: {blocks} blocks processed meanwhile, longest {longest:?}");
    if blocks == 0 {
        return Err("no audio block was processed while the editor was open".to_string());
    }

    // Destroy the plugin here, on its loading thread, through the runner's handoff.
    runner.stop().map_err(|e| format!("stop processing: {e}"))?;
    drop(runner);
    if !control.service_teardown() {
        return Err("the runner didn't hand the plugin back".to_string());
    }
    check_released(&probe)
}

fn drive_editor(editor: EditorHandle) -> Result<(), String> {
    let mut window = PluginWindow::for_editor(editor);
    window.open().map_err(|e| format!("open editor: {e}"))?;
    if !window.is_open() {
        return Err("editor reported closed right after open()".to_string());
    }
    println!("open: editor window is up while the runner processes");

    for _ in 0..FRAMES {
        // Services the window and, for an editor handle, the editor's run loop too.
        window
            .service_platform_events()
            .map_err(|e| format!("service editor window: {e}"))?;
        std::thread::sleep(Duration::from_millis(16));
    }
    if !window.is_open() {
        return Err("editor window went away while it was being serviced".to_string());
    }
    println!("service: survived {FRAMES} frames");

    window.close();
    if window.is_open() {
        return Err("window still open after close()".to_string());
    }
    println!("close: editor detached and window destroyed");
    Ok(())
}

fn check_released(probe: &EditorHandle) -> Result<(), String> {
    if probe.is_alive() {
        return Err("the editor handle still reports a live plugin after teardown".to_string());
    }
    // SAFETY: a released handle fails before touching the parent, which is never used.
    let parent = unsafe { WindowHandle::from_raw(std::ptr::NonNull::dangling().as_ptr()) };
    if probe.open(parent).is_ok() {
        return Err("a released editor handle opened an editor".to_string());
    }
    probe.service();
    println!("teardown: the remaining handle reports the plugin gone and refuses to open");
    Ok(())
}

fn default_plugin_path() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../test_plugins/Dexed.vst3").to_string()
}
