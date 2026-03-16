use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::sync::Mutex;
use std::time::Duration;

use core_foundation::base::{kCFAllocatorDefault, TCFType};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::base::CFTypeRef;
use core_foundation_sys::uuid::{CFUUIDBytes, CFUUIDGetUUIDBytes, CFUUIDRef};
use IOKit_sys as iokit_sys;
use iokit_sys::{io_iterator_t, io_service_t, kIOReturnSuccess};
// Note: kIOMasterPortDefault is often 0 (MACH_PORT_NULL) or specifically defined.
// In newer SDKs it's kIOMainPortDefault.
const K_IO_MASTER_PORT_DEFAULT: u32 = 0;

use crate::core::{
    ConfigDescriptor, ControlSetup, DeviceDescriptor, DeviceInfo, EndpointInfo, PipePolicy,
    PipePolicyKind,
};
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
// Manual IOUSBLib declarations missing from IOKit-sys 0.1.x
// -----------------------------------------------------------------------

type IOReturn = i32;
type HRESULT = i32;
type ULONG = u32;
const K_IORETURN_EXCLUSIVE_ACCESS: IOReturn = 0xe00002d5u32 as i32;
const K_IORETURN_NOT_OPEN: IOReturn = 0xe00002d6u32 as i32;
const K_IORETURN_UNSUPPORTED: IOReturn = 0xe00002c5u32 as i32;
const K_IORETURN_NO_DEVICE: IOReturn = 0xe00002edu32 as i32;

const K_IOUSB_FIND_INTERFACE_DONT_CARE: u16 = 0xFFFF;

#[repr(C)]
struct IOUSBFindInterfaceRequest {
    bInterfaceClass: u16,
    bInterfaceSubClass: u16,
    bInterfaceProtocol: u16,
    bAlternateSetting: u16,
}

#[repr(C)]
struct IOUSBDevRequestTO {
    bmRequestType: u8,
    bRequest: u8,
    wValue: u16,
    wIndex: u16,
    wLength: u16,
    pData: *mut std::ffi::c_void,
    wLenDone: u16,
    noDataTimeout: u32,
    completionTimeout: u32,
}

#[repr(C)]
struct IOCFPlugInInterface {
    QueryInterface: Option<
        unsafe extern "C" fn(
            this: *mut std::ffi::c_void,
            iid: CFUUIDBytes,
            ppv: *mut *mut std::ffi::c_void,
        ) -> HRESULT,
    >,
    AddRef: Option<unsafe extern "C" fn(this: *mut std::ffi::c_void) -> ULONG>,
    Release: Option<unsafe extern "C" fn(this: *mut std::ffi::c_void) -> ULONG>,
}

#[repr(C)]
struct IOUSBDeviceInterface {
    QueryInterface: Option<
        unsafe extern "C" fn(
            this: *mut std::ffi::c_void,
            iid: CFUUIDBytes,
            ppv: *mut *mut std::ffi::c_void,
        ) -> HRESULT,
    >,
    AddRef: Option<unsafe extern "C" fn(this: *mut std::ffi::c_void) -> ULONG>,
    Release: Option<unsafe extern "C" fn(this: *mut std::ffi::c_void) -> ULONG>,
    USBDeviceOpen: Option<unsafe extern "C" fn(this: *mut std::ffi::c_void) -> IOReturn>,
    USBDeviceClose: Option<unsafe extern "C" fn(this: *mut std::ffi::c_void) -> IOReturn>,
    CreateInterfaceIterator: Option<
        unsafe extern "C" fn(
            this: *mut std::ffi::c_void,
            req: *mut IOUSBFindInterfaceRequest,
            iter: *mut io_iterator_t,
        ) -> IOReturn,
    >,
    DeviceRequestTO: Option<
        unsafe extern "C" fn(
            this: *mut std::ffi::c_void,
            req: *mut IOUSBDevRequestTO,
        ) -> IOReturn,
    >,
}

