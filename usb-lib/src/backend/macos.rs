use std::ffi::CStr;
use std::time::Duration;

use core_foundation::base::{kCFAllocatorDefault, CFType, TCFType};
use core_foundation::dictionary::CFMutableDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFRelease, CFTypeRef};
use iokit_sys::io_iterator_t;
use iokit_sys::io_service_t;
use iokit_sys::kIOMasterPortDefault;
use iokit_sys::ret_codes::kIOReturnSuccess;

use crate::core::{ConfigDescriptor, ControlSetup, DeviceDescriptor, DeviceInfo};
use crate::error::UsbError;

use super::{UsbBackend, UsbDevice};

// -----------------------------------------------------------------------
// IOKit / IOUSBLib FFI types and constants
// -----------------------------------------------------------------------

// IOKit framework is loaded at link time via the iokit-sys crate.
// We call raw C functions from iokit-sys for device iteration.

/// USB class name used with IOServiceMatching.
const K_IO_USB_DEVICE_CLASS_NAME: &CStr =
    unsafe { CStr::from_bytes_with_nul_unchecked(b"IOUSBDevice\0") };

/// Standard USB GET_DESCRIPTOR request type (IN | Standard | Device)
const REQ_TYPE_IN_STD_DEV: u8 = 0x80;
const GET_DESCRIPTOR: u8 = 0x06;

// -----------------------------------------------------------------------
// Public backend entry point
// -----------------------------------------------------------------------

pub struct MacOsBackend;

impl UsbBackend for MacOsBackend {
    fn enumerate(&self) -> Result<Vec<DeviceInfo>, UsbError> {
        enumerate_iokit_devices()
    }

    fn open(&self, path: &str) -> Result<Box<dyn UsbDevice>, UsbError> {
        let dev = MacOsDevice::open(path)?;
        Ok(Box::new(dev))
    }
}

// -----------------------------------------------------------------------
// Device enumeration via IOKit
// -----------------------------------------------------------------------

fn enumerate_iokit_devices() -> Result<Vec<DeviceInfo>, UsbError> {
    let mut result = Vec::new();

    // IOServiceMatching("IOUSBDevice") — returns a dictionary with retain count 1.
    // SAFETY: K_IO_USB_DEVICE_CLASS_NAME is a valid null-terminated C string.
    let matching_dict = unsafe {
        iokit_sys::IOServiceMatching(K_IO_USB_DEVICE_CLASS_NAME.as_ptr())
    };
    if matching_dict.is_null() {
        return Err(UsbError::Other("IOServiceMatching returned NULL".into()));
    }
    // Note: IOServiceGetMatchingServices consumes the matching dict reference (no need to release).

    let mut iter: io_iterator_t = 0;
    // SAFETY: matching_dict is valid; iter is a valid out-pointer.
    let kr = unsafe {
        iokit_sys::IOServiceGetMatchingServices(kIOMasterPortDefault, matching_dict, &mut iter)
    };
    if kr != kIOReturnSuccess {
        return Err(UsbError::Other(format!("IOServiceGetMatchingServices err {kr:#x}")));
    }

    // Iterate services
    loop {
        // SAFETY: iter is a valid IOIterator obtained above.
        let service: io_service_t = unsafe { iokit_sys::IOIteratorNext(iter) };
        if service == 0 {
            break;
        }

        if let Some(info) = device_info_from_service(service) {
            result.push(info);
        }

        // SAFETY: service is a valid io_object_t; release after we are done.
        unsafe { iokit_sys::IOObjectRelease(service) };
    }

    // SAFETY: iter is a valid io_iterator_t; release iterator when done.
    unsafe { iokit_sys::IOObjectRelease(iter) };

    Ok(result)
}

