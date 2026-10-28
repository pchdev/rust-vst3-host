//! The UI-thread side of an in-process plugin: its `IEditController`, the attached `IPlugView`
//! and the host objects the view talks to, plus the component reference saving state needs.
//!
//! VST3 keeps the editor and the processor apart: the editor lives on the UI thread while the
//! processor runs on the audio thread, and hosts save state (`getState`) from the UI thread
//! while processing goes on. This state is therefore kept apart from the rest of
//! [`PluginImpl`](super::plugin_impl::PluginImpl), behind its own lock, so that an
//! [`EditorHandle`](crate::editor::EditorHandle) or a [`StateHandle`](crate::state::StateHandle)
//! can use it while the plugin itself is owned elsewhere (e.g. by a
//! [`RealtimePluginRunner`](crate::realtime::RealtimePluginRunner)). The plugin
//! [`release`](ControllerState::release)s it before terminating, after which every call fails
//! cleanly instead of reaching into an unloaded module.

use crate::error::{Error, Result};
use crossbeam_queue::ArrayQueue;
use std::sync::{Arc, Mutex};
use std::thread::{self, ThreadId};
use vst3::Steinberg::{
    IPlugView, IPlugViewContentScaleSupport, IPlugViewContentScaleSupportTrait, IPlugViewTrait,
};
use vst3::{ComPtr, ComWrapper, Steinberg::Vst::*, Steinberg::*};

#[cfg(target_os = "linux")]
use super::com_implementations::RunLoopRegistry;
use super::com_implementations::{
    create_memory_stream_with_metadata, HostPlugFrame, StreamStateType,
};
use crate::plugin::{encode_state_snapshot, StateSnapshot};

pub(crate) struct ControllerState {
    /// `None` once the plugin was released, like `controller`.
    component: Option<ComPtr<IComponent>>,
    /// The component is the controller too, see `save_state`.
    single_component: bool,
    /// `None` once the plugin was released: every call then fails.
    controller: Option<ComPtr<IEditController>>,
    /// From the plugin's metadata, see `has_editor`.
    has_gui: bool,
    view: Option<ComPtr<IPlugView>>,
    scale_factor: f32,
    // The IPlugFrame handed to the view, and the slot it writes requested sizes into.
    plug_frame: ComWrapper<HostPlugFrame>,
    resize_request: Arc<Mutex<Option<(i32, i32)>>>,
    // Linux IRunLoop registrations from the editor (fd handlers + timers).
    #[cfg(target_os = "linux")]
    run_loop: Arc<Mutex<RunLoopRegistry>>,
    // Parameter values that reached the processor from a thread that must not call
    // `IEditController`, shared with the plugin. Drained into the controller from the control
    // thread so the editor shows what the DSP plays.
    deferred_controller_sync: Arc<ArrayQueue<(u32, f64)>>,
    control_thread: ThreadId,
}