#[repr(C)]
struct IOUSBInterfaceInterface {
    QueryInterface: Option<
        unsafe extern "C" fn(
            this: *mut std::ffi::c_void,
            iid: CFUUIDBytes,
            ppv: *mut *mut std::ffi::c_void,
        ) -> HRESULT,
    >,
    AddRef: Option<unsafe extern "C" fn(this: *mut std::ffi::c_void) -> ULONG>,
    Release: Option<unsafe extern "C" fn(this: *mut std::ffi::c_void) -> ULONG>,
    USBInterfaceOpen: Option<unsafe extern "C" fn(this: *mut std::ffi::c_void) -> IOReturn>,
    USBInterfaceClose: Option<unsafe extern "C" fn(this: *mut std::ffi::c_void) -> IOReturn>,
    GetNumEndpoints: Option<
        unsafe extern "C" fn(this: *mut std::ffi::c_void, num: *mut u8) -> IOReturn,
    >,
    GetPipeProperties: Option<
        unsafe extern "C" fn(
            this: *mut std::ffi::c_void,
            pipe_ref: u8,
            direction: *mut u8,
            number: *mut u8,
            transfer_type: *mut u8,
            max_packet_size: *mut u16,
            interval: *mut u8,
        ) -> IOReturn,
    >,
    ReadPipe: Option<
        unsafe extern "C" fn(
            this: *mut std::ffi::c_void,
            pipe_ref: u8,
            buf: *mut std::ffi::c_void,
            size: *mut u32,
        ) -> IOReturn,
    >,
    WritePipe: Option<
        unsafe extern "C" fn(
            this: *mut std::ffi::c_void,
            pipe_ref: u8,
            buf: *mut std::ffi::c_void,
            size: u32,
        ) -> IOReturn,
    >,
    GetInterfaceNumber: Option<
        unsafe extern "C" fn(
            this: *mut std::ffi::c_void,
            interface_number: *mut u8,
        ) -> IOReturn,
    >,
    SetAlternateInterface: Option<
        unsafe extern "C" fn(
            this: *mut std::ffi::c_void,
            alt_setting: u8,
        ) -> IOReturn,
    >,
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOCreatePlugInInterfaceForService(
        service: io_service_t,
        plugin_type: CFUUIDRef,
        interface_type: CFUUIDRef,
        plugin: *mut *mut *mut IOCFPlugInInterface,
        score: *mut i32,
    ) -> IOReturn;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFUUIDCreateWithBytes(
        alloc: *const std::ffi::c_void,
        b0: u8, b1: u8, b2: u8, b3: u8, b4: u8, b5: u8, b6: u8, b7: u8,
        b8: u8, b9: u8, b10: u8, b11: u8, b12: u8, b13: u8, b14: u8, b15: u8,
    ) -> CFUUIDRef;
}

fn k_io_cf_plugin_interface_id() -> CFUUIDRef {
    unsafe { CFUUIDCreateWithBytes(std::ptr::null_mut(), 0xC2, 0x44, 0xE8, 0x58, 0x10, 0x9C, 0x11, 0xD4, 0x91, 0xD4, 0x00, 0x50, 0xE4, 0xC0, 0x2F, 0xDC) }
}
fn k_io_usb_device_user_client_type_id() -> CFUUIDRef {
    unsafe { CFUUIDCreateWithBytes(std::ptr::null_mut(), 0x9D, 0x5D, 0x72, 0x1A, 0x1E, 0xBD, 0x11, 0xD3, 0x83, 0x9C, 0x00, 0x05, 0x02, 0x8F, 0x18, 0xD5) }
}
fn k_io_usb_interface_user_client_type_id() -> CFUUIDRef {
    unsafe { CFUUIDCreateWithBytes(std::ptr::null_mut(), 0x2D, 0x97, 0x86, 0xC6, 0x9E, 0xF3, 0x11, 0xD4, 0xAD, 0x51, 0x00, 0x05, 0x02, 0x8F, 0x18, 0xD5) }
}