/// Extract a DeviceInfo from one io_service_t.
fn device_info_from_service(service: io_service_t) -> Option<DeviceInfo> {
    let vendor_id = iokit_integer_property(service, "idVendor")? as u16;
    let product_id = iokit_integer_property(service, "idProduct")? as u16;

    let bus_number = iokit_integer_property(service, "USBBusNumber")
        .unwrap_or(0) as u8;
    let device_address = iokit_integer_property(service, "USB Address")
        .unwrap_or(0) as u8;

    // Build a stable "path" from bus + address — matches the format used by MacOsDevice::open.
    let path = format!("iokit:bus={bus_number},addr={device_address},vid={vendor_id:04x},pid={product_id:04x}");

    let manufacturer = iokit_string_property(service, "USB Vendor Name");
    let product = iokit_string_property(service, "USB Product Name");
    let serial_number = iokit_string_property(service, "USB Serial Number");

    Some(DeviceInfo {
        vendor_id,
        product_id,
        bus_number,
        device_address,
        path,
        manufacturer,
        product,
        serial_number,
    })
}

/// Read an integer IORegistry property from a service, returning i64.
fn iokit_integer_property(service: io_service_t, key: &str) -> Option<i64> {
    let cf_key = CFString::new(key);
    // SAFETY: service is valid; cf_key lifetime covers this call.
    let cf_val: CFTypeRef = unsafe {
        iokit_sys::IORegistryEntryCreateCFProperty(
            service,
            cf_key.as_concrete_TypeRef() as _,
            kCFAllocatorDefault,
            0,
        )
    };
    if cf_val.is_null() {
        return None;
    }
    // Treat as CFNumber and extract i64.
    // SAFETY: We retain-count-transfer ownership; release after extraction.
    let number = unsafe { CFNumber::wrap_under_create_rule(cf_val as _) };
    number.to_i64()
}

/// Read a string IORegistry property from a service.
fn iokit_string_property(service: io_service_t, key: &str) -> Option<String> {
    let cf_key = CFString::new(key);
    // SAFETY: service is valid; cf_key lifetime covers this call.
    let cf_val: CFTypeRef = unsafe {
        iokit_sys::IORegistryEntryCreateCFProperty(
            service,
            cf_key.as_concrete_TypeRef() as _,
            kCFAllocatorDefault,
            0,
        )
    };
    if cf_val.is_null() {
        return None;
    }
    // SAFETY: We own the reference; wrap and extract.
    let cf_str = unsafe { CFString::wrap_under_create_rule(cf_val as _) };
    Some(cf_str.to_string())
}

// -----------------------------------------------------------------------
// MacOsDevice — uses IOUSBLib via a plugin interface for control transfers
// -----------------------------------------------------------------------

/// A macOS USB device opened via IOUSBLib.
///
/// We use the IOCFPlugIn / IOUSBDeviceInterface approach:
/// 1. IOCreatePlugInInterfaceForService → IOCFPlugInInterface
/// 2. QueryInterface for kIOUSBDeviceInterfaceID → IOUSBDeviceInterface
/// 3. USBDeviceOpen → exclusive access  
/// 4. DeviceRequest for control transfers
///
/// This is encapsulated in UnsafeCell to satisfy Send; the handle is used
/// exclusively from one thread at a time (no Sync needed).
use std::cell::UnsafeCell;

// IOUSBLib CFUUID strings (from IOKit/USB/IOUSBLib.h)
// We use raw CFUUIDs to locate the plugin interface and device interface.

/// Raw pointer bundle representing an opened IOUSBDevice interface.
/// Null means not yet opened or failed.
struct MacOsDevice {
    /// The path string used when opening (for re-identification).
    path: String,
    /// IOUSBDeviceInterface** — opaque pointer managed via IOKit COM-style interfaces.
    /// NULL if not opened.
    device_intf: UnsafeCell<*mut *mut iokit_sys::IOUSBDeviceInterface>,
}

// SAFETY: MacOsDevice is used from a single thread at a time; the raw pointer
// is not shared across threads concurrently.
unsafe impl Send for MacOsDevice {}

