//! Saving a plugin's state while the plugin is processed elsewhere.
//!
//! Hosts save state (`IComponent::getState`, `IEditController::getState`) from the UI thread
//! while the audio thread keeps processing, for autosaves or presets. A [`Plugin`] can only be
//! used from one place at a time, so once it is processing on the audio thread — owned by a
//! [`RealtimePluginRunner`](crate::RealtimePluginRunner), say — [`Plugin::save_state`] is out
//! of reach. A [`StateHandle`] taken beforehand keeps it reachable:
//!
//! ```no_run
//! use vst3_host::{simple, RealtimePluginRunner};
//! # fn main() -> vst3_host::Result<()> {
//! let plugin = simple::load_plugin("/path/synth.vst3")?;
//! let state = plugin.state_handle()?;
//! let (mut runner, _control) = RealtimePluginRunner::new(plugin, 1024);
//! runner.start()?;
//! // ... hand `runner` to the audio thread ...
//!
//! let preset = state.save_preset()?; // while it plays
//! # Ok(())
//! # }
//! ```
//!
//! Restoring state isn't offered here: `Plugin::load_state` stops and deactivates the plugin
//! around `setState`, which can't overlap with processing.
//!
//! [`Plugin`]: crate::Plugin
//! [`Plugin::save_state`]: crate::Plugin::save_state

use crate::error::{Error, Result};
use crate::internal::controller_state::ControllerState;
use crate::plugin::{PluginInfo, PluginPreset};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

/// Saves the state of the [`Plugin`](crate::Plugin) it was taken from with
/// [`Plugin::state_handle`](crate::Plugin::state_handle), wherever the plugin is.
///
/// Must be used on the thread that loaded the plugin. Like an
/// [`EditorHandle`](crate::EditorHandle), it never keeps the plugin alive: once the plugin is
/// destroyed, [`is_alive`](Self::is_alive) is false and saving fails.
#[derive(Clone)]
pub struct StateHandle {
    state: Arc<Mutex<ControllerState>>,
    info: PluginInfo,
}

impl StateHandle {
    pub(crate) fn new(state: Arc<Mutex<ControllerState>>, info: PluginInfo) -> Self {
        StateHandle { state, info }
    }

    /// The state, if this is the plugin's control thread and the plugin is still alive.
    fn checked_state(&self) -> Result<MutexGuard<'_, ControllerState>> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.is_released() {
            return Err(Error::Other("the plugin was destroyed".to_string()));
        }
        if thread::current().id() != state.control_thread() {
            return Err(Error::Other(
                "a plugin's state must be saved on the thread that loaded it".to_string(),
            ));
        }
        Ok(state)
    }

    /// Information about the plugin this handle saves.
    pub fn plugin_info(&self) -> &PluginInfo {
        &self.info
    }

    /// Whether the plugin still exists.
    pub fn is_alive(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_released()
    }

    /// The plugin's state, as [`Plugin::save_state`](crate::Plugin::save_state) returns it,
    /// and restored by [`Plugin::load_state`](crate::Plugin::load_state).
    pub fn save_state(&self) -> Result<Vec<u8>> {
        self.checked_state()?.save_state()
    }

    /// The plugin's state with its `uid` and name, as
    /// [`Plugin::save_preset`](crate::Plugin::save_preset) writes it to a file.
    pub fn save_preset(&self) -> Result<PluginPreset> {
        Ok(PluginPreset {
            uid: self.info.uid.clone(),
            plugin_name: self.info.name.clone(),
            state: self.save_state()?,
        })
    }
}

impl std::fmt::Debug for StateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateHandle")
            .field("plugin", &self.info.name)
            .field("alive", &self.is_alive())
            .finish()
    }
}