// Manual UUID byte definitions for standard IOKit USB interfaces.
// These are used when the framework symbols are not easily linked.
const K_IO_CF_PLUGIN_INTERFACE_ID_BYTES: CFUUIDBytes = CFUUIDBytes {
    byte0: 0xC2, byte1: 0x44, byte2: 0xE8, byte3: 0x58, byte4: 0x10, byte5: 0x9C, byte6: 0x11, byte7: 0xD4,
    byte8: 0x91, byte9: 0xD4, byte10: 0x00, byte11: 0x50, byte12: 0xE4, byte13: 0xC0, byte14: 0x2F, byte15: 0xDC,
};
const K_IO_USB_DEVICE_INTERFACE_ID_BYTES: CFUUIDBytes = CFUUIDBytes {
    byte0: 0x5E, byte1: 0xAD, byte2: 0x81, byte3: 0x51, byte4: 0x50, byte5: 0xBC, byte6: 0x11, byte7: 0xD4,
    byte8: 0xA7, byte9: 0x1C, byte10: 0x00, byte11: 0x05, byte12: 0x02, byte13: 0x8F, byte14: 0x18, byte15: 0xD5,
};
const K_IO_USB_INTERFACE_INTERFACE_ID_BYTES: CFUUIDBytes = CFUUIDBytes {
    byte0: 0x23, byte1: 0x83, byte2: 0x67, byte3: 0x61, byte4: 0x9E, byte5: 0x86, byte6: 0x11, byte7: 0xD4,
    byte8: 0xB3, byte9: 0x24, byte10: 0x00, byte11: 0x05, byte12: 0x02, byte13: 0x8F, byte14: 0x18, byte15: 0xD5,
};

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
        iokit_sys::IOServiceGetMatchingServices(K_IO_MASTER_PORT_DEFAULT, matching_dict, &mut iter)
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
            std::ptr::null_mut(),
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
            std::ptr::null_mut(),
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
    device_intf: UnsafeCell<*mut *mut IOUSBDeviceInterface>,
    /// Interfaces currently claimed by the library.
    claimed_interfaces: HashSet<u8>,
    /// Endpoint address -> pipe index (1-based) discovered from interface scans.
    pipe_cache: Mutex<HashMap<u8, u8>>,
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
        let kr = unsafe {
            (**intf_ptr)
                .USBDeviceOpen
                .map(|f| f(intf_ptr as _))
                .unwrap_or(K_IORETURN_NOT_OPEN)
        };
        if kr != kIOReturnSuccess && kr != K_IORETURN_EXCLUSIVE_ACCESS {
            // 0xe00002d5 = kIOReturnExclusiveAccess — another process has it, treat as permission denied
            // 0xe00002d6 = kIOReturnNotOpen — shouldn't happen, but guard
            // On exclusive access we still proceed — read-only ops (GET_DESCRIPTOR) work without open on some versions
            if kr == K_IORETURN_EXCLUSIVE_ACCESS {
                log::warn!("IOUSBDevice: exclusive access denied for {path}; descriptor reads may fail");
            } else {
                return Err(UsbError::Io(std::io::Error::from_raw_os_error(kr as i32)));
            }
        }

        Ok(Self {
            path: path.to_owned(),
            device_intf: UnsafeCell::new(intf_ptr),
            claimed_interfaces: HashSet::new(),
            pipe_cache: Mutex::new(HashMap::new()),
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

        let mut req = IOUSBDevRequestTO {
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
                .unwrap_or(K_IORETURN_UNSUPPORTED) // kIOReturnUnsupported
        };

        if kr != kIOReturnSuccess {
            return Err(iokit_kr_to_usb_error(kr as u32));
        }

        Ok(req.wLenDone as usize)
    }

    /// GET_INTERFACE via standard control request.
    fn get_interface_alt_setting(&self, interface: u8, timeout_ms: u32) -> Result<u8, UsbError> {
        let mut buf = [0u8; 1];
        // IN | Standard | Interface, bRequest=GET_INTERFACE
        let n = self.raw_control(0x81, 0x0A, 0, interface as u16, &mut buf, timeout_ms)?;
        if n < 1 {
            return Err(UsbError::InvalidDescriptor);
        }
        Ok(buf[0])
    }

    /// SET_INTERFACE via standard control request.
    fn set_interface_alt_setting(
        &self,
        interface: u8,
        alt_setting: u8,
        timeout_ms: u32,
    ) -> Result<(), UsbError> {
        // OUT | Standard | Interface, bRequest=SET_INTERFACE
        let mut empty = [];
        self.raw_control(
            0x01,
            0x0B,
            alt_setting as u16,
            interface as u16,
            &mut empty,
            timeout_ms,
        )?;
        Ok(())
    }

    fn interface_exists(&self, interface: u8) -> Result<bool, UsbError> {
        let cfg = self.read_config_descriptor(0)?;
        Ok(cfg
            .interfaces
            .iter()
            .any(|iface| iface.interface_number == interface))
    }

    fn open_interface_for_endpoint(
        &self,
        endpoint: u8,
    ) -> Result<*mut *mut IOUSBInterfaceInterface, UsbError> {
        // SAFETY: device_intf is initialized in open(); backend uses this pointer as an opaque handle.
        let device_intf = unsafe { *self.device_intf.get() };
        if device_intf.is_null() {
            return Err(UsbError::InvalidHandle);
        }

        let mut find_req = IOUSBFindInterfaceRequest {
            bInterfaceClass: K_IOUSB_FIND_INTERFACE_DONT_CARE,
            bInterfaceSubClass: K_IOUSB_FIND_INTERFACE_DONT_CARE,
            bInterfaceProtocol: K_IOUSB_FIND_INTERFACE_DONT_CARE,
            bAlternateSetting: K_IOUSB_FIND_INTERFACE_DONT_CARE,
        };
        let mut iter: io_iterator_t = 0;

        // SAFETY: device_intf is a valid IOUSBDeviceInterface** and pointers are valid out-params.
        let create_iter_kr = unsafe {
            (**device_intf)
                .CreateInterfaceIterator
                .map(|f| f(device_intf as _, &mut find_req, &mut iter))
                .unwrap_or(K_IORETURN_UNSUPPORTED)
        };
        if create_iter_kr != kIOReturnSuccess {
            return Err(iokit_pipe_kr_to_usb_error(create_iter_kr));
        }

        if iter == 0 {
            return Err(UsbError::InvalidHandle);
        }

        loop {
            // SAFETY: iter is valid while held in this function.
            let service = unsafe { iokit_sys::IOIteratorNext(iter) };
            if service == 0 {
                break;
            }

            let mut plugin: *mut *mut IOCFPlugInInterface = std::ptr::null_mut();
            let mut score: i32 = 0;

            // SAFETY: service is valid and out-pointers are valid.
            let create_plugin_kr = unsafe {
                IOCreatePlugInInterfaceForService(
                    service,
                    k_io_usb_interface_user_client_type_id(),
                    k_io_cf_plugin_interface_id(),
                    &mut plugin,
                    &mut score,
                )
            };
            // SAFETY: service came from IOIteratorNext and must be released once consumed.
            unsafe { iokit_sys::IOObjectRelease(service) };

            if create_plugin_kr != kIOReturnSuccess {
                // SAFETY: iter is valid and owned by this function.
                unsafe { iokit_sys::IOObjectRelease(iter) };
                return Err(iokit_pipe_kr_to_usb_error(create_plugin_kr));
            }
            if plugin.is_null() {
                // SAFETY: iter is valid and owned by this function.
                unsafe { iokit_sys::IOObjectRelease(iter) };
                return Err(UsbError::InvalidHandle);
            }

            let mut interface_intf: *mut *mut IOUSBInterfaceInterface = std::ptr::null_mut();
            // SAFETY: plugin is valid; QueryInterface writes to interface_intf on success.
            let query_kr = unsafe {
                (**plugin)
                    .QueryInterface
                    .map(|qi| {
                        qi(
                            plugin as _,
                            K_IO_USB_INTERFACE_INTERFACE_ID_BYTES,
                            &mut interface_intf as *mut _ as *mut _,
                        )
                    })
                    .unwrap_or(K_IORETURN_UNSUPPORTED)
            };
            // SAFETY: plugin is valid and must always be released after QueryInterface.
            unsafe { (**plugin).Release.map(|r| r(plugin as _)) };

            if query_kr != 0 || interface_intf.is_null() {
                // SAFETY: iter is valid and owned by this function.
                unsafe { iokit_sys::IOObjectRelease(iter) };
                return Err(UsbError::Other(format!(
                    "QueryInterface for IOUSBInterfaceInterface failed: {query_kr:#x}"
                )));
            }

            // SAFETY: interface_intf is valid and points to an interface vtable.
            let open_kr = unsafe {
                (**interface_intf)
                    .USBInterfaceOpen
                    .map(|f| f(interface_intf as _))
                    .unwrap_or(K_IORETURN_UNSUPPORTED)
            };
            if open_kr != kIOReturnSuccess {
                close_and_release_interface(interface_intf);
                // SAFETY: iter is valid and owned by this function.
                unsafe { iokit_sys::IOObjectRelease(iter) };
                return Err(iokit_pipe_kr_to_usb_error(open_kr));
            }

            let mut num_endpoints: u8 = 0;
            // SAFETY: interface_intf is open/valid and num_endpoints is a valid out-pointer.
            let num_ep_kr = unsafe {
                (**interface_intf)
                    .GetNumEndpoints
                    .map(|f| f(interface_intf as _, &mut num_endpoints))
                    .unwrap_or(K_IORETURN_UNSUPPORTED)
            };
            if num_ep_kr != kIOReturnSuccess {
                close_and_release_interface(interface_intf);
                // SAFETY: iter is valid and owned by this function.
                unsafe { iokit_sys::IOObjectRelease(iter) };
                return Err(iokit_pipe_kr_to_usb_error(num_ep_kr));
            }

            let mut discovered = HashMap::new();
            for pipe_ref in 1..=num_endpoints {
                let mut direction: u8 = 0;
                let mut number: u8 = 0;
                let mut transfer_type: u8 = 0;
                let mut max_packet_size: u16 = 0;
                let mut interval: u8 = 0;

                // SAFETY: interface_intf is valid and all out-pointers are initialized above.
                let pipe_kr = unsafe {
                    (**interface_intf)
                        .GetPipeProperties
                        .map(|f| {
                            f(
                                interface_intf as _,
                                pipe_ref,
                                &mut direction,
                                &mut number,
                                &mut transfer_type,
                                &mut max_packet_size,
                                &mut interval,
                            )
                        })
                        .unwrap_or(K_IORETURN_UNSUPPORTED)
                };
                if pipe_kr != kIOReturnSuccess {
                    close_and_release_interface(interface_intf);
                    // SAFETY: iter is valid and owned by this function.
                    unsafe { iokit_sys::IOObjectRelease(iter) };
                    return Err(iokit_pipe_kr_to_usb_error(pipe_kr));
                }

                let endpoint_address = (number & 0x0f) | if direction != 0 { 0x80 } else { 0x00 };
                discovered.insert(endpoint_address, pipe_ref);
            }

            let found = discovered.contains_key(&endpoint);
            let mut cache = match self.pipe_cache.lock() {
                Ok(cache) => cache,
                Err(_) => {
                    close_and_release_interface(interface_intf);
                    // SAFETY: iter is valid and owned by this function.
                    unsafe { iokit_sys::IOObjectRelease(iter) };
                    return Err(UsbError::Other("macOS pipe cache lock poisoned".into()));
                }
            };
            cache.extend(discovered.into_iter());
            drop(cache);

            if found {
                // SAFETY: iter is valid and owned by this function.
                unsafe { iokit_sys::IOObjectRelease(iter) };
                return Ok(interface_intf);
            }

            close_and_release_interface(interface_intf);
        }

        // SAFETY: iter is valid and owned by this function.
        unsafe { iokit_sys::IOObjectRelease(iter) };
        Err(UsbError::Other(format!(
            "endpoint {endpoint:#04x} not found on any interface"
        )))
    }

    /// Find and open the IOUSBInterfaceInterface matching `interface_number`.
    /// The returned pointer is already opened; the caller is responsible for
    /// calling `close_and_release_interface` on every exit path.
    fn find_interface(
        &self,
        interface_number: u8,
    ) -> Result<*mut *mut IOUSBInterfaceInterface, UsbError> {
        // SAFETY: device_intf is initialised in open(); single-threaded access.
        let device_intf = unsafe { *self.device_intf.get() };
        if device_intf.is_null() {
            return Err(UsbError::InvalidHandle);
        }

        let mut find_req = IOUSBFindInterfaceRequest {
            bInterfaceClass: K_IOUSB_FIND_INTERFACE_DONT_CARE,
            bInterfaceSubClass: K_IOUSB_FIND_INTERFACE_DONT_CARE,
            bInterfaceProtocol: K_IOUSB_FIND_INTERFACE_DONT_CARE,
            bAlternateSetting: K_IOUSB_FIND_INTERFACE_DONT_CARE,
        };
        let mut iter: io_iterator_t = 0;

        // SAFETY: device_intf is a valid IOUSBDeviceInterface**; both out-params are valid.
        let create_iter_kr = unsafe {
            (**device_intf)
                .CreateInterfaceIterator
                .map(|f| f(device_intf as _, &mut find_req, &mut iter))
                .unwrap_or(K_IORETURN_UNSUPPORTED)
        };
        if create_iter_kr != kIOReturnSuccess {
            return Err(iokit_pipe_kr_to_usb_error(create_iter_kr));
        }
        if iter == 0 {
            return Err(UsbError::InvalidHandle);
        }

        loop {
            // SAFETY: iter is a valid io_iterator_t owned by this function.
            let service = unsafe { iokit_sys::IOIteratorNext(iter) };
            if service == 0 {
                break;
            }

            let mut plugin: *mut *mut IOCFPlugInInterface = std::ptr::null_mut();
            let mut score: i32 = 0;

            // SAFETY: service is valid and out-pointers are valid.
            let create_plugin_kr = unsafe {
                IOCreatePlugInInterfaceForService(
                    service,
                    k_io_usb_interface_user_client_type_id(),
                    k_io_cf_plugin_interface_id(),
                    &mut plugin,
                    &mut score,
                )
            };
            // SAFETY: service came from IOIteratorNext and must be released once consumed.
            unsafe { iokit_sys::IOObjectRelease(service) };

            if create_plugin_kr != kIOReturnSuccess || plugin.is_null() {
                // SAFETY: iter is owned by this function; release before returning.
                unsafe { iokit_sys::IOObjectRelease(iter) };
                return Err(iokit_pipe_kr_to_usb_error(create_plugin_kr));
            }

            let mut interface_intf: *mut *mut IOUSBInterfaceInterface = std::ptr::null_mut();
            // SAFETY: plugin is valid; QueryInterface writes to interface_intf on success.
            let query_kr = unsafe {
                (**plugin)
                    .QueryInterface
                    .map(|qi| {
                        qi(
                            plugin as _,
                            K_IO_USB_INTERFACE_INTERFACE_ID_BYTES,
                            &mut interface_intf as *mut _ as *mut _,
                        )
                    })
                    .unwrap_or(K_IORETURN_UNSUPPORTED)
            };
            // SAFETY: plugin must always be released after QueryInterface.
            unsafe { (**plugin).Release.map(|r| r(plugin as _)) };

            if query_kr != 0 || interface_intf.is_null() {
                // SAFETY: iter is owned by this function; release before returning.
                unsafe { iokit_sys::IOObjectRelease(iter) };
                return Err(UsbError::Other(format!(
                    "QueryInterface for IOUSBInterfaceInterface failed: {query_kr:#x}"
                )));
            }

            // SAFETY: interface_intf is valid and points to an interface vtable.
            let open_kr = unsafe {
                (**interface_intf)
                    .USBInterfaceOpen
                    .map(|f| f(interface_intf as _))
                    .unwrap_or(K_IORETURN_UNSUPPORTED)
            };
            if open_kr != kIOReturnSuccess {
                close_and_release_interface(interface_intf);
                // SAFETY: iter is owned by this function; release before returning.
                unsafe { iokit_sys::IOObjectRelease(iter) };
                return Err(iokit_pipe_kr_to_usb_error(open_kr));
            }

            let mut found_number: u8 = 0;
            // SAFETY: interface_intf is open/valid; found_number is a valid out-pointer.
            let num_kr = unsafe {
                (**interface_intf)
                    .GetInterfaceNumber
                    .map(|f| f(interface_intf as _, &mut found_number))
                    .unwrap_or(K_IORETURN_UNSUPPORTED)
            };
            if num_kr != kIOReturnSuccess {
                close_and_release_interface(interface_intf);
                // SAFETY: iter is owned by this function; release before returning.
                unsafe { iokit_sys::IOObjectRelease(iter) };
                return Err(iokit_pipe_kr_to_usb_error(num_kr));
            }

            if found_number == interface_number {
                // Match found — release iterator and return the open interface pointer.
                // SAFETY: iter is owned by this function; release before returning.
                unsafe { iokit_sys::IOObjectRelease(iter) };
                return Ok(interface_intf);
            }

            // Not a match — close and release this interface, continue searching.
            close_and_release_interface(interface_intf);
        }

        // Iterator exhausted; no interface with the requested number exists.
        // SAFETY: iter is owned by this function; release on exit.
        unsafe { iokit_sys::IOObjectRelease(iter) };
        Err(UsbError::Other(format!(
            "interface {interface_number} not found"
        )))
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
        if self.claimed_interfaces.contains(&interface) {
            return Ok(());
        }

        if !self.interface_exists(interface)? {
            return Err(UsbError::Other(format!(
                "interface {interface} not found in configuration"
            )));
        }

        // Validate the interface is reachable by querying current alt-setting.
        let alt = self.get_interface_alt_setting(interface, 1000)?;
        log::debug!("macOS: claimed interface {interface} (alt={alt})");
        self.claimed_interfaces.insert(interface);
        Ok(())
    }

    fn release_interface(&mut self, interface: u8) -> Result<(), UsbError> {
        if !self.claimed_interfaces.contains(&interface) {
            return Ok(());
        }

        // Best-effort return to alt 0 on release.
        self.set_interface_alt_setting(interface, 0, 1000)?;
        self.claimed_interfaces.remove(&interface);
        log::debug!("macOS: released interface {interface}");
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

    fn bulk_read(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        if buf.is_empty() {
            return Ok(0);
        }

        let mut transfer_size = u32::try_from(buf.len())
            .map_err(|_| UsbError::Other("buffer length exceeds macOS pipe size".into()))?;

        let interface_intf = self.open_interface_for_endpoint(endpoint)?;
        let pipe_ref = match self.pipe_cache.lock() {
            Ok(cache) => match cache.get(&endpoint).copied() {
                Some(pipe_ref) => pipe_ref,
                None => {
                    close_and_release_interface(interface_intf);
                    return Err(UsbError::Other(format!(
                        "endpoint {endpoint:#04x} has no mapped pipe"
                    )));
                }
            },
            Err(_) => {
                close_and_release_interface(interface_intf);
                return Err(UsbError::Other("macOS pipe cache lock poisoned".into()));
            }
        };

        // SAFETY: interface_intf is open/valid; buf points to writable memory for transfer_size bytes.
        let kr = unsafe {
            (**interface_intf)
                .ReadPipe
                .map(|f| {
                    f(
                        interface_intf as _,
                        pipe_ref,
                        buf.as_mut_ptr() as *mut std::ffi::c_void,
                        &mut transfer_size,
                    )
                })
                .unwrap_or(K_IORETURN_UNSUPPORTED)
        };

        let result = if kr == kIOReturnSuccess {
            Ok(transfer_size as usize)
        } else {
            Err(iokit_pipe_kr_to_usb_error(kr))
        };

        close_and_release_interface(interface_intf);
        result
    }

    fn bulk_write(
        &self,
        endpoint: u8,
        buf: &[u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        if buf.is_empty() {
            return Ok(0);
        }

        let transfer_size = u32::try_from(buf.len())
            .map_err(|_| UsbError::Other("buffer length exceeds macOS pipe size".into()))?;

        let interface_intf = self.open_interface_for_endpoint(endpoint)?;
        let pipe_ref = match self.pipe_cache.lock() {
            Ok(cache) => match cache.get(&endpoint).copied() {
                Some(pipe_ref) => pipe_ref,
                None => {
                    close_and_release_interface(interface_intf);
                    return Err(UsbError::Other(format!(
                        "endpoint {endpoint:#04x} has no mapped pipe"
                    )));
                }
            },
            Err(_) => {
                close_and_release_interface(interface_intf);
                return Err(UsbError::Other("macOS pipe cache lock poisoned".into()));
            }
        };

        // SAFETY: interface_intf is open/valid; buf points to readable memory for transfer_size bytes.
        let kr = unsafe {
            (**interface_intf)
                .WritePipe
                .map(|f| {
                    f(
                        interface_intf as _,
                        pipe_ref,
                        buf.as_ptr() as *mut std::ffi::c_void,
                        transfer_size,
                    )
                })
                .unwrap_or(K_IORETURN_UNSUPPORTED)
        };

        let result = if kr == kIOReturnSuccess {
            Ok(transfer_size as usize)
        } else {
            Err(iokit_pipe_kr_to_usb_error(kr))
        };

        close_and_release_interface(interface_intf);
        result
    }

    fn interrupt_read(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        self.bulk_read(endpoint, buf, timeout)
    }

    fn interrupt_write(
        &self,
        endpoint: u8,
        buf: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        self.bulk_write(endpoint, buf, timeout)
    }

    fn get_alternate_setting(&self, interface: u8) -> Result<u8, UsbError> {
        // SAFETY: delegates to raw_control which uses the already-opened device interface.
        // A standard GET_INTERFACE control request (IN | Standard | Interface, bRequest=0x0A)
        // returns the current alternate setting directly from the device without requiring
        // an IOKit interface object — the device interface vtable handles it via DeviceRequestTO.
        self.get_interface_alt_setting(interface, 5000)
    }

    fn set_alternate_setting(&mut self, interface: u8, alt: u8) -> Result<(), UsbError> {
        // Locate the IOUSBInterfaceInterface for this interface number.
        let intf = self.find_interface(interface)?;

        // SAFETY: intf is a valid, open IOUSBInterfaceInterface** returned by find_interface.
        // SetAlternateInterface reprograms the interface to the given alternate setting on the
        // host controller side; we close and release the interface after the call regardless.
        let kr = unsafe {
            (**intf)
                .SetAlternateInterface
                .map(|f| f(intf as _, alt))
                .unwrap_or(K_IORETURN_UNSUPPORTED)
        };
        close_and_release_interface(intf);

        if kr != kIOReturnSuccess {
            return Err(iokit_pipe_kr_to_usb_error(kr));
        }

        // An alternate setting change can reassign pipe indices, so the cached
        // endpoint-to-pipe-ref mapping is no longer valid.
        self.pipe_cache.lock().unwrap().clear();
        Ok(())
    }

    fn get_pipe_info(&self, endpoint: u8) -> Result<EndpointInfo, UsbError> {
        let interface_intf = self.open_interface_for_endpoint(endpoint)?;

        let pipe_ref = match self.pipe_cache.lock() {
            Ok(cache) => match cache.get(&endpoint).copied() {
                Some(r) => r,
                None => {
                    close_and_release_interface(interface_intf);
                    return Err(UsbError::Other(format!(
                        "endpoint {endpoint:#04x} not in pipe cache after interface scan"
                    )));
                }
            },
            Err(_) => {
                close_and_release_interface(interface_intf);
                return Err(UsbError::Other("macOS pipe cache lock poisoned".into()));
            }
        };

        let mut direction: u8 = 0;
        let mut number: u8 = 0;
        let mut transfer_type: u8 = 0;
        let mut max_packet_size: u16 = 0;
        let mut interval: u8 = 0;

        // SAFETY: interface_intf is an open, valid IOUSBInterfaceInterface** returned by
        // open_interface_for_endpoint; all out-pointers are valid stack locations.
        let kr = unsafe {
            (**interface_intf)
                .GetPipeProperties
                .map(|f| {
                    f(
                        interface_intf as _,
                        pipe_ref,
                        &mut direction,
                        &mut number,
                        &mut transfer_type,
                        &mut max_packet_size,
                        &mut interval,
                    )
                })
                .unwrap_or(K_IORETURN_UNSUPPORTED)
        };

        close_and_release_interface(interface_intf);

        if kr != kIOReturnSuccess {
            return Err(iokit_pipe_kr_to_usb_error(kr));
        }

        // Reconstruct the endpoint address byte (direction bit | endpoint number) and
        // the attributes byte (transfer_type in bits 1:0) so we can reuse EndpointInfo::new.
        // IOKit direction: 0 = Out, 1 = In.  IOKit transfer_type: 0=Control 1=Iso 2=Bulk 3=Interrupt
        // — these values are identical to the USB spec bmAttributes bits 1:0.
        let address = (number & 0x0f) | if direction != 0 { 0x80 } else { 0x00 };
        Ok(EndpointInfo::new(address, transfer_type, max_packet_size, interval))
    }

    fn get_pipe_policy(
        &self,
        _endpoint: u8,
        _kind: PipePolicyKind,
    ) -> Result<PipePolicy, UsbError> {
        // macOS: IOKit does not expose pipe policy in user space.
        Err(UsbError::Unsupported)
    }

    fn set_pipe_policy(&self, _endpoint: u8, _policy: PipePolicy) -> Result<(), UsbError> {
        // macOS: IOKit does not expose pipe policy in user space.
        Err(UsbError::Unsupported)
    }
}

// -----------------------------------------------------------------------
// IOKit helpers
// -----------------------------------------------------------------------

fn close_and_release_interface(interface_intf: *mut *mut IOUSBInterfaceInterface) {
    if interface_intf.is_null() {
        return;
    }

    // SAFETY: interface_intf is an interface pointer returned by QueryInterface; calls are COM-style cleanup.
    unsafe {
        let _ = (**interface_intf)
            .USBInterfaceClose
            .map(|f| f(interface_intf as _));
        let _ = (**interface_intf).Release.map(|f| f(interface_intf as _));
    }
}

fn iokit_pipe_kr_to_usb_error(kr: IOReturn) -> UsbError {
    if kr == kIOReturnSuccess {
        return UsbError::Other("unexpected success code in error mapper".into());
    }

    match kr {
        K_IORETURN_NO_DEVICE | K_IORETURN_NOT_OPEN => UsbError::InvalidHandle,
        K_IORETURN_UNSUPPORTED => UsbError::Unsupported,
        other => UsbError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("IOReturn {:#010x}", other as u32),
        )),
    }
}