impl MacOsDevice {
    /// Open a device by the path produced by enumerate (format: "iokit:bus=B,addr=A,...").
    fn open(path: &str) -> Result<Self, UsbError> {
        let (bus, addr) = parse_iokit_path(path)?;

        // Re-enumerate to find the matching io_service_t.
        let service = find_service_by_bus_addr(bus, addr)
            .ok_or(UsbError::DeviceNotFound)?;

        // Create IOCFPlugin to obtain IOUSBDeviceInterface.
        let intf_ptr = create_device_interface(service).map_err(|e| {
            // SAFETY: service is a valid io_object_t.
            unsafe { iokit_sys::IOObjectRelease(service) };
            e
        })?;

        // SAFETY: service is a valid io_object_t; release after plugin creation.
        unsafe { iokit_sys::IOObjectRelease(service) };

        // Open the device (USBDeviceOpen).
        // SAFETY: intf_ptr is a valid IOUSBDeviceInterface** obtained above.
        let kr = unsafe { (**intf_ptr).USBDeviceOpen.map(|f| f(intf_ptr as _)).unwrap_or(0xe00002d6) };
        if kr != kIOReturnSuccess && kr != 0xe00002d5 {
            // 0xe00002d5 = kIOReturnExclusiveAccess — another process has it, treat as permission denied
            // 0xe00002d6 = kIOReturnNotOpen — shouldn't happen, but guard
            // On exclusive access we still proceed — read-only ops (GET_DESCRIPTOR) work without open on some versions
            if kr == 0xe00002d5 {
                log::warn!("IOUSBDevice: exclusive access denied for {path}; descriptor reads may fail");
            } else {
                return Err(UsbError::Io(std::io::Error::from_raw_os_error(kr as i32)));
            }
        }

        Ok(Self {
            path: path.to_owned(),
            device_intf: UnsafeCell::new(intf_ptr),
        })
    }

    /// Issue a synchronous control request via IOUSBDeviceInterface::DeviceRequest.
    fn raw_control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        buf: &mut [u8],
        timeout_ms: u32,
    ) -> Result<usize, UsbError> {
        // SAFETY: device_intf is valid and set during open(); we are single-threaded.
        let intf_ptr = unsafe { *self.device_intf.get() };
        if intf_ptr.is_null() {
            return Err(UsbError::InvalidHandle);
        }

        let mut req = iokit_sys::IOUSBDevRequestTO {
            bmRequestType: request_type,
            bRequest: request,
            wValue: value,
            wIndex: index,
            wLength: buf.len() as u16,
            pData: buf.as_mut_ptr() as *mut std::ffi::c_void,
            wLenDone: 0,
            noDataTimeout: timeout_ms,
            completionTimeout: timeout_ms,
        };

        // SAFETY: intf_ptr is valid; req struct is correctly initialised.
        let kr = unsafe {
            (**intf_ptr)
                .DeviceRequestTO
                .map(|f| f(intf_ptr as _, &mut req))
                .unwrap_or(0xe00002c5) // kIOReturnUnsupported
        };

        if kr != kIOReturnSuccess {
            return Err(iokit_kr_to_usb_error(kr));
        }

        Ok(req.wLenDone as usize)
    }
}

impl Drop for MacOsDevice {
    fn drop(&mut self) {
        // SAFETY: device_intf is set during open(); we are the only owner.
        let intf_ptr = unsafe { *self.device_intf.get() };
        if !intf_ptr.is_null() {
            unsafe {
                // Close and release the device interface.
                let _ = (**intf_ptr).USBDeviceClose.map(|f| f(intf_ptr as _));
                (**intf_ptr).Release.map(|f| f(intf_ptr as _));
            }
        }
    }
}

impl UsbDevice for MacOsDevice {
    fn read_device_descriptor(&self) -> Result<DeviceDescriptor, UsbError> {
        let mut buf = [0u8; 18];
        let n = self.raw_control(
            REQ_TYPE_IN_STD_DEV,
            GET_DESCRIPTOR,
            0x0100,
            0x0000,
            &mut buf,
            5000,
        )?;
        if n < 18 {
            return Err(UsbError::InvalidDescriptor);
        }
        DeviceDescriptor::from_bytes(&buf)
    }