impl ControllerState {
    // One argument per shared object, all built by the plugin's loader.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        component: ComPtr<IComponent>,
        single_component: bool,
        controller: Option<ComPtr<IEditController>>,
        has_gui: bool,
        plug_frame: ComWrapper<HostPlugFrame>,
        resize_request: Arc<Mutex<Option<(i32, i32)>>>,
        #[cfg(target_os = "linux")] run_loop: Arc<Mutex<RunLoopRegistry>>,
        deferred_controller_sync: Arc<ArrayQueue<(u32, f64)>>,
    ) -> Self {
        ControllerState {
            component: Some(component),
            single_component,
            controller,
            has_gui,
            view: None,
            scale_factor: 1.0,
            plug_frame,
            resize_request,
            #[cfg(target_os = "linux")]
            run_loop,
            deferred_controller_sync,
            control_thread: thread::current().id(),
        }
    }

    /// Closes the editor and drops the component and controller references. Called by the
    /// plugin before it terminates: from then on the editor can't be opened again, nor the
    /// state saved.
    pub(crate) fn release(&mut self) {
        let _ = self.close();
        self.controller = None;
        self.component = None;
        self.has_gui = false;
    }

    pub(crate) fn is_released(&self) -> bool {
        self.component.is_none()
    }

    /// The plugin's state, as `Plugin::save_state` returns it. Only reads the component and
    /// the controller (`getState`), which hosts do from the UI thread while processing goes
    /// on, so it doesn't need the rest of the plugin.
    pub(crate) fn save_state(&self) -> Result<Vec<u8>> {
        // So the controller's state includes values set through the processor.
        self.apply_deferred_controller_sync();
        let component = self
            .component
            .as_ref()
            .ok_or_else(|| Error::Other("the plugin was destroyed".to_string()))?;
        unsafe {
            let component_stream =
                create_memory_stream_with_metadata(None, StreamStateType::Project);
            let component_ptr = component_stream.to_com_ptr::<IBStream>().ok_or_else(|| {
                Error::InterfaceError("Failed to create component state stream".into())
            })?;
            let result = component.getState(component_ptr.as_ptr());
            if result != kResultOk {
                return Err(Error::Other(format!(
                    "Plugin does not provide state (getState: {result:#x})"
                )));
            }

            // A single-component plugin's "controller" is the component itself, so asking it for
            // its state returns the blob above a second time: the envelope would carry the same
            // bytes twice, the combined size cap would effectively halve, and `load_state` would
            // apply `setState` twice. `load_state` has the matching guard.
            let controller = if let Some(controller) =
                self.controller.as_ref().filter(|_| !self.single_component)
            {
                let stream = create_memory_stream_with_metadata(None, StreamStateType::Project);
                let stream_ptr = stream.to_com_ptr::<IBStream>().ok_or_else(|| {
                    Error::InterfaceError("Failed to create controller state stream".into())
                })?;
                let result = controller.getState(stream_ptr.as_ptr());
                if result == kResultOk {
                    Some(stream.to_vec())
                } else if result == kNotImplemented || result == kResultFalse {
                    None
                } else {
                    return Err(Error::Other(format!(
                        "Failed to save controller state (getState: {result:#x})"
                    )));
                }
            } else {
                None
            };

            encode_state_snapshot(&StateSnapshot {
                component: component_stream.to_vec(),
                controller,
            })
        }
    }

    pub(crate) fn control_thread(&self) -> ThreadId {
        self.control_thread
    }

    pub(crate) fn take_resize_request(&self) -> Option<(i32, i32)> {
        self.resize_request.lock().ok().and_then(|mut s| s.take())
    }

    /// Applies parameter values queued off the control thread to the controller. A no-op
    /// anywhere but the control thread.
    pub(crate) fn apply_deferred_controller_sync(&self) {
        if thread::current().id() != self.control_thread {
            return;
        }
        let Some(controller) = self.controller.as_ref() else {
            return;
        };
        while let Some((id, value)) = self.deferred_controller_sync.pop() {
            let result = unsafe { controller.setParamNormalized(id, value) };
            if result != kResultOk && result != kResultTrue {
                log::debug!(
                    "setParamNormalized({id}) returned {result:#x}; applying anyway \
                     (many plugins report kResultFalse on success)"
                );
            }
        }
    }

    pub(crate) fn has_editor(&self) -> bool {
        // First check our cached value
        if self.has_gui {
            return true;
        }

        // An open editor is proof enough, and probing for a second view while one is attached
        // upsets plugins that assume a single live view.
        if self.view.is_some() {
            return true;
        }

        // Otherwise do a runtime check
        if let Some(ref controller) = self.controller {
            unsafe {
                // Check if controller can create an editor view
                let view_type = c"editor".as_ptr();
                let view_ptr = controller.createView(view_type);
                if !view_ptr.is_null() {
                    // Release the probe view; never call `removed()` on it — that pairs with
                    // `attached()`, and an unmatched `removed()` crashes some plugins that
                    // initialize their close state only on attach.
                    let _ = ComPtr::<IPlugView>::from_raw(view_ptr);
                    true
                } else {
                    false
                }
            }
        } else {
            false
        }
    }

    pub(crate) fn open(&mut self, parent: *mut std::ffi::c_void) -> Result<()> {
        if self.view.is_some() {
            return Err(Error::Other("Editor already open".to_string()));
        }
        if parent.is_null() {
            return Err(Error::Other(
                "editor parent window handle is null".to_string(),
            ));
        }

        if let Some(ref controller) = self.controller {
            unsafe {
                // Create editor view
                let view_type = c"editor".as_ptr();
                let view_ptr = controller.createView(view_type);
                if view_ptr.is_null() {
                    return Err(Error::Other("Failed to create editor view".to_string()));
                }

                let view = ComPtr::<IPlugView>::from_raw(view_ptr)
                    .ok_or_else(|| Error::Other("Failed to wrap view".to_string()))?;

                // Get view size
                let mut view_rect = ViewRect {
                    left: 0,
                    top: 0,
                    right: 400,
                    bottom: 300,
                };

                if view.getSize(&mut view_rect) != kResultOk {
                    return Err(Error::Other("Failed to get view size".to_string()));
                }
                view_rect_size(&view_rect)?;

                // Hand the plugin an IPlugFrame (before attach, per the SDK) so it can
                // request host-side resizes; requests land in `editor_resize`.
                let frame = self.plug_frame.to_com_ptr::<IPlugFrame>().ok_or_else(|| {
                    Error::Other("Failed to create editor plug frame".to_string())
                })?;
                let frame_result = view.setFrame(frame.as_ptr());
                if frame_result != kResultOk && frame_result != kResultTrue {
                    return Err(Error::Other(format!(
                        "Plugin rejected editor plug frame: {frame_result:#x}"
                    )));
                }

                // Offer the current scale factor, but never let the answer decide whether the
                // editor opens: a view that declines simply renders at its own scale.
                let _ = set_view_scale_factor(&view, self.scale_factor);

                // Platform-specific attachment
                #[cfg(target_os = "macos")]
                let platform_type = kPlatformTypeNSView;
                #[cfg(target_os = "windows")]
                let platform_type = kPlatformTypeHWND;
                #[cfg(target_os = "linux")]
                let platform_type = kPlatformTypeX11EmbedWindowID;
                #[cfg(target_os = "android")]
                {
                    view.setFrame(std::ptr::null_mut());
                    return Err(Error::Other(
                        "embedded VST3 editors are not supported on Android".to_string(),
                    ));
                }

                // Check platform support
                #[cfg(not(target_os = "android"))]
                if view.isPlatformTypeSupported(platform_type) != kResultOk {
                    view.setFrame(std::ptr::null_mut());
                    return Err(Error::Other("Platform type not supported".to_string()));
                }

                // Attach to parent window
                #[cfg(not(target_os = "android"))]
                let attach_result = view.attached(parent, platform_type);
                #[cfg(not(target_os = "android"))]
                if attach_result != kResultOk {
                    view.setFrame(std::ptr::null_mut());
                    return Err(Error::Other(format!(
                        "Failed to attach view: {:#x}",
                        attach_result
                    )));
                }

                #[cfg(not(target_os = "android"))]
                {
                    self.view = Some(view);
                    Ok(())
                }
            }
        } else {
            Err(Error::Other("No controller available".to_string()))
        }
    }

    pub(crate) fn close(&mut self) -> Result<()> {
        let mut close_error = None;
        if let Some(view) = self.view.take() {
            unsafe {
                let removed_result = view.removed();
                // Break the view -> host-frame reference before releasing the view. The frame
                // intentionally never retains the view, so this also avoids a COM retain cycle.
                let frame_result = view.setFrame(std::ptr::null_mut());
                if removed_result != kResultOk && removed_result != kResultTrue {
                    close_error = Some(Error::Other(format!(
                        "Failed to detach editor view: {removed_result:#x}"
                    )));
                } else if frame_result != kResultOk && frame_result != kResultTrue {
                    close_error = Some(Error::Other(format!(
                        "Failed to clear editor plug frame: {frame_result:#x}"
                    )));
                }
            }
        }
        // Drop any run-loop registrations the editor left behind. A well-behaved plugin
        // unregisters its own handlers and timers during `removed()`, but one that doesn't would
        // otherwise leave live ComPtrs into a view that no longer exists — and since
        // `Plugin::service_run_loop` is public with no editor-open guard, a host driving it from
        // its frame loop would then dispatch straight into the removed view.
        #[cfg(target_os = "linux")]
        if let Ok(mut reg) = self.run_loop.lock() {
            reg.handlers.clear();
            reg.timers.clear();
        }
        close_error.map_or(Ok(()), Err)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn service_run_loop(&mut self) {
        use vst3::Steinberg::Linux::{IEventHandlerTrait, ITimerHandlerTrait};

        // Fire due timers. Snapshot the handlers, then invoke with the lock
        // RELEASED: a callback may re-enter registerTimer/unregisterTimer
        // (VSTGUI does), which takes the same lock.
        let now = std::time::Instant::now();
        let mut due = Vec::new();
        if let Ok(mut reg) = self.run_loop.lock() {
            for timer in reg.timers.iter_mut() {
                if now >= timer.due {
                    timer.due = now + std::time::Duration::from_millis(timer.interval_ms);
                    due.push(timer.handler.clone());
                }
            }
        }
        for handler in due {
            unsafe { handler.onTimer() };
        }

        // Poll registered fds (zero timeout - never blocks the UI thread)
        // and notify ready ones. Same snapshot-then-invoke pattern.
        let handlers: Vec<_> = match self.run_loop.lock() {
            Ok(reg) => reg.handlers.clone(),
            Err(_) => return,
        };
        if handlers.is_empty() {
            return;
        }
        let mut fds: Vec<libc::pollfd> = handlers
            .iter()
            .map(|&(_, fd)| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 0) };
        if ready > 0 {
            for (pfd, (handler, fd)) in fds.iter().zip(handlers.iter()) {
                if pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
                    unsafe { handler.onFDIsSet(*fd) };
                }
            }
        }
    }

    pub(crate) fn size(&self) -> Result<(i32, i32)> {
        if let Some(view) = self.view.as_ref() {
            unsafe {
                let mut view_rect = ViewRect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                if view.getSize(&mut view_rect) != kResultOk {
                    return Err(Error::Other("Failed to query open editor size".to_string()));
                }
                return view_rect_size(&view_rect);
            }
        }

        if let Some(ref controller) = self.controller {
            unsafe {
                // Create a temporary view to get size
                let view_type = c"editor".as_ptr();
                let view_ptr = controller.createView(view_type);
                if view_ptr.is_null() {
                    return Err(Error::Other(
                        "Failed to create view for size query".to_string(),
                    ));
                }

                let view = ComPtr::<IPlugView>::from_raw(view_ptr)
                    .ok_or_else(|| Error::Other("Failed to wrap plug view".to_string()))?;

                // Get view size
                let mut view_rect = ViewRect {
                    left: 0,
                    top: 0,
                    right: 400,
                    bottom: 300,
                };

                let result = view.getSize(&mut view_rect);

                // Released when `view` drops; do not call `removed()` — it pairs with
                // `attached()`, which this probe never calls.

                if result == kResultOk {
                    view_rect_size(&view_rect)
                } else {
                    Ok((800, 600)) // Default size
                }
            }
        } else {
            Err(Error::Other("No controller available".to_string()))
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.view.is_some()
    }

    pub(crate) fn attached_size(&self) -> Result<Option<(i32, i32)>> {
        let Some(view) = self.view.as_ref() else {
            return Ok(None);
        };

        unsafe {
            let mut view_rect = ViewRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if view.getSize(&mut view_rect) != kResultOk {
                return Err(Error::Other(
                    "Failed to query attached editor size".to_string(),
                ));
            }
            view_rect_size(&view_rect).map(Some)
        }
    }

    pub(crate) fn attached_can_resize(&self) -> Option<bool> {
        self.view
            .as_ref()
            .map(|view| unsafe { view.canResize() == kResultTrue })
    }

    pub(crate) fn can_resize(&self) -> bool {
        unsafe {
            if let Some(view) = self.view.as_ref() {
                return view.canResize() == kResultTrue;
            }

            let Some(controller) = self.controller.as_ref() else {
                return false;
            };
            let view_ptr = controller.createView(c"editor".as_ptr());
            let Some(view) = ComPtr::<IPlugView>::from_raw(view_ptr) else {
                return false;
            };
            view.canResize() == kResultTrue
        }
    }

    pub(crate) fn resize(&mut self, width: i32, height: i32) -> Result<(i32, i32)> {
        if width <= 0 || height <= 0 {
            return Err(Error::Other(
                "editor dimensions must be greater than zero".to_string(),
            ));
        }
        let view = self
            .view
            .as_ref()
            .ok_or_else(|| Error::Other("Plugin editor is not open".to_string()))?;

        unsafe {
            if view.canResize() != kResultTrue {
                let mut current = ViewRect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                if view.getSize(&mut current) != kResultOk {
                    return Err(Error::Other(
                        "Plugin editor is fixed-size and its size could not be queried".to_string(),
                    ));
                }
                return view_rect_size(&current);
            }

            let mut requested = ViewRect {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            // The view constrains `requested` in place. `kResultFalse` means it left the rect
            // alone (nothing to constrain, or it wants a different size than asked for) — a
            // refusal to adapt, not a broken call — so take whatever rect it ended up with and
            // only reject result codes that mean the call itself failed.
            let constraint_result = view.checkSizeConstraint(&mut requested);
            let constrained = constraint_result == kResultOk
                || constraint_result == kResultTrue
                || constraint_result == kResultFalse
                || constraint_result == kNotImplemented;
            if !constrained {
                return Err(Error::Other(format!(
                    "Plugin failed to check the editor size constraint: {constraint_result:#x}"
                )));
            }
            let accepted = view_rect_size(&requested)?;

            // The SDK calls onSize only when the size actually changes; re-sending the current
            // one makes VSTGUI-based editors rebuild their frame for nothing.
            let mut current = ViewRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if view.getSize(&mut current) == kResultOk
                && view_rect_size(&current).ok() == Some(accepted)
            {
                return Ok(accepted);
            }

            let resize_result = view.onSize(&mut requested);
            if resize_result != kResultOk && resize_result != kResultTrue {
                return Err(Error::Other(format!(
                    "Plugin rejected editor resize: {resize_result:#x}"
                )));
            }
            Ok(accepted)
        }
    }

    pub(crate) fn set_scale_factor(&mut self, factor: f32) -> Result<bool> {
        if !factor.is_finite() || factor <= 0.0 {
            return Err(Error::Other(
                "editor scale factor must be finite and greater than zero".to_string(),
            ));
        }
        let supported = match self.view.as_ref() {
            Some(view) => unsafe { set_view_scale_factor(view, factor)? },
            None => false,
        };
        self.scale_factor = factor;
        Ok(supported)
    }
}

fn view_rect_size(rect: &ViewRect) -> Result<(i32, i32)> {
    let width = rect
        .right
        .checked_sub(rect.left)
        .filter(|width| *width > 0)
        .ok_or_else(|| Error::Other("plugin editor returned an invalid width".to_string()))?;
    let height = rect
        .bottom
        .checked_sub(rect.top)
        .filter(|height| *height > 0)
        .ok_or_else(|| Error::Other("plugin editor returned an invalid height".to_string()))?;
    Ok((width, height))
}

/// Offer a content scale factor to an editor view.
///
/// `Ok(true)` means the view took the factor. `Ok(false)` means it declined — either it does not
/// implement `IPlugViewContentScaleSupport` at all, or it implements it and answered with a
/// non-success code. Declining is a normal answer, not a failure: JUCE editors implement the
/// interface and return `kResultFalse` on macOS, where the window server already scales the
/// backing store. Treating that as an error takes the editor away from every JUCE plugin, so it
/// never becomes one. Only a nonsensical *request* (non-finite or non-positive) is an error.
pub(crate) unsafe fn set_view_scale_factor(view: &ComPtr<IPlugView>, factor: f32) -> Result<bool> {
    if !factor.is_finite() || factor <= 0.0 {
        return Err(Error::Other(
            "editor scale factor must be finite and greater than zero".to_string(),
        ));
    }
    let Some(scale_support) = view.cast::<IPlugViewContentScaleSupport>() else {
        return Ok(false);
    };
    let result = scale_support.setContentScaleFactor(factor);
    let accepted = result == kResultOk || result == kResultTrue;
    if !accepted {
        log::debug!("plugin declined editor scale factor {factor}: {result:#x}");
    }
    Ok(accepted)
}