/// Parse the "iokit:bus=B,addr=A,..." path back into (bus, addr).
fn parse_iokit_path(path: &str) -> Result<(u8, u8), UsbError> {
    let normalized = path.strip_prefix("iokit:").unwrap_or(path);
    let mut bus: Option<u8> = None;
    let mut addr: Option<u8> = None;

    for part in normalized.split(',') {
        let mut it = part.splitn(2, '=');
        let key = it.next().unwrap_or("").trim();
        let val = it.next().unwrap_or("").trim();
        match key {
            "bus" => bus = val.parse::<u8>().ok(),
            "addr" => addr = val.parse::<u8>().ok(),
            _ => {}
        }
    }

    let bus = bus.ok_or_else(|| UsbError::Other(format!("invalid iokit path (missing bus): {path}")))?;
    let addr =
        addr.ok_or_else(|| UsbError::Other(format!("invalid iokit path (missing addr): {path}")))?;

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
        iokit_sys::IOServiceGetMatchingServices(K_IO_MASTER_PORT_DEFAULT, matching_dict, &mut iter)
    };
    if kr != kIOReturnSuccess {
        return None;
    }

    loop {
        let service: io_service_t = unsafe { iokit_sys::IOIteratorNext(iter) };
        if service == 0 {
            break;
        }

        let svc_bus = iokit_integer_property(service, "USBBusNumber").unwrap_or(0) as u8;
        let svc_addr = iokit_integer_property(service, "USB Address").unwrap_or(0) as u8;

        if svc_bus == bus && svc_addr == addr {
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
) -> Result<*mut *mut IOUSBDeviceInterface, UsbError> {
    let mut plugin: *mut *mut IOCFPlugInInterface = std::ptr::null_mut();
    let mut score: i32 = 0;

    // SAFETY: service is valid; plugin and score are valid out-pointers.
    let kr = unsafe {
        IOCreatePlugInInterfaceForService(
            service,
            k_io_usb_device_user_client_type_id(),
            k_io_cf_plugin_interface_id(),
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
    let mut device_intf: *mut *mut IOUSBDeviceInterface = std::ptr::null_mut();
    let kr = unsafe {
        (**plugin).QueryInterface.map(|qi| {
            qi(
                plugin as _,
                K_IO_USB_DEVICE_INTERFACE_ID_BYTES,
                &mut device_intf as *mut _ as *mut _,
            )
        }).unwrap_or(K_IORETURN_UNSUPPORTED)
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