    fn read_config_descriptor(&self, index: u8) -> Result<ConfigDescriptor, UsbError> {
        // Pass 1: read 9-byte header for wTotalLength
        let mut hdr = [0u8; 9];
        let req_value = (0x02u16 << 8) | index as u16;
        let n = self.raw_control(REQ_TYPE_IN_STD_DEV, GET_DESCRIPTOR, req_value, 0, &mut hdr, 5000)?;
        if n < 9 {
            return Err(UsbError::InvalidDescriptor);
        }
        let total_len = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
        if total_len < 9 {
            return Err(UsbError::InvalidDescriptor);
        }

        // Pass 2: read full descriptor
        let mut full = vec![0u8; total_len];
        self.raw_control(REQ_TYPE_IN_STD_DEV, GET_DESCRIPTOR, req_value, 0, &mut full, 5000)?;
        ConfigDescriptor::from_bytes(&full)
    }

    fn read_string_descriptor(&self, index: u8, lang: u16) -> Result<String, UsbError> {
        let mut buf = [0u8; 255];
        let req_value = (0x03u16 << 8) | index as u16;
        let n = self.raw_control(REQ_TYPE_IN_STD_DEV, GET_DESCRIPTOR, req_value, lang, &mut buf, 5000)?;
        if n < 2 {
            return Err(UsbError::InvalidDescriptor);
        }
        let str_len = buf[0] as usize;
        if str_len < 2 || str_len > n {
            return Err(UsbError::InvalidDescriptor);
        }
        let chars: Vec<u16> = buf[2..str_len]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(String::from_utf16_lossy(&chars).to_string())
    }

    fn claim_interface(&mut self, interface: u8) -> Result<(), UsbError> {
        // IOUSBLib does not have a separate claim step at the device level;
        // interfaces are accessed by creating an IOUSBInterface service plugin.
        // For Phase 1 (control transfers on EP0), this is a no-op.
        log::debug!("macOS: claim_interface({interface}) — no-op at device level");
        Ok(())
    }

    fn release_interface(&mut self, interface: u8) -> Result<(), UsbError> {
        log::debug!("macOS: release_interface({interface}) — no-op at device level");
        Ok(())
    }

    fn control_transfer(
        &self,
        setup: ControlSetup,
        data: Option<&mut [u8]>,
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let is_in = (setup.request_type & 0x80) != 0;

        if is_in {
            let buf = data
                .ok_or_else(|| UsbError::Other("IN transfer requires a data buffer".into()))?;
            self.raw_control(
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                buf,
                timeout_ms,
            )
        } else {
            let len = setup.length as usize;
            match data {
                Some(buf) => self.raw_control(
                    setup.request_type,
                    setup.request,
                    setup.value,
                    setup.index,
                    buf,
                    timeout_ms,
                ),
                None => {
                    let mut empty = vec![0u8; len];
                    self.raw_control(
                        setup.request_type,
                        setup.request,
                        setup.value,
                        setup.index,
                        &mut empty,
                        timeout_ms,
                    )
                }
            }
        }
    }
}

// -----------------------------------------------------------------------
// IOKit helpers
// -----------------------------------------------------------------------

/// Parse the "iokit:bus=B,addr=A,..." path back into (bus, addr).
fn parse_iokit_path(path: &str) -> Result<(u8, u8), UsbError> {
    let bus = path
        .split(',')
        .find(|s| s.starts_with("bus="))
        .and_then(|s| s.strip_prefix("bus="))
        .and_then(|s| s.parse::<u8>().ok())
        .ok_or_else(|| UsbError::Other(format!("invalid iokit path: {path}")))?;

    let addr = path
        .split(',')
        .find(|s| s.starts_with("addr="))
        .and_then(|s| s.strip_prefix("addr="))
        .and_then(|s| s.parse::<u8>().ok())
        .ok_or_else(|| UsbError::Other(format!("invalid iokit path: {path}")))?;

    Ok((bus, addr))
}

