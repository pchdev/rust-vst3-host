//! Driving a plugin's editor on the UI thread while the plugin is processed elsewhere.
//!
//! VST3 keeps a plugin's editor and its processor apart: the editor runs on the UI thread
//! while the processor runs on the audio thread, at the same time. A [`Plugin`] can only be
//! used from one place at a time, so once it is processing on the audio thread — owned by a
//! [`RealtimePluginRunner`](crate::RealtimePluginRunner), say — its own editor methods are out
//! of reach. An [`EditorHandle`] taken beforehand keeps the editor reachable:
//!
//! ```no_run
//! use vst3_host::{simple, PluginWindow, RealtimePluginRunner};
//! # fn main() -> vst3_host::Result<()> {
//! let plugin = simple::load_plugin("/path/synth.vst3")?;
//! let editor = plugin.editor_handle()?;
//! let (mut runner, _control) = RealtimePluginRunner::new(plugin, 1024);
//! runner.start()?;
//! // ... hand `runner` to the audio thread ...
//!
//! let mut window = PluginWindow::for_editor(editor);
//! window.open()?;
//! while window.is_open() {
//!     window.service_platform_events()?; // also services the editor, every UI frame
//!     std::thread::sleep(std::time::Duration::from_millis(16));
//! }
//! # Ok(())
//! # }
//! ```

use crate::error::{Error, Result};
use crate::internal::editor_state::EditorState;
use crate::plugin::WindowHandle;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;

/// A plugin's editor (its GUI), usable independently from the [`Plugin`](crate::Plugin) it was
/// taken from with [`Plugin::editor_handle`](crate::Plugin::editor_handle).
///
/// Like the plugin's own editor methods, it must be used on the thread that loaded the plugin:
/// the calls that reach the editor check this and fail anywhere else.
///
/// Clones share the same editor: opening it through one shows it open through all.
///
/// The handle never keeps the plugin alive. When the plugin is destroyed it closes its editor
/// first, and from then on [`is_alive`](Self::is_alive) is false and every call fails (or does
/// nothing) instead of reaching into an unloaded module.
#[derive(Clone)]
pub struct EditorHandle {
    state: Arc<Mutex<EditorState>>,
    plugin_name: String,
}

impl EditorHandle {
    pub(crate) fn new(state: Arc<Mutex<EditorState>>, plugin_name: String) -> Self {
        EditorHandle { state, plugin_name }
    }

    /// Name of the plugin this editor belongs to.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// The shared state, recovering a lock poisoned by an unrelated panic.
    fn state(&self) -> MutexGuard<'_, EditorState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The state, if this is the plugin's control thread and the plugin is still alive.
    fn checked_state(&self) -> Result<MutexGuard<'_, EditorState>> {
        let state = self.state();
        if state.is_released() {
            return Err(Error::Other("the plugin was destroyed".to_string()));
        }
        if thread::current().id() != state.control_thread() {
            return Err(Error::Other(
                "a plugin editor must be used on the thread that loaded the plugin".to_string(),
            ));
        }
        Ok(state)
    }

    /// Whether the plugin this editor belongs to still exists.
    pub fn is_alive(&self) -> bool {
        !self.state().is_released()
    }

    /// Whether the plugin has an editor. May create and release a probe view.
    pub fn has_editor(&self) -> bool {
        self.checked_state().is_ok_and(|state| state.has_editor())
    }

    /// Open the editor, embedded in `parent`. See [`Plugin::open_editor`](crate::Plugin::open_editor).
    pub fn open(&self, parent: WindowHandle) -> Result<()> {
        self.checked_state()?.open(parent.0)
    }

    /// Close the editor. A no-op when it isn't open, or once the plugin was destroyed (which
    /// closed it already).
    pub fn close(&self) -> Result<()> {
        let mut state = self.state();
        if state.is_released() {
            return Ok(());
        }
        state.close()
    }

    /// Whether the editor is attached to a window.
    pub fn is_open(&self) -> bool {
        self.state().is_open()
    }

    /// The editor's size: the attached view's, or else the preferred one (which may create and
    /// release a probe view).
    pub fn size(&self) -> Result<(i32, i32)> {
        self.checked_state()?.size()
    }

    /// Whether the editor can be resized by the host.
    pub fn can_resize(&self) -> bool {
        self.checked_state().is_ok_and(|state| state.can_resize())
    }

    /// Resize the attached editor. See [`Plugin::resize_editor`](crate::Plugin::resize_editor).
    pub fn resize(&self, width: i32, height: i32) -> Result<(i32, i32)> {
        self.checked_state()?.resize(width, height)
    }

    /// See [`Plugin::set_editor_scale_factor`](crate::Plugin::set_editor_scale_factor).
    pub fn set_scale_factor(&self, factor: f32) -> Result<bool> {
        self.checked_state()?.set_scale_factor(factor)
    }

    /// A size the editor asked its window to take, if any since the last call.
    pub fn take_resize_request(&self) -> Option<(i32, i32)> {
        self.state().take_resize_request()
    }

    /// Call every UI frame (30–60 Hz) while the editor is open. On Linux it services the run
    /// loop that VSTGUI-style editors only paint and respond from. It also applies parameter
    /// values the host changed through the processor since (e.g. from a
    /// [`RtControl`](crate::RtControl)), so the editor shows what the plugin plays.
    pub fn service(&self) {
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let Ok(mut state) = self.checked_state() else {
            return;
        };
        state.apply_deferred_controller_sync();
        #[cfg(target_os = "linux")]
        state.service_run_loop();
    }
}

impl std::fmt::Debug for EditorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditorHandle")
            .field("plugin", &self.plugin_name)
            .field("alive", &self.is_alive())
            .field("open", &self.is_open())
            .finish()
    }
}
