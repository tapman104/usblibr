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
    /// A new USB device has appeared.
    DeviceArrived {
        /// A platform path that can typically be passed to [`crate::UsbContext::open`].
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
        CM_Register_Notification, CM_Unregister_Notification, CM_NOTIFY_ACTION,
        CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL, CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL,
        CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
        HCMNOTIFICATION,
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

        if action != CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL
            && action != CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL
        {
            return 0;
        }

        // SAFETY: `context` is non-null and was created from `Arc<CallbackData>`
        // in `register`; `Inner` keeps that allocation alive for callback lifetime.
        let data = unsafe { &*(context as *const CallbackData) };

        // SAFETY: action has already been validated to one of the two
        // DEVICEINTERFACE actions above, so accessing the DeviceInterface union
        // member is sound for this callback invocation.
        let sym_ptr = unsafe { (*event_data).u.DeviceInterface.SymbolicLink.as_ptr() };
        // Find null terminator to safely reconstruct the path.
        let mut len = 0usize;
        // SAFETY: symbolic link is a null-terminated UTF-16 string supplied by CM.
        while unsafe { *sym_ptr.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: we computed `len` by scanning until the first terminator.
        let sym_slice = unsafe { std::slice::from_raw_parts(sym_ptr, len) };
        let path = String::from_utf16_lossy(sym_slice);

        let event = if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL {
            HotplugEvent::DeviceArrived { path }
        } else {
            HotplugEvent::DeviceLeft { path }
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
            CM_Register_Notification(&filter, Some(ctx_ptr), Some(cm_callback), &mut hnotify)
        };
        // CR_SUCCESS = 0
        if rc.0 != 0 {
            return Err(crate::error::UsbError::Other(format!(
                "CM_Register_Notification failed with CONFIGRET {:#010x}",
                rc.0
            )));
        }

        Ok(Inner {
            hnotify,
            _data: data,
        })
    }
}

// Stub for non-Windows platforms.
#[cfg(target_os = "linux")]
mod platform {
    use std::os::unix::io::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};

    use nix::libc;
    use udev::{EventType, MonitorBuilder};

    use super::HotplugEvent;

    pub(crate) struct Inner {
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    pub(crate) fn register<F>(callback: F) -> Result<Inner, crate::error::UsbError>
    where
        F: Fn(HotplugEvent) + Send + Sync + 'static,
    {
        let monitor = MonitorBuilder::new()
            .map_err(crate::error::UsbError::Io)?
            .match_subsystem_devtype("usb", "usb_device")
            .map_err(crate::error::UsbError::Io)?
            .listen()
            .map_err(crate::error::UsbError::Io)?;

        let callback: Arc<dyn Fn(HotplugEvent) + Send + Sync> = Arc::new(callback);
        let stop = Arc::new(AtomicBool::new(false));

        let worker_stop = Arc::clone(&stop);
        let worker_cb = Arc::clone(&callback);

        let worker = thread::spawn(move || {
            let fd = monitor.as_raw_fd();
            let mut poll_fd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            while !worker_stop.load(Ordering::Relaxed) {
                // Wait up to 250ms so Drop can stop the thread promptly.
                let rc = unsafe { libc::poll(&mut poll_fd, 1, 250) };
                if rc <= 0 {
                    continue;
                }
                if (poll_fd.revents & libc::POLLIN) == 0 {
                    continue;
                }

                for event in monitor.iter() {
                    let path = event
                        .devnode()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|| event.syspath().to_string_lossy().to_string());

                    match event.event_type() {
                        EventType::Add => (worker_cb)(HotplugEvent::DeviceArrived { path }),
                        EventType::Remove => (worker_cb)(HotplugEvent::DeviceLeft { path }),
                        _ => {}
                    }
                }
            }
        });

        Ok(Inner {
            stop,
            worker: Some(worker),
        })
    }
}