/// Find an io_service_t for the device at (bus, address). Caller must release.
fn find_service_by_bus_addr(bus: u8, addr: u8) -> Option<io_service_t> {
    let matching_dict = unsafe {
        iokit_sys::IOServiceMatching(K_IO_USB_DEVICE_CLASS_NAME.as_ptr())
    };
    if matching_dict.is_null() {
        return None;
    }

    let mut iter: io_iterator_t = 0;
    let kr = unsafe {
        iokit_sys::IOServiceGetMatchingServices(kIOMasterPortDefault, matching_dict, &mut iter)
    };
    if kr != kIOReturnSuccess {
        return None;
    }

    loop {
        let service: io_service_t = unsafe { iokit_sys::IOIteratorNext(iter) };
        if service == 0 {
            break;
        }

        let svc_bus = iokit_integer_property(service, "USBBusNumber").unwrap_or(-1) as i16;
        let svc_addr = iokit_integer_property(service, "USB Address").unwrap_or(-1) as i16;

        if svc_bus == bus as i16 && svc_addr == addr as i16 {
            unsafe { iokit_sys::IOObjectRelease(iter) };
            return Some(service);
        }

        unsafe { iokit_sys::IOObjectRelease(service) };
    }

    unsafe { iokit_sys::IOObjectRelease(iter) };
    None
}

/// Create an IOUSBDeviceInterface** for a given io_service_t.
fn create_device_interface(
    service: io_service_t,
) -> Result<*mut *mut iokit_sys::IOUSBDeviceInterface, UsbError> {
    use iokit_sys::{
        kIOCFPlugInInterfaceID, kIOUSBDeviceUserClientTypeID, IOCreatePlugInInterfaceForService,
        IOCFPlugInInterface,
    };

    let mut plugin: *mut *mut IOCFPlugInInterface = std::ptr::null_mut();
    let mut score: i32 = 0;

    // SAFETY: service is valid; plugin and score are valid out-pointers.
    let kr = unsafe {
        IOCreatePlugInInterfaceForService(
            service,
            kIOUSBDeviceUserClientTypeID(),
            kIOCFPlugInInterfaceID(),
            &mut plugin,
            &mut score,
        )
    };

    if kr != kIOReturnSuccess || plugin.is_null() {
        return Err(UsbError::Other(format!(
            "IOCreatePlugInInterfaceForService failed: {kr:#x}"
        )));
    }

    // QueryInterface for IOUSBDeviceInterface.
    // SAFETY: plugin is a valid IOCFPlugInInterface**.
    let mut device_intf: *mut *mut iokit_sys::IOUSBDeviceInterface = std::ptr::null_mut();
    let kr = unsafe {
        (**plugin).QueryInterface.map(|qi| {
            qi(
                plugin as _,
                iokit_sys::CFUUIDGetUUIDBytes(iokit_sys::kIOUSBDeviceInterfaceID()),
                &mut device_intf as *mut _ as *mut _,
            )
        }).unwrap_or(0xe00002c5 as i32)
    };

    // Release the plugin interface regardless of QueryInterface result.
    // SAFETY: plugin is valid; Release decrements refcount.
    unsafe { (**plugin).Release.map(|r| r(plugin as _)) };

    if kr != 0 || device_intf.is_null() {
        return Err(UsbError::Other(format!(
            "QueryInterface for IOUSBDeviceInterface failed: {kr:#x}"
        )));
    }

    Ok(device_intf)
}

/// Convert an IOReturn error code to a UsbError.
fn iokit_kr_to_usb_error(kr: u32) -> UsbError {
    match kr {
        0xe000404f => UsbError::Stall,       // kIOUSBPipeStalled
        0xe0004051 => UsbError::Timeout,     // kIOUSBTransactionTimeout
        0xe00002ed => UsbError::DeviceNotFound, // kIOReturnNoDevice
        0xe00002c1 => UsbError::PermissionDenied, // kIOReturnNotPermitted
        other => UsbError::Other(format!("IOReturn {other:#x}")),
    }
}
