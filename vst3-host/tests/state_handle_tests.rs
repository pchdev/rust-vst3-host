//! `StateHandle`: saving a plugin's state while a `RealtimePluginRunner` processes it on
//! another thread, then restoring it through `RealtimePluginRunner::plugin_mut` once the runner
//! is back on the control thread.
//!
//! `#[ignore]`d like the other real-plugin tests. The plugin defaults to the bundled Dexed,
//! `VST3_HOST_STATE_TEST_PLUGIN` points it at another one:
//!
//! ```bash
//! cargo test -p vst3-host --test state_handle_tests -- --ignored --nocapture
//! VST3_HOST_STATE_TEST_PLUGIN=/path/Some.vst3 cargo test -p vst3-host --test state_handle_tests -- --ignored
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use vst3_host::{AudioBuffers, RealtimePluginRunner, Vst3Host};

const SAMPLE_RATE: f64 = 48000.0;
const BLOCK: usize = 256;

fn test_plugin() -> Option<PathBuf> {
    let path = std::env::var_os("VST3_HOST_STATE_TEST_PLUGIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../test_plugins/Dexed.vst3"
            ))
        });
    if path.exists() {
        Some(path)
    } else {
        println!("Test plugin not found at {}, skipping", path.display());
        None
    }
}

#[test]
#[ignore]
fn saves_while_processing_and_restores_offline() {
    let Some(path) = test_plugin() else { return };
    let mut host = Vst3Host::builder()
        .sample_rate(SAMPLE_RATE)
        .block_size(BLOCK)
        .build()
        .unwrap();
    let plugin = host.load_plugin(&path).unwrap();
    let layout = plugin.audio_bus_layout().unwrap();
    let inputs = layout.inputs.first().map_or(0, |bus| bus.channel_count);
    let outputs = layout.outputs.first().map_or(0, |bus| bus.channel_count);

    let state = plugin.state_handle().unwrap();
    assert_eq!(state.plugin_info().uid, plugin.info().uid);
    let initial = state.save_state().unwrap();
    assert!(!initial.is_empty());

    let (mut runner, mut control) = RealtimePluginRunner::new(plugin, 64);
    runner.start().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let audio = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut buffers = AudioBuffers::new(inputs, outputs, BLOCK, SAMPLE_RATE);
            let mut blocks = 0u32;
            while !stop.load(Ordering::Relaxed) {
                runner.process(&mut buffers).unwrap();
                blocks += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
            (runner, blocks)
        })
    };

    // Saving goes on while the other thread processes.
    for _ in 0..20 {
        let preset = state.save_preset().unwrap();
        assert_eq!(preset.uid, state.plugin_info().uid);
        assert!(!preset.state.is_empty());
        std::thread::sleep(Duration::from_millis(5));
    }
    stop.store(true, Ordering::Relaxed);
    let (mut runner, blocks) = audio.join().unwrap();
    assert!(blocks > 0, "the runner processed while saving");

    // Restoring needs the plugin to itself: the runner is back on this thread.
    runner.plugin_mut().load_state(&initial).unwrap();
    let mut buffers = AudioBuffers::new(inputs, outputs, BLOCK, SAMPLE_RATE);
    runner.process(&mut buffers).unwrap();

    runner.stop().unwrap();
    drop(runner);
    assert!(control.service_teardown(), "plugin handed back");
    assert!(!state.is_alive());
    assert!(state.save_state().is_err());
}

#[test]
#[ignore]
fn refuses_other_threads() {
    let Some(path) = test_plugin() else { return };
    let mut host = Vst3Host::new().unwrap();
    let plugin = host.load_plugin(&path).unwrap();
    let state = plugin.state_handle().unwrap();
    let elsewhere = state.clone();
    let result = std::thread::spawn(move || elsewhere.save_state().map(|_| ()))
        .join()
        .unwrap();
    assert!(result.is_err());
    assert!(state.save_state().is_ok());
}
