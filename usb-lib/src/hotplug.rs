//! Hotplug detection — device arrival and removal notifications.
//!
//! Use [`UsbContext::register_hotplug`] to subscribe to events.
//!
//! The returned [`HotplugHandle`] keeps the subscription alive.
//! Drop it (or call [`HotplugHandle::unregister`]) to cancel.

// ---------------------------------------------------------------------------
// Public event type
// ---------------------------------------------------------------------------

/// An event emitted by the hotplug subsystem.
#[derive(Debug, Clone)]
pub enum HotplugEvent {
    /// A new device matching the WinUSB device interface class has appeared.
    DeviceArrived {
        /// The device symbolic link / path that can be passed to [`crate::UsbContext::open`].
        path: String,
    },
    /// A device is being removed (or has been removed).
    DeviceLeft {
        /// The device symbolic link that was reported at arrival time.
        path: String,
    },
}

// ---------------------------------------------------------------------------
// HotplugHandle
// ---------------------------------------------------------------------------

/// An active hotplug subscription.
///
/// The subscription is automatically cancelled when this handle is dropped.
/// You can also cancel it explicitly with [`HotplugHandle::unregister`].
pub struct HotplugHandle {
    // Kept only for its Drop effect; not read after registration.
    #[allow(dead_code)]
    inner: HotplugHandleInner,
}

impl HotplugHandle {
    /// Cancel the hotplug subscription immediately.
    pub fn unregister(self) {
        // Dropping `self` triggers the platform-specific cleanup via Drop.
        drop(self);
    }
}

// ---------------------------------------------------------------------------
// Platform-specific inner implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod platform {
    use std::sync::Arc;

    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Register_Notification, CM_Unregister_Notification, HCMNOTIFICATION,
        CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
        CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
        CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
    };

    use super::HotplugEvent;

    // The WinUSB device interface GUID must match the one in the Windows backend.
    const WINUSB_DEVICE_INTERFACE_GUID: windows::core::GUID = windows::core::GUID {
        data1: 0xDEE8_24EF,
        data2: 0x729B,
        data3: 0x4A0E,
        data4: [0x9C, 0x14, 0xB7, 0x11, 0x7D, 0x33, 0xA8, 0x17],
    };

    /// Heap-allocated callback state.  A raw pointer to this is passed as the
    /// `pContext` user-data to `CM_Register_Notification`.
    struct CallbackData {
        callback: Box<dyn Fn(HotplugEvent) + Send + Sync + 'static>,
    }

    /// The CM notification handle plus the heap-allocated callback data.
    pub(crate) struct Inner {
        hnotify: HCMNOTIFICATION,
        /// Kept alive here so it is not freed before `CM_Unregister_Notification`.
        _data: Arc<CallbackData>,
    }

    /// # Safety
    /// `HCMNOTIFICATION` is an opaque pointer with no thread affinity — it can be
    /// moved across threads.
    unsafe impl Send for Inner {}

    impl Drop for Inner {
        fn drop(&mut self) {
            // Unregister *before* `_data` is freed so no callbacks fire after free.
            // SAFETY: hnotify was created by CM_Register_Notification.
            unsafe {
                let _ = CM_Unregister_Notification(self.hnotify);
            }
        }
    }

    /// CM notification callback (called on an internal system thread).
    ///
    /// # Safety
    /// `context` must be a valid `*const CallbackData` for the duration of the
    /// notification.  This is guaranteed by `Inner` keeping `_data` alive and
    /// always unregistering before freeing.
    unsafe extern "system" fn cm_callback(
        _hnotify: HCMNOTIFICATION,
        context: *const core::ffi::c_void,
        action: CM_NOTIFY_ACTION,
        event_data: *const CM_NOTIFY_EVENT_DATA,
        _event_data_size: u32,
    ) -> u32 {
        if context.is_null() || event_data.is_null() {
            return 0;
        }
        let data = &*(context as *const CallbackData);

        // Extract the symbolic link from the DeviceInterface union member.
        // CM_NOTIFY_EVENT_DATA.u.DeviceInterface.SymbolicLink is a WCHAR array.
        let sym_ptr = (*event_data).u.DeviceInterface.SymbolicLink.as_ptr();
        // Find null terminator to safely reconstruct the path.
        let mut len = 0usize;
        while *sym_ptr.add(len) != 0 {
            len += 1;
        }
        let sym_slice = std::slice::from_raw_parts(sym_ptr, len);
        let path = String::from_utf16_lossy(sym_slice).to_string();

        let event = if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL {
            HotplugEvent::DeviceArrived { path }
        } else if action == CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL {
            HotplugEvent::DeviceLeft { path }
        } else {
            return 0;
        };

        (data.callback)(event);
        0
    }

    /// Register a callback with the Configuration Manager and return the inner state.
    pub(crate) fn register<F>(callback: F) -> Result<Inner, crate::error::UsbError>
    where
        F: Fn(HotplugEvent) + Send + Sync + 'static,
    {
        let data = Arc::new(CallbackData {
            callback: Box::new(callback),
        });

        // Build the filter: listen to device-interface events for the WinUSB GUID.
        let mut filter = CM_NOTIFY_FILTER {
            cbSize: core::mem::size_of::<CM_NOTIFY_FILTER>() as u32,
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
            ..Default::default()
        };
        // SAFETY: union field access — setting the DeviceInterface variant.
        filter.u.DeviceInterface.ClassGuid = WINUSB_DEVICE_INTERFACE_GUID;

        let ctx_ptr = Arc::as_ptr(&data) as *const core::ffi::c_void;
        let mut hnotify = HCMNOTIFICATION::default();

        // SAFETY: cm_callback is a valid extern "system" fn pointer; ctx_ptr is valid
        // as long as `data` (and therefore `Inner._data`) is alive.
        let rc = unsafe {
            CM_Register_Notification(
                &filter,
                Some(ctx_ptr),
                Some(cm_callback),
                &mut hnotify,
            )
        };
        // CR_SUCCESS = 0
        if rc.0 != 0 {
            return Err(crate::error::UsbError::Other(format!(
                "CM_Register_Notification failed with CONFIGRET {:#010x}", rc.0
            )));
        }

        Ok(Inner {
            hnotify,
            _data: data,
        })
    }
}

// Stub for non-Windows platforms.
#[cfg(not(target_os = "windows"))]
mod platform {
    pub(crate) struct Inner;

    impl Drop for Inner {
        fn drop(&mut self) {}
    }

    pub(crate) fn register<F>(_callback: F) -> Result<Inner, crate::error::UsbError>
    where
        F: Fn(super::HotplugEvent) + Send + Sync + 'static,
    {
        Err(crate::error::UsbError::Unsupported)
    }
}

// ---------------------------------------------------------------------------
// HotplugHandleInner — wraps the platform Inner
// ---------------------------------------------------------------------------

#[allow(dead_code)]
struct HotplugHandleInner(platform::Inner);

impl HotplugHandle {
    /// Register a hotplug callback.
    ///
    /// `callback` will be invoked from a **system thread** whenever a WinUSB
    /// device arrives or departs.  Keep it short and avoid blocking calls.
    pub fn register<F>(callback: F) -> Result<Self, crate::error::UsbError>
    where
        F: Fn(HotplugEvent) + Send + Sync + 'static,
    {
        let inner = platform::register(callback)?;
        Ok(Self {
            inner: HotplugHandleInner(inner),
        })
    }
}