// macOS hotplug via IOKit IOServiceAddMatchingNotification + CFRunLoop.
#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{c_char, c_void, CStr};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};

    use core_foundation::base::{kCFAllocatorDefault, TCFType};
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_foundation_sys::base::CFTypeRef;
    use core_foundation_sys::runloop::{
        kCFRunLoopDefaultMode, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRef,
        CFRunLoopRun, CFRunLoopSourceRef, CFRunLoopStop,
    };
    use IOKit_sys as iokit_sys;
    use iokit_sys::{io_iterator_t, kIOReturnSuccess};

    const K_IO_MASTER_PORT_DEFAULT: u32 = 0;

    use super::HotplugEvent;

    /// USB device class name used with IOServiceMatching.
    const K_IO_USB_DEVICE_CLASS_NAME: &CStr =
        unsafe { CStr::from_bytes_with_nul_unchecked(b"IOUSBDevice\0") };

    /// IOKit notification type: first service matching the criteria appeared.
    const K_IO_FIRST_MATCH_NOTIFICATION: &CStr =
        unsafe { CStr::from_bytes_with_nul_unchecked(b"IOServiceFirstMatch\0") };

    /// IOKit notification type: matching service is being terminated.
    const K_IO_TERMINATED_NOTIFICATION: &CStr =
        unsafe { CStr::from_bytes_with_nul_unchecked(b"IOServiceTerminate\0") };

    /// Opaque pointer to an IONotificationPort.
    type IONotificationPortRef = *mut c_void;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        /// Create an IONotificationPort backed by the given master Mach port.
        fn IONotificationPortCreate(master_port: u32) -> IONotificationPortRef;
        /// Destroy an IONotificationPort and free its resources.
        fn IONotificationPortDestroy(notify: IONotificationPortRef);
        /// Return the CFRunLoopSource that dispatches events from `notify`.
        fn IONotificationPortGetRunLoopSource(
            notify: IONotificationPortRef,
        ) -> CFRunLoopSourceRef;
        /// Register for service-matching or termination events.
        ///
        /// The `matching` dictionary reference is consumed (its retain count is
        /// decremented) by this function whether it succeeds or fails.
        fn IOServiceAddMatchingNotification(
            notify_port: IONotificationPortRef,
            notification_type: *const c_char,
            matching: *const c_void,
            callback: unsafe extern "C" fn(*mut c_void, io_iterator_t),
            ref_con: *mut c_void,
            notification: *mut io_iterator_t,
        ) -> i32;
    }

    /// Newtype wrapper so that `CFRunLoopRef` (a raw pointer) can be sent to
    /// the owner thread.  Apple explicitly documents `CFRunLoopStop` as safe
    /// to call from threads other than the one running the loop.
    struct SendRunLoop(CFRunLoopRef);
    // SAFETY: CFRunLoopStop may be called from any thread per Apple documentation.
    unsafe impl Send for SendRunLoop {}

    /// Per-notification callback context — heap allocated, freed after the
    /// worker's run loop exits.
    struct IterContext {
        is_arrival: bool,
        callback: Arc<dyn Fn(HotplugEvent) + Send + Sync + 'static>,
    }

    pub(crate) struct Inner {
        /// The CFRunLoop running in the worker thread.  Calling `CFRunLoopStop`
        /// on it causes the worker to exit and clean up.
        run_loop: SendRunLoop,
        worker: Option<JoinHandle<()>>,
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            if !self.run_loop.0.is_null() {
                // SAFETY: CFRunLoopStop is thread-safe; run_loop.0 was created
                // by CFRunLoopGetCurrent in the worker and sent here via channel.
                unsafe { CFRunLoopStop(self.run_loop.0) };
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    /// IOKit service-matching callback.  Called on the worker's run loop thread
    /// when services arrive or terminate.
    ///
    /// # Safety
    /// `refcon` must point to a live `IterContext` for the duration of the
    /// notification port's lifetime.  Guaranteed by `Inner` keeping both the
    /// port and the context alive until after `CFRunLoopRun` returns.
    unsafe extern "C" fn service_callback(refcon: *mut c_void, iterator: io_iterator_t) {
        if refcon.is_null() {
            return;
        }
        // SAFETY: refcon is a Box<IterContext> cast to raw pointer; it is valid
        // because the worker frees it only after CFRunLoopRun() returns, and all
        // callbacks fire on the worker thread before that point.
        let ctx = &*(refcon as *const IterContext);
        loop {
            // SAFETY: iterator is the valid io_iterator_t supplied by IOKit.
            let service = iokit_sys::IOIteratorNext(iterator);
            if service == 0 {
                break;
            }
            let path = build_device_path(service);
            let event = if ctx.is_arrival {
                HotplugEvent::DeviceArrived { path }
            } else {
                HotplugEvent::DeviceLeft { path }
            };
            (ctx.callback)(event);
            // SAFETY: service is a valid io_object_t returned by IOIteratorNext.
            iokit_sys::IOObjectRelease(service);
        }
    }

    /// Build an `iokit:bus=…` path from a (possibly terminating) IOService.
    fn build_device_path(service: io_iterator_t) -> String {
        let bus = read_iokit_integer(service, "USBBusNumber").unwrap_or(0) as u8;
        let addr = read_iokit_integer(service, "USB Address").unwrap_or(0) as u8;
        let vid = read_iokit_integer(service, "idVendor").unwrap_or(0) as u16;
        let pid = read_iokit_integer(service, "idProduct").unwrap_or(0) as u16;
        format!("iokit:bus={bus},addr={addr},vid={vid:04x},pid={pid:04x}")
    }

    /// Read an integer IORegistry property; mirrors the helper in macos.rs.
    fn read_iokit_integer(service: io_iterator_t, key: &str) -> Option<i64> {
        let cf_key = CFString::new(key);
        // SAFETY: service is valid; cf_key lifetime covers the call.
        let cf_val: CFTypeRef = unsafe {
            iokit_sys::IORegistryEntryCreateCFProperty(
                service,
                cf_key.as_concrete_TypeRef() as _,
                std::ptr::null_mut(),
                0,
            )
        };
        if cf_val.is_null() {
            return None;
        }
        // SAFETY: we own the reference; wrap_under_create_rule releases it on drop.
        let number = unsafe { CFNumber::wrap_under_create_rule(cf_val as _) };
        number.to_i64()
    }

    /// Drain an iterator without emitting events (arms future notifications).
    ///
    /// # Safety
    /// `iter` must be a valid io_iterator_t.
    unsafe fn drain_iterator_silent(iter: io_iterator_t) {
        loop {
            let service = iokit_sys::IOIteratorNext(iter);
            if service == 0 {
                break;
            }
            // SAFETY: service is a valid io_object_t returned by IOIteratorNext.
            iokit_sys::IOObjectRelease(service);
        }
    }

    pub(crate) fn register<F>(callback: F) -> Result<Inner, crate::error::UsbError>
    where
        F: Fn(HotplugEvent) + Send + Sync + 'static,
    {
        let callback: Arc<dyn Fn(HotplugEvent) + Send + Sync + 'static> = Arc::new(callback);
        let callback_arr = Arc::clone(&callback);
        let callback_rem = Arc::clone(&callback);

        // Channel to receive the worker thread's CFRunLoopRef (or null on failure).
        let (rl_tx, rl_rx) = mpsc::channel::<SendRunLoop>();

        let worker = thread::spawn(move || {
            // SAFETY: CFRunLoopGetCurrent returns the run loop for this thread.
            let run_loop: CFRunLoopRef = unsafe { CFRunLoopGetCurrent() };

            // SAFETY: K_IO_MASTER_PORT_DEFAULT as u32 is sound on Apple platforms.
            let notify_port: IONotificationPortRef =
                unsafe { IONotificationPortCreate(K_IO_MASTER_PORT_DEFAULT) };
            if notify_port.is_null() {
                log::error!("macOS hotplug: IONotificationPortCreate failed");
                let _ = rl_tx.send(SendRunLoop(std::ptr::null_mut()));
                return;
            }

            // Obtain the run loop source from the notification port and attach it
            // to this thread's run loop so that IOKit can deliver callbacks here.
            // SAFETY: notify_port is a valid IONotificationPortRef created above.
            let source: CFRunLoopSourceRef =
                unsafe { IONotificationPortGetRunLoopSource(notify_port) };
            if source.is_null() {
                // SAFETY: notify_port is valid; destroy it before returning.
                unsafe { IONotificationPortDestroy(notify_port) };
                log::error!("macOS hotplug: IONotificationPortGetRunLoopSource returned null");
                let _ = rl_tx.send(SendRunLoop(std::ptr::null_mut()));
                return;
            }
            // SAFETY: run_loop, source, and kCFRunLoopDefaultMode are all valid.
            unsafe { CFRunLoopAddSource(run_loop, source, kCFRunLoopDefaultMode) };

            // ---- Register for arrival (IOServiceFirstMatch) ----
            let arrived_ctx = Box::into_raw(Box::new(IterContext {
                is_arrival: true,
                callback: callback_arr,
            }));
            let mut arrived_iter: io_iterator_t = 0;
            // SAFETY: K_IO_USB_DEVICE_CLASS_NAME is a valid null-terminated string.
            // IOServiceMatching returns a dict with retain count 1; ownership is
            // transferred to IOServiceAddMatchingNotification.
            let arrived_dict =
                unsafe { iokit_sys::IOServiceMatching(K_IO_USB_DEVICE_CLASS_NAME.as_ptr()) };
            if arrived_dict.is_null() {
                // SAFETY: notify_port is valid; arrived_ctx was just allocated.
                unsafe {
                    let _ = Box::from_raw(arrived_ctx);
                    IONotificationPortDestroy(notify_port);
                }
                log::error!("macOS hotplug: IOServiceMatching returned null (arrival)");
                let _ = rl_tx.send(SendRunLoop(std::ptr::null_mut()));
                return;
            }
            // SAFETY: all pointers are valid; arrived_dict ownership is consumed
            // by IOServiceAddMatchingNotification regardless of return value.
            let kr = unsafe {
                IOServiceAddMatchingNotification(
                    notify_port,
                    K_IO_FIRST_MATCH_NOTIFICATION.as_ptr(),
                    arrived_dict as *const c_void,
                    service_callback,
                    arrived_ctx as *mut c_void,
                    &mut arrived_iter,
                )
            };
            if kr != kIOReturnSuccess {
                // SAFETY: arrived_dict was consumed above; do not release it again.
                unsafe {
                    let _ = Box::from_raw(arrived_ctx);
                    IONotificationPortDestroy(notify_port);
                }
                log::error!(
                    "macOS hotplug: IOServiceAddMatchingNotification (arrival) failed {kr:#x}"
                );
                let _ = rl_tx.send(SendRunLoop(std::ptr::null_mut()));
                return;
            }
            // Drain the initial iterator to arm future arrival notifications.
            // SAFETY: arrived_iter is a valid io_iterator_t filled by the call above.
            unsafe { drain_iterator_silent(arrived_iter) };

            // ---- Register for removal (IOServiceTerminate) ----
            let removed_ctx = Box::into_raw(Box::new(IterContext {
                is_arrival: false,
                callback: callback_rem,
            }));
            let mut removed_iter: io_iterator_t = 0;
            let removed_dict =
                unsafe { iokit_sys::IOServiceMatching(K_IO_USB_DEVICE_CLASS_NAME.as_ptr()) };
            if removed_dict.is_null() {
                // SAFETY: clean up both context allocations and the notification port.
                unsafe {
                    let _ = Box::from_raw(removed_ctx);
                    let _ = Box::from_raw(arrived_ctx);
                    iokit_sys::IOObjectRelease(arrived_iter);
                    IONotificationPortDestroy(notify_port);
                }
                log::error!("macOS hotplug: IOServiceMatching returned null (removal)");
                let _ = rl_tx.send(SendRunLoop(std::ptr::null_mut()));
                return;
            }
            // SAFETY: all pointers valid; removed_dict ownership consumed by IOKit.
            let kr = unsafe {
                IOServiceAddMatchingNotification(
                    notify_port,
                    K_IO_TERMINATED_NOTIFICATION.as_ptr(),
                    removed_dict as *const c_void,
                    service_callback,
                    removed_ctx as *mut c_void,
                    &mut removed_iter,
                )
            };
            if kr != kIOReturnSuccess {
                // SAFETY: removed_dict was consumed; clean up remaining resources.
                unsafe {
                    let _ = Box::from_raw(removed_ctx);
                    let _ = Box::from_raw(arrived_ctx);
                    iokit_sys::IOObjectRelease(arrived_iter);
                    IONotificationPortDestroy(notify_port);
                }
                log::error!(
                    "macOS hotplug: IOServiceAddMatchingNotification (removal) failed {kr:#x}"
                );
                let _ = rl_tx.send(SendRunLoop(std::ptr::null_mut()));
                return;
            }
            // Drain the initial iterator to arm future removal notifications.
            // SAFETY: removed_iter is a valid io_iterator_t filled by the call above.
            unsafe { drain_iterator_silent(removed_iter) };

            // Send our run loop ref to the owner so Drop can call CFRunLoopStop.
            // This send happens before CFRunLoopRun; if the owner drops Inner before
            // CFRunLoopRun starts, CFRunLoopStop will have set the stop flag and
            // CFRunLoopRun will return immediately on the next iteration.
            let _ = rl_tx.send(SendRunLoop(run_loop));

            // Block until CFRunLoopStop is called from Inner::drop.
            // SAFETY: CFRunLoopRun blocks this thread on its default-mode run loop;
            // it returns when CFRunLoopStop is called from another thread.
            unsafe { CFRunLoopRun() };

            // The run loop has stopped — clean up all resources on this thread.
            // No IOKit callbacks can fire after this point.
            // SAFETY: iterators and port are valid; contexts are not accessed after
            // CFRunLoopRun returns (the loop is stopped, no callbacks can fire).
            unsafe {
                iokit_sys::IOObjectRelease(arrived_iter);
                iokit_sys::IOObjectRelease(removed_iter);
                IONotificationPortDestroy(notify_port);
                let _ = Box::from_raw(arrived_ctx);
                let _ = Box::from_raw(removed_ctx);
            }
        });

        // Block until the worker finishes setup and either sends a valid run loop
        // ref or signals failure with a null pointer.
        let run_loop = rl_rx.recv().map_err(|_| {
            crate::error::UsbError::Other("macOS hotplug worker thread panicked during setup".into())
        })?;

        if run_loop.0.is_null() {
            let _ = worker.join();
            return Err(crate::error::UsbError::Other(
                "macOS hotplug: IONotificationPort setup failed".into(),
            ));
        }

        Ok(Inner {
            run_loop,
            worker: Some(worker),
        })
    }
}

// Stub for platforms other than Windows, Linux, and macOS.
#[cfg(all(
    not(target_os = "windows"),
    not(target_os = "linux"),
    not(target_os = "macos")
))]
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
    /// `callback` will be invoked from a **system thread** whenever a USB
    /// device arrives or departs. Keep it short and avoid blocking calls.
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
