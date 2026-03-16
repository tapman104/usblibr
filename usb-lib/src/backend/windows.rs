use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, SetupDiGetDeviceRegistryPropertyW, DIGCF_DEVICEINTERFACE,
    DIGCF_PRESENT, SPDRP_HARDWAREID, SPDRP_LOCATION_INFORMATION, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows::Win32::Devices::Usb::{
    WinUsb_AbortPipe, WinUsb_ControlTransfer, WinUsb_Free, WinUsb_GetAssociatedInterface,
    WinUsb_GetCurrentAlternateSetting, WinUsb_GetPipePolicy, WinUsb_Initialize, WinUsb_QueryPipe,
    WinUsb_ReadPipe, WinUsb_ResetPipe, WinUsb_SetCurrentAlternateSetting, WinUsb_SetPipePolicy,
    WinUsb_WritePipe, WINUSB_INTERFACE_HANDLE, WINUSB_PIPE_INFORMATION, WINUSB_PIPE_POLICY,
    WINUSB_SETUP_PACKET,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

use crate::core::{
    BosDescriptor, ConfigDescriptor, ControlSetup, DeviceDescriptor, DeviceInfo, EndpointInfo,
    HubDescriptor, PipePolicy, PipePolicyKind,
};
use crate::error::UsbError;

use super::{UsbBackend, UsbDevice};

// Manual FFI: WinUsb_ResetDevice is missing from windows-rs 0.58.
// winusb.dll is already linked by the windows crate, so no #[link] attribute needed.
extern "system" {
    fn WinUsb_ResetDevice(InterfaceHandle: *mut core::ffi::c_void) -> windows::Win32::Foundation::BOOL;
}

// Manual FFI: Isochronous APIs missing from windows-rs 0.58.
// Opaque handle type returned by WinUsb_RegisterIsochBuffer.
#[cfg(feature = "isochronous")]
type IsochBufferHandle = *mut core::ffi::c_void;

#[cfg(feature = "isochronous")]
extern "system" {
    fn WinUsb_RegisterIsochBuffer(
        InterfaceHandle: *mut core::ffi::c_void,
        PipeID: u8,
        Buffer: *mut core::ffi::c_void,
        BufferLength: u32,
        IsochBufferHandle: *mut IsochBufferHandle,
    ) -> windows::Win32::Foundation::BOOL;

    fn WinUsb_UnregisterIsochBuffer(
        IsochBufferHandle: IsochBufferHandle,
    ) -> windows::Win32::Foundation::BOOL;

    fn WinUsb_ReadIsochPipeAsap(
        BufferHandle: IsochBufferHandle,
        Offset: u32,
        Length: u32,
        ContinueStream: windows::Win32::Foundation::BOOL,
        NumberOfPackets: u32,
        IsoPacketDescriptor: *mut core::ffi::c_void,
        Overlapped: *mut windows::Win32::System::IO::OVERLAPPED,
    ) -> windows::Win32::Foundation::BOOL;

    fn WinUsb_WriteIsochPipeAsap(
        BufferHandle: IsochBufferHandle,
        Offset: u32,
        Length: u32,
        ContinueStream: windows::Win32::Foundation::BOOL,
        Overlapped: *mut windows::Win32::System::IO::OVERLAPPED,
    ) -> windows::Win32::Foundation::BOOL;
}

// WinUSB device interface GUID: {DEE824EF-729B-4A0E-9C14-B7117D33A817}
const WINUSB_DEVICE_INTERFACE_GUID: windows::core::GUID = windows::core::GUID {
    data1: 0xDEE8_24EF,
    data2: 0x729B,
    data3: 0x4A0E,
    data4: [0x9C, 0x14, 0xB7, 0x11, 0x7D, 0x33, 0xA8, 0x17],
};

// WinUSB pipe policy type: PIPE_TRANSFER_TIMEOUT = 0x03
const PIPE_TRANSFER_TIMEOUT: WINUSB_PIPE_POLICY = WINUSB_PIPE_POLICY(0x03);

// Control pipe ID for WinUsb_SetPipePolicy timeout (EP0)
const CONTROL_PIPE_ID: u8 = 0x00;

// -------------------------------------------------------------------
// Public backend entry point
// -------------------------------------------------------------------

pub struct WindowsBackend;

impl UsbBackend for WindowsBackend {
    fn enumerate(&self) -> Result<Vec<DeviceInfo>, UsbError> {
        enumerate_winusb_devices()
    }

    fn open(&self, path: &str) -> Result<Box<dyn UsbDevice>, UsbError> {
        let dev = WinUsbDevice::open(path)?;
        Ok(Box::new(dev))
    }
}

// -------------------------------------------------------------------
// Device enumeration
// -------------------------------------------------------------------

fn enumerate_winusb_devices() -> Result<Vec<DeviceInfo>, UsbError> {
    let mut devices = Vec::new();

    // SAFETY: SetupDiGetClassDevsW is always safe to call with valid arguments.
    let dev_info = unsafe {
        SetupDiGetClassDevsW(
            Some(&WINUSB_DEVICE_INTERFACE_GUID),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .map_err(UsbError::from)?
    };

    // Guard: ensure we destroy the info set even on early return.
    struct DevInfoGuard(windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO);
    impl Drop for DevInfoGuard {
        fn drop(&mut self) {
            // SAFETY: self.0 is a valid HDEVINFO obtained from SetupDiGetClassDevsW.
            unsafe {
                let _ = SetupDiDestroyDeviceInfoList(self.0);
            }
        }
    }
    let _guard = DevInfoGuard(dev_info);

    let mut index: u32 = 0;
    loop {
        let mut iface_data = SP_DEVICE_INTERFACE_DATA {
            cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };

        // SAFETY: dev_info is valid; iface_data is correctly initialised.
        let result = unsafe {
            SetupDiEnumDeviceInterfaces(
                dev_info,
                None,
                &WINUSB_DEVICE_INTERFACE_GUID,
                index,
                &mut iface_data,
            )
        };

        // SetupDiEnumDeviceInterfaces returns Err when index is out of range — end of list.
        if result.is_err() {
            break;
        }

        index += 1;

        // Pass 1: query required buffer size (detail_ptr = None, required_size is set).
        let mut required_size: u32 = 0;
        let mut dev_info_data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };

        // SAFETY: First call with None detail buffer is the documented way to query required size.
        unsafe {
            let _ = SetupDiGetDeviceInterfaceDetailW(
                dev_info,
                &iface_data,
                None,
                0,
                Some(&mut required_size),
                Some(&mut dev_info_data),
            );
        }

        if required_size == 0 {
            continue;
        }

        // Allocate buffer: SP_DEVICE_INTERFACE_DETAIL_DATA_W has a variable-length DevicePath field.
        // The header is 4 bytes (cbSize u32); DevicePath follows as null-terminated UTF-16LE.
        let buf_len = required_size as usize;
        let mut detail_buf: Vec<u8> = vec![0u8; buf_len];

        // SAFETY: We cast to *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W solely to set cbSize.
        // The buffer is at least required_size bytes.
        let detail_ptr = detail_buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        unsafe {
            (*detail_ptr).cbSize =
                std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        }

        // Pass 2: retrieve device path.
        // SAFETY: detail_ptr is valid for required_size bytes; all other args are valid.
        let result = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                dev_info,
                &iface_data,
                Some(detail_ptr),
                required_size,
                None,
                Some(&mut dev_info_data),
            )
        };

        if result.is_err() {
            continue;
        }

        // DevicePath starts at byte offset 4 (after the cbSize u32 field).
        let path_offset = 4usize;
        if buf_len <= path_offset + 1 {
            continue;
        }
        // SAFETY: detail_buf is required_size bytes; we read u16 pairs from offset 4.
        let path_u16: Vec<u16> = detail_buf[path_offset..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        let path = String::from_utf16_lossy(&path_u16).to_string();

        if path.is_empty() {
            continue;
        }

        let (vid, pid) = read_vid_pid(dev_info, &dev_info_data);
        let (bus_number, device_address) =
            read_location_info(dev_info, &dev_info_data).unwrap_or((0, 0));
        let (manufacturer, product, serial_number) = read_string_descriptors(&path);

        devices.push(DeviceInfo {
            vendor_id: vid,
            product_id: pid,
            bus_number,
            device_address,
            path,
            manufacturer,
            product,
            serial_number,
        });
    }

    Ok(devices)
}

// -------------------------------------------------------------------
// Helper: read VID / PID from hardware ID registry property
// -------------------------------------------------------------------

fn read_vid_pid(
    dev_info: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO,
    dev_info_data: &SP_DEVINFO_DATA,
) -> (u16, u16) {
    let mut buf = vec![0u8; 512];
    let mut required: u32 = 0;

    // SAFETY: dev_info and dev_info_data are valid; buf is adequately sized.
    let result = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            dev_info,
            dev_info_data as *const SP_DEVINFO_DATA as *mut SP_DEVINFO_DATA,
            SPDRP_HARDWAREID,
            None,
            Some(&mut buf),
            Some(&mut required),
        )
    };

    if result.is_err() {
        return (0, 0);
    }

    // REG_MULTI_SZ: UTF-16LE strings separated by null chars, list ends with double null.
    let words: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();

    let mut pos = 0;
    while pos < words.len() {
        let end = words[pos..]
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(words.len() - pos);
        let hw_id = String::from_utf16_lossy(&words[pos..pos + end]).to_uppercase();

        if let Some((vid, pid)) = parse_vid_pid(&hw_id) {
            return (vid, pid);
        }

        pos += end + 1;
        if pos >= words.len() || words[pos] == 0 {
            break;
        }
    }

    (0, 0)
}

/// Parse "USB\VID_045E&PID_07A5..." into (0x045E, 0x07A5).
fn parse_vid_pid(hw_id: &str) -> Option<(u16, u16)> {
    let vid_pos = hw_id.find("VID_")?;
    let pid_pos = hw_id.find("PID_")?;

    let vid_str = hw_id.get(vid_pos + 4..vid_pos + 8)?;
    let pid_str = hw_id.get(pid_pos + 4..pid_pos + 8)?;

    let vid = u16::from_str_radix(vid_str, 16).ok()?;
    let pid = u16::from_str_radix(pid_str, 16).ok()?;

    Some((vid, pid))
}

// -------------------------------------------------------------------
// Helper: parse bus/address from SPDRP_LOCATION_INFORMATION
// "Port_#0001.Hub_#0003" → (hub=3, port=1) used as (bus, address)
// -------------------------------------------------------------------

fn read_location_info(
    dev_info: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO,
    dev_info_data: &SP_DEVINFO_DATA,
) -> Option<(u8, u8)> {
    let mut buf = vec![0u8; 512];
    let mut required: u32 = 0;

    // SAFETY: dev_info and dev_info_data are valid; buf is adequately sized.
    let result = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            dev_info,
            dev_info_data as *const SP_DEVINFO_DATA as *mut SP_DEVINFO_DATA,
            SPDRP_LOCATION_INFORMATION,
            None,
            Some(&mut buf),
            Some(&mut required),
        )
    };

    if result.is_err() {
        return None;
    }

    let words: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();

    let loc = String::from_utf16_lossy(&words).to_uppercase();

    // Pattern: "Port_#NNNN.Hub_#NNNN"
    let port = loc
        .find("PORT_#")
        .and_then(|p| loc.get(p + 6..p + 10))
        .and_then(|s| s.trim_start_matches('0').parse::<u8>().ok())
        .unwrap_or(0);

    let hub = loc
        .find("HUB_#")
        .and_then(|p| loc.get(p + 5..p + 9))
        .and_then(|s| s.trim_start_matches('0').parse::<u8>().ok())
        .unwrap_or(0);

    Some((hub, port))
}

// -------------------------------------------------------------------
// Helper: open device briefly to read string descriptors
// -------------------------------------------------------------------

fn read_string_descriptors(path: &str) -> (Option<String>, Option<String>, Option<String>) {
    let dev = match WinUsbDevice::open(path) {
        Ok(d) => d,
        Err(_) => return (None, None, None),
    };

    let dd = match dev.read_device_descriptor() {
        Ok(d) => d,
        Err(_) => return (None, None, None),
    };

    let lang_id: u16 = 0x0409; // English (US)

    let manufacturer = if dd.manufacturer_index != 0 {
        dev.read_string_descriptor(dd.manufacturer_index, lang_id).ok()
    } else {
        None
    };

    let product = if dd.product_index != 0 {
        dev.read_string_descriptor(dd.product_index, lang_id).ok()
    } else {
        None
    };

    let serial_number = if dd.serial_number_index != 0 {
        dev.read_string_descriptor(dd.serial_number_index, lang_id).ok()
    } else {
        None
    };

    (manufacturer, product, serial_number)
}

// -------------------------------------------------------------------
// WinUsbDevice — wraps a file handle and a WinUSB interface handle
// -------------------------------------------------------------------

struct WinUsbDevice {
    file_handle: HANDLE,
    usb_handle: WINUSB_INTERFACE_HANDLE,
    /// Handles for claimed interfaces N > 0: (interface_number, handle).
    assoc_handles: Vec<(u8, WINUSB_INTERFACE_HANDLE)>,
    /// Endpoint-address → WinUSB handle cache, populated on claim_interface.
    endpoint_cache: std::collections::HashMap<u8, WINUSB_INTERFACE_HANDLE>,
}

impl WinUsbDevice {
    /// Open a WinUSB device by its device path.
    ///
    /// First attempts `GENERIC_READ | GENERIC_WRITE`. On `ERROR_ACCESS_DENIED`
    /// retries with `GENERIC_READ` only — descriptor reading still works;
    /// host-to-device control transfers will fail at the OS level.
    fn open(path: &str) -> Result<Self, UsbError> {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let wide_ptr = PCWSTR(wide.as_ptr());

        // Attempt 1: read + write.
        // SAFETY: wide_ptr points to a valid null-terminated UTF-16 string for this call's duration.
        let fh_result = unsafe {
            CreateFileW(
                wide_ptr,
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        };

        let file_handle: HANDLE = match fh_result {
            Ok(h) if h != INVALID_HANDLE_VALUE => h,
            Err(ref e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                log::warn!("WinUSB: GENERIC_WRITE denied on {path}; retrying read-only");
                // Attempt 2: read-only fallback.
                // SAFETY: Same safety invariant as attempt 1.
                let h = unsafe {
                    CreateFileW(
                        wide_ptr,
                        GENERIC_READ.0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        None,
                        OPEN_EXISTING,
                        FILE_FLAG_OVERLAPPED,
                        None,
                    )
                    .map_err(UsbError::from)?
                };
                if h == INVALID_HANDLE_VALUE {
                    return Err(UsbError::PermissionDenied);
                }
                h
            }
            Err(e) => return Err(UsbError::from(e)),
            Ok(_) => return Err(UsbError::DeviceNotFound),
        };

        let mut usb_handle = WINUSB_INTERFACE_HANDLE::default();
        // SAFETY: file_handle is a valid open file handle from CreateFileW.
        unsafe {
            WinUsb_Initialize(file_handle, &mut usb_handle).map_err(|e| {
                let _ = CloseHandle(file_handle);
                UsbError::from(e)
            })?;
        }

        Ok(Self {
            file_handle,
            usb_handle,
            assoc_handles: Vec::new(),
            endpoint_cache: std::collections::HashMap::new(),
        })
    }

    /// Return the WinUSB interface handle for the given interface number.
    /// Returns `Err` if the interface has not been claimed.
    fn interface_handle(&self, interface: u8) -> Result<WINUSB_INTERFACE_HANDLE, UsbError> {
        if interface == 0 {
            return Ok(self.usb_handle);
        }
        self.assoc_handles
            .iter()
            .find(|(n, _)| *n == interface)
            .map(|(_, h)| *h)
            .ok_or_else(|| {
                UsbError::Other(format!(
                    "interface {interface} not claimed; call claim_interface first"
                ))
            })
    }

    /// Find which held interface handle owns `endpoint`.
    ///
    /// Checks the endpoint cache (populated by `claim_interface`) first; falls
    /// back to scanning all held handles via `WinUsb_QueryPipe` for single-
    /// interface devices where the primary handle was never explicitly claimed.
    fn handle_for_endpoint(&self, endpoint: u8) -> WINUSB_INTERFACE_HANDLE {
        // Fast path: cache lookup.
        if let Some(&h) = self.endpoint_cache.get(&endpoint) {
            return h;
        }
        // Slow path: probe all held handles (single-interface or un-cached).
        if self.assoc_handles.is_empty() {
            return self.usb_handle;
        }
        let candidates = std::iter::once(&self.usb_handle)
            .chain(self.assoc_handles.iter().map(|(_, h)| h));
        for &h in candidates {
            let mut info = WINUSB_PIPE_INFORMATION::default();
            for idx in 0u8..32 {
                // SAFETY: h is a valid WinUSB interface handle.
                if unsafe { WinUsb_QueryPipe(h, 0, idx, &mut info).is_err() } {
                    break;
                }
                if info.PipeId == endpoint {
                    return h;
                }
            }
        }
        self.usb_handle // fallback — correct for single-interface devices
    }

    /// Populate the endpoint cache for a given interface handle.
    fn build_endpoint_cache(&mut self, h: WINUSB_INTERFACE_HANDLE) {
        let mut info = WINUSB_PIPE_INFORMATION::default();
        for idx in 0u8..32 {
            // SAFETY: h is a valid WinUSB interface handle.
            if unsafe { WinUsb_QueryPipe(h, 0, idx, &mut info).is_err() } {
                break;
            }
            self.endpoint_cache.insert(info.PipeId, h);
        }
    }

    /// Apply `PIPE_TRANSFER_TIMEOUT` on `endpoint` using the given interface handle.
    fn set_pipe_timeout(handle: WINUSB_INTERFACE_HANDLE, endpoint: u8, timeout_ms: u32) {
        if timeout_ms > 0 {
            // SAFETY: handle is a valid WinUSB interface handle; value is 4-byte LE u32.
            unsafe {
                let val = timeout_ms.to_le_bytes();
                let _ = WinUsb_SetPipePolicy(
                    handle,
                    endpoint,
                    PIPE_TRANSFER_TIMEOUT,
                    4,
                    val.as_ptr() as *const _,
                );
            }
        }
    }

    /// Issue a synchronous control transfer on EP0.
    fn do_control_transfer(
        &self,
        setup: &ControlSetup,
        data: Option<&mut [u8]>,
        timeout_ms: u32,
    ) -> Result<usize, UsbError> {
        // Apply timeout policy on EP0 before transfer.
        // SAFETY: usb_handle is a valid WinUSB handle; value is a 4-byte LE u32.
        if timeout_ms > 0 {
            unsafe {
                let timeout_val = timeout_ms.to_le_bytes();
                let _ = WinUsb_SetPipePolicy(
                    self.usb_handle,
                    CONTROL_PIPE_ID,
                    PIPE_TRANSFER_TIMEOUT,
                    4,
                    timeout_val.as_ptr() as *const _,
                );
            }
        }

        let pkt = WINUSB_SETUP_PACKET {
            RequestType: setup.request_type,
            Request: setup.request,
            Value: setup.value,
            Index: setup.index,
            Length: setup.length,
        };

        let mut transferred: u32 = 0;

        match data {
            Some(buf) => {
                // SAFETY: buf is a valid mutable slice for the transfer; usb_handle is valid.
                // None for lpOverlapped → synchronous I/O.
                unsafe {
                    WinUsb_ControlTransfer(
                        self.usb_handle,
                        pkt,
                        Some(buf),
                        Some(&mut transferred),
                        None,
                    )
                    .map_err(UsbError::from)?;
                }
            }
            None => {
                // Zero-data phase (e.g. SET_CONFIGURATION where wLength = 0).
                // SAFETY: empty slice is valid for a zero-length transfer.
                unsafe {
                    WinUsb_ControlTransfer(
                        self.usb_handle,
                        pkt,
                        Some(&mut []),
                        Some(&mut transferred),
                        None,
                    )
                    .map_err(UsbError::from)?;
                }
            }
        }

        Ok(transferred as usize)
    }
}

impl Drop for WinUsbDevice {
    fn drop(&mut self) {
        // Release associated interface handles in reverse order before the primary handle.
        for (_, h) in self.assoc_handles.drain(..).rev() {
            // SAFETY: h is a valid WINUSB_INTERFACE_HANDLE from WinUsb_GetAssociatedInterface.
            unsafe {
                let _ = WinUsb_Free(h);
            }
        }
        // SAFETY: usb_handle was obtained from WinUsb_Initialize and has not been freed yet.
        unsafe {
            let _ = WinUsb_Free(self.usb_handle);
        }
        // SAFETY: file_handle was obtained from CreateFileW and has not been closed yet.
        unsafe {
            let _ = CloseHandle(self.file_handle);
        }
    }
}

// SAFETY: WinUsbDevice exclusively owns its handles and uses no thread-local state.
unsafe impl Send for WinUsbDevice {}

// -------------------------------------------------------------------
// UsbDevice implementation for WinUsbDevice
// -------------------------------------------------------------------

impl UsbDevice for WinUsbDevice {
    fn read_device_descriptor(&self) -> Result<DeviceDescriptor, UsbError> {
        let setup = ControlSetup::get_descriptor(0x01, 0, 0x0000, 18);
        let mut buf = [0u8; 18];
        self.do_control_transfer(&setup, Some(&mut buf), 1000)?;
        DeviceDescriptor::from_bytes(&buf)
    }

    fn read_config_descriptor(&self, index: u8) -> Result<ConfigDescriptor, UsbError> {
        // Pass 1: read 9-byte header to get wTotalLength.
        let setup = ControlSetup::get_descriptor(0x02, index, 0x0000, 9);
        let mut hdr = [0u8; 9];
        self.do_control_transfer(&setup, Some(&mut hdr), 1000)?;

        let total_len = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
        if total_len < 9 {
            return Err(UsbError::InvalidDescriptor);
        }

        // Pass 2: read full configuration descriptor blob.
        let setup2 = ControlSetup::get_descriptor(0x02, index, 0x0000, total_len as u16);
        let mut buf = vec![0u8; total_len];
        self.do_control_transfer(&setup2, Some(&mut buf), 1000)?;

        ConfigDescriptor::from_bytes(&buf)
    }

    fn read_string_descriptor(&self, index: u8, lang: u16) -> Result<String, UsbError> {
        let setup = ControlSetup::get_descriptor(0x03, index, lang, 255);
        let mut buf = [0u8; 255];
        let transferred = self.do_control_transfer(&setup, Some(&mut buf), 1000)?;

        if transferred < 2 {
            return Err(UsbError::InvalidDescriptor);
        }

        let b_length = buf[0] as usize;
        if b_length < 2 || b_length > transferred {
            return Err(UsbError::InvalidDescriptor);
        }

        // String data starts at byte 2, UTF-16LE encoded.
        let s = String::from_utf16_lossy(
            &buf[2..b_length]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<u16>>(),
        )
        .to_owned();

        Ok(s)
    }

    fn claim_interface(&mut self, interface: u8) -> Result<(), UsbError> {
        // Interface 0 is implicitly held by usb_handle from WinUsb_Initialize.
        if interface == 0 {
            // Still populate the cache for interface 0.
            let primary = self.usb_handle;
            self.build_endpoint_cache(primary);
            return Ok(());
        }

        if self.assoc_handles.iter().any(|(n, _)| *n == interface) {
            return Ok(()); // already claimed
        }

        // WinUSB associated-interface index is 0-based from interface 1.
        let assoc_index = interface - 1;
        let mut assoc_handle = WINUSB_INTERFACE_HANDLE::default();

        // SAFETY: usb_handle is valid; assoc_index is within the device's interface count.
        unsafe {
            WinUsb_GetAssociatedInterface(self.usb_handle, assoc_index, &mut assoc_handle)
                .map_err(UsbError::from)?;
        }

        self.build_endpoint_cache(assoc_handle);
        self.assoc_handles.push((interface, assoc_handle));
        Ok(())
    }

    fn release_interface(&mut self, interface: u8) -> Result<(), UsbError> {
        if interface == 0 {
            return Ok(()); // released on device close
        }

        if let Some(pos) = self
            .assoc_handles
            .iter()
            .position(|(n, _)| *n == interface)
        {
            let (_, h) = self.assoc_handles.remove(pos);
            // Evict any cache entries that pointed at this handle.
            self.endpoint_cache.retain(|_, cached_h| *cached_h != h);
            // SAFETY: h is a valid WINUSB_INTERFACE_HANDLE obtained from WinUsb_GetAssociatedInterface.
            // WinUsb_Free returns BOOL; a FALSE return means the handle was already invalid,
            // which is not a correctable error at this point so we ignore it.
            unsafe {
                let _ = WinUsb_Free(h);
            }
        }

        Ok(())
    }

    fn control_transfer(
        &self,
        setup: ControlSetup,
        data: Option<&mut [u8]>,
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        self.do_control_transfer(&setup, data, timeout_ms)
    }

    fn bulk_read(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        Self::set_pipe_timeout(h, endpoint, timeout_ms);
        let mut transferred = 0u32;
        // SAFETY: h is a valid WinUSB handle; buf is writable for its full length.
        unsafe {
            WinUsb_ReadPipe(h, endpoint, Some(buf), Some(&mut transferred), None)
                .map_err(UsbError::from)?;
        }
        Ok(transferred as usize)
    }

    fn bulk_write(
        &self,
        endpoint: u8,
        buf: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        Self::set_pipe_timeout(h, endpoint, timeout_ms);
        let mut transferred = 0u32;
        // SAFETY: h is a valid WinUSB handle; buf is readable for its full length.
        unsafe {
            WinUsb_WritePipe(h, endpoint, buf, Some(&mut transferred), None)
                .map_err(UsbError::from)?;
        }
        Ok(transferred as usize)
    }

    fn interrupt_read(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        // WinUSB uses the same underlying pipe I/O for bulk and interrupt endpoints.
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

    fn reset_pipe(&self, endpoint: u8) -> Result<(), UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        // SAFETY: h is a valid WinUSB handle; endpoint is the pipe ID.
        unsafe { WinUsb_ResetPipe(h, endpoint).map_err(UsbError::from) }
    }

    fn abort_pipe(&self, endpoint: u8) -> Result<(), UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        // SAFETY: h is a valid WinUSB handle.
        unsafe { WinUsb_AbortPipe(h, endpoint).map_err(UsbError::from) }
    }

    fn reset_device(&self) -> Result<(), UsbError> {
        // SAFETY: usb_handle.0 is the opaque WinUSB interface handle pointer.
        // WinUsb_ResetDevice is declared in our extern "system" block above.
        let ok = unsafe { WinUsb_ResetDevice(self.usb_handle.0) };
        if ok.as_bool() {
            Ok(())
        } else {
            Err(UsbError::from(windows::core::Error::from_win32()))
        }
    }

    fn get_alternate_setting(&self, interface: u8) -> Result<u8, UsbError> {
        let h = self.interface_handle(interface)?;
        let mut alt = 0u8;
        // SAFETY: h is a valid WinUSB interface handle; alt is a valid out-pointer.
        unsafe {
            WinUsb_GetCurrentAlternateSetting(h, &mut alt).map_err(UsbError::from)?;
        }
        Ok(alt)
    }

    fn set_alternate_setting(&mut self, interface: u8, alt: u8) -> Result<(), UsbError> {
        let h = self.interface_handle(interface)?;
        // SAFETY: h is a valid WinUSB interface handle.
        unsafe { WinUsb_SetCurrentAlternateSetting(h, alt).map_err(UsbError::from) }
    }

    fn get_pipe_info(&self, endpoint: u8) -> Result<EndpointInfo, UsbError> {
        // Search all held interface handles (interface 0 then associated).
        let handles: Vec<WINUSB_INTERFACE_HANDLE> = std::iter::once(self.usb_handle)
            .chain(self.assoc_handles.iter().map(|(_, h)| *h))
            .collect();

        for h in handles {
            let mut info = WINUSB_PIPE_INFORMATION::default();
            for idx in 0u8..32u8 {
                // SAFETY: h is a valid WinUSB interface handle; info is a valid out-pointer.
                // WinUsb_QueryPipe returns Err when idx exceeds the pipe count.
                if unsafe { WinUsb_QueryPipe(h, 0, idx, &mut info).is_err() } {
                    break;
                }
                if info.PipeId == endpoint {
                    // USBD_PIPE_TYPE values 0–3 match USB spec bmAttributes bits 1:0 exactly:
                    //   0 = Control, 1 = Isochronous, 2 = Bulk, 3 = Interrupt
                    return Ok(EndpointInfo::new(
                        info.PipeId,
                        info.PipeType.0 as u8,
                        info.MaximumPacketSize,
                        info.Interval,
                    ));
                }
            }
        }

        Err(UsbError::Other(format!(
            "endpoint {endpoint:#04x} not found on any claimed interface"
        )))
    }

    fn get_pipe_policy(
        &self,
        endpoint: u8,
        kind: PipePolicyKind,
    ) -> Result<PipePolicy, UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        let (policy_id, is_u32) = winusb_policy_params(kind);

        if is_u32 {
            let mut val = 0u32;
            let mut len = 4u32;
            // SAFETY: h is valid; val is a 4-byte region described by len.
            unsafe {
                WinUsb_GetPipePolicy(
                    h,
                    endpoint,
                    policy_id,
                    &mut len,
                    (&mut val as *mut u32).cast(),
                )
                .map_err(UsbError::from)?;
            }
            Ok(PipePolicy::TransferTimeout(val))
        } else {
            let mut val = 0u8;
            let mut len = 1u32;
            // SAFETY: h is valid; val is a 1-byte region described by len.
            unsafe {
                WinUsb_GetPipePolicy(
                    h,
                    endpoint,
                    policy_id,
                    &mut len,
                    (&mut val as *mut u8).cast(),
                )
                .map_err(UsbError::from)?;
            }
            let b = val != 0;
            let policy = match kind {
                PipePolicyKind::ShortPacketTerminate => PipePolicy::ShortPacketTerminate(b),
                PipePolicyKind::AutoClearStall       => PipePolicy::AutoClearStall(b),
                PipePolicyKind::AllowPartialReads    => PipePolicy::AllowPartialReads(b),
                PipePolicyKind::AutoFlush            => PipePolicy::AutoFlush(b),
                PipePolicyKind::RawIo                => PipePolicy::RawIo(b),
                PipePolicyKind::ResetPipeOnResume    => PipePolicy::ResetPipeOnResume(b),
                PipePolicyKind::TransferTimeout      => unreachable!("handled as u32 above"),
            };
            Ok(policy)
        }
    }

    fn set_pipe_policy(&self, endpoint: u8, policy: PipePolicy) -> Result<(), UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        let (policy_id, _) = winusb_policy_params(policy.kind());

        match policy {
            PipePolicy::TransferTimeout(ms) => {
                let val = ms.to_le_bytes();
                // SAFETY: h valid; val is a 4-byte LE u32.
                unsafe {
                    WinUsb_SetPipePolicy(h, endpoint, policy_id, 4, val.as_ptr().cast())
                        .map_err(UsbError::from)
                }
            }
            _ => {
                let byte_val = policy.as_bool().unwrap_or(false) as u8;
                // SAFETY: h valid; byte_val is a 1-byte UCHAR.
                unsafe {
                    WinUsb_SetPipePolicy(
                        h,
                        endpoint,
                        policy_id,
                        1,
                        (&byte_val as *const u8).cast(),
                    )
                    .map_err(UsbError::from)
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Advanced descriptors
    // -----------------------------------------------------------------------

    fn read_bos_descriptor(&self) -> Result<BosDescriptor, UsbError> {
        // First pass: read just the 5-byte BOS header to get wTotalLength.
        let setup = ControlSetup::get_descriptor(0x0F, 0, 0, 5);
        let mut hdr = [0u8; 5];
        let n = self.do_control_transfer(&setup, Some(&mut hdr), 1000)?;
        if n < 5 || hdr[1] != 0x0F {
            return Err(UsbError::InvalidDescriptor);
        }
        let total_len = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;

        // Second pass: read the full BOS descriptor.
        let setup2 = ControlSetup::get_descriptor(0x0F, 0, 0, total_len.min(4096) as u16);
        let mut buf = vec![0u8; total_len.min(4096)];
        let n2 = self.do_control_transfer(&setup2, Some(&mut buf), 1000)?;
        BosDescriptor::from_bytes(&buf[..n2])
    }

    fn read_hub_descriptor(&self) -> Result<HubDescriptor, UsbError> {
        // Hub class GET_DESCRIPTOR: bmRequestType = 0xA0, bRequest = 0x06,
        // wValue = 0x2900 (type 0x29, index 0), wIndex = 0.
        let setup = ControlSetup {
            request_type: 0xA0,
            request: 0x06,
            value: 0x2900,
            index: 0,
            length: 71, // max hub descriptor length (USB 2 spec: 7 + 2*ceil(ports/8))
        };
        let mut buf = [0u8; 71];
        let n = self.do_control_transfer(&setup, Some(&mut buf), 1000)?;
        HubDescriptor::from_bytes(&buf[..n])
    }

    // -----------------------------------------------------------------------
    // Overlapped (OS-async) transfers
    // -----------------------------------------------------------------------

    fn async_bulk_read(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        overlapped_read(self.file_handle, h, endpoint, buf, timeout)
    }

    fn async_bulk_write(
        &self,
        endpoint: u8,
        buf: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        overlapped_write(self.file_handle, h, endpoint, buf, timeout)
    }

    fn async_interrupt_read(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        overlapped_read(self.file_handle, h, endpoint, buf, timeout)
    }

    fn async_interrupt_write(
        &self,
        endpoint: u8,
        buf: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        overlapped_write(self.file_handle, h, endpoint, buf, timeout)
    }

    // -----------------------------------------------------------------------
    // Isochronous transfers — only compiled with feature = "isochronous"
    // -----------------------------------------------------------------------

    #[cfg(feature = "isochronous")]
    fn isoch_read(&self, endpoint: u8, buf: &mut [u8]) -> Result<usize, UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        isoch_transfer_read(self.file_handle, h, endpoint, buf)
    }

    #[cfg(feature = "isochronous")]
    fn isoch_write(&self, endpoint: u8, buf: &[u8]) -> Result<usize, UsbError> {
        let h = self.handle_for_endpoint(endpoint);
        isoch_transfer_write(self.file_handle, h, endpoint, buf)
    }
}

// -----------------------------------------------------------------------
// Overlapped I/O helpers
// -----------------------------------------------------------------------

/// Issue a WinUSB pipe read using an OVERLAPPED structure and wait for
/// completion.  Returns the number of bytes transferred.
fn overlapped_read(
    file_handle: HANDLE,
    usb_handle: WINUSB_INTERFACE_HANDLE,
    endpoint: u8,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<usize, UsbError> {
    // SAFETY: CreateEventW with default security, manual-reset=FALSE, initial-state=FALSE.
    let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map_err(UsbError::from)?;

    // Box the OVERLAPPED to give it a stable address during the I/O operation.
    let mut ov = Box::new(OVERLAPPED::default());
    ov.hEvent = event;

    let mut transferred = 0u32;
    // SAFETY: usb_handle and buf are valid for the duration of this function.
    let io_result = unsafe {
        WinUsb_ReadPipe(usb_handle, endpoint, Some(buf), Some(&mut transferred), Some(&*ov))
    };

    let result = match io_result {
        Ok(()) => {
            // Completed synchronously.
            Ok(transferred as usize)
        }
        Err(ref e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
            // I/O pending — wait.
            let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
            let wait_ms = if timeout_ms == 0 { INFINITE } else { timeout_ms };
            // SAFETY: event is a valid event handle; wait_ms is a valid timeout.
            let wait_result = unsafe { WaitForSingleObject(event, wait_ms) };
            if wait_result == WAIT_OBJECT_0 {
                // SAFETY: ov is pinned via Box; file_handle is valid.
                unsafe {
                    GetOverlappedResult(file_handle, &*ov, &mut transferred, false)
                        .map_err(UsbError::from)?;
                }
                Ok(transferred as usize)
            } else {
                Err(UsbError::Timeout)
            }
        }
        Err(e) => Err(UsbError::from(e)),
    };

    // SAFETY: event was created by CreateEventW and must be closed.
    unsafe { let _ = CloseHandle(event); }
    result
}

/// Issue a WinUSB pipe write using an OVERLAPPED structure and wait for
/// completion.  Returns the number of bytes transferred.
fn overlapped_write(
    file_handle: HANDLE,
    usb_handle: WINUSB_INTERFACE_HANDLE,
    endpoint: u8,
    buf: &[u8],
    timeout: Duration,
) -> Result<usize, UsbError> {
    // SAFETY: CreateEventW with default security, manual-reset=FALSE, initial-state=FALSE.
    let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map_err(UsbError::from)?;

    let mut ov = Box::new(OVERLAPPED::default());
    ov.hEvent = event;

    let mut transferred = 0u32;
    // SAFETY: usb_handle and buf are valid for the duration of this function.
    let io_result = unsafe {
        WinUsb_WritePipe(usb_handle, endpoint, buf, Some(&mut transferred), Some(&*ov))
    };

    let result = match io_result {
        Ok(()) => Ok(transferred as usize),
        Err(ref e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
            let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
            let wait_ms = if timeout_ms == 0 { INFINITE } else { timeout_ms };
            // SAFETY: event is a valid event handle.
            let wait_result = unsafe { WaitForSingleObject(event, wait_ms) };
            if wait_result == WAIT_OBJECT_0 {
                // SAFETY: ov is pinned via Box; file_handle is valid.
                unsafe {
                    GetOverlappedResult(file_handle, &*ov, &mut transferred, false)
                        .map_err(UsbError::from)?;
                }
                Ok(transferred as usize)
            } else {
                Err(UsbError::Timeout)
            }
        }
        Err(e) => Err(UsbError::from(e)),
    };

    // SAFETY: event was created by CreateEventW and must be closed.
    unsafe { let _ = CloseHandle(event); }
    result
}


// -----------------------------------------------------------------------
// Pipe policy helpers
// -----------------------------------------------------------------------

/// Map a [`PipePolicyKind`] to its WinUSB policy constant and whether the
/// value is a 4-byte ULONG (`true`) or a 1-byte UCHAR (`false`).
fn winusb_policy_params(kind: PipePolicyKind) -> (WINUSB_PIPE_POLICY, bool) {
    match kind {
        PipePolicyKind::ShortPacketTerminate => (WINUSB_PIPE_POLICY(0x01), false),
        PipePolicyKind::AutoClearStall       => (WINUSB_PIPE_POLICY(0x02), false),
        PipePolicyKind::TransferTimeout      => (WINUSB_PIPE_POLICY(0x03), true),
        PipePolicyKind::AllowPartialReads    => (WINUSB_PIPE_POLICY(0x05), false),
        PipePolicyKind::AutoFlush            => (WINUSB_PIPE_POLICY(0x06), false),
        PipePolicyKind::RawIo                => (WINUSB_PIPE_POLICY(0x07), false),
        PipePolicyKind::ResetPipeOnResume    => (WINUSB_PIPE_POLICY(0x09), false),
    }
}

// -----------------------------------------------------------------------
// Isochronous transfer helpers (feature = "isochronous")
// -----------------------------------------------------------------------

/// Register `buf` with WinUSB for the given isochronous IN endpoint, issue a
/// non-blocking read via `WinUsb_ReadIsochPipeAsap`, wait for OVERLAPPED
/// completion, then unregister the buffer.  Returns bytes transferred.
#[cfg(feature = "isochronous")]
fn isoch_transfer_read(
    file_handle: HANDLE,
    usb_handle: WINUSB_INTERFACE_HANDLE,
    endpoint: u8,
    buf: &mut [u8],
) -> Result<usize, UsbError> {
    use windows::Win32::Foundation::BOOL;

    let buf_len = buf.len();
    if buf_len == 0 {
        return Ok(0);
    }

    let mut isoch_handle: IsochBufferHandle = core::ptr::null_mut();

    // SAFETY: usb_handle is valid; buf lives for the entire function.
    let ok = unsafe {
        WinUsb_RegisterIsochBuffer(
            usb_handle.0,
            endpoint,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf_len as u32,
            &mut isoch_handle,
        )
    };
    if !ok.as_bool() {
        return Err(UsbError::from(windows::core::Error::from_win32()));
    }

    // Create event for OVERLAPPED.
    // SAFETY: CreateEventW with valid arguments.
    let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map_err(UsbError::from)?;

    let mut ov = Box::new(OVERLAPPED::default());
    ov.hEvent = event;

    // SAFETY: isoch_handle and ov are valid; 0 packets (driver-determined).
    let io_ok = unsafe {
        WinUsb_ReadIsochPipeAsap(
            isoch_handle,
            0,
            buf_len as u32,
            BOOL(0), // ContinueStream = FALSE (start new stream)
            0,       // NumberOfPackets = 0 (driver determined)
            core::ptr::null_mut(),
            &mut *ov,
        )
    };

    let mut transferred = 0u32;
    let result = if io_ok.as_bool() {
        // Completed synchronously: query final byte count.
        // SAFETY: ov is boxed (stable addr); file_handle is valid.
        unsafe {
            GetOverlappedResult(file_handle, &*ov, &mut transferred, false)
                .map_err(UsbError::from)?;
        }
        Ok(transferred as usize)
    } else {
        let err = windows::core::Error::from_win32();
        if err.code() == ERROR_IO_PENDING.to_hresult() {
            // SAFETY: event is valid.
            let wait = unsafe { WaitForSingleObject(event, INFINITE) };
            if wait == WAIT_OBJECT_0 {
                // SAFETY: ov is boxed (stable addr); file_handle is valid.
                unsafe {
                    GetOverlappedResult(file_handle, &*ov, &mut transferred, false)
                        .map_err(UsbError::from)?;
                }
                Ok(transferred as usize)
            } else {
                Err(UsbError::Timeout)
            }
        } else {
            Err(UsbError::from(err))
        }
    };

    // Always unregister and close, even on error.
    // SAFETY: isoch_handle is valid (was successfully registered above).
    unsafe { let _ = WinUsb_UnregisterIsochBuffer(isoch_handle); }
    // SAFETY: event was created by CreateEventW.
    unsafe { let _ = CloseHandle(event); }

    result
}

/// Register `buf` with WinUSB for the given isochronous OUT endpoint, issue a
/// non-blocking write via `WinUsb_WriteIsochPipeAsap`, wait for OVERLAPPED
/// completion, then unregister the buffer.  Returns bytes transferred.
#[cfg(feature = "isochronous")]
fn isoch_transfer_write(
    file_handle: HANDLE,
    usb_handle: WINUSB_INTERFACE_HANDLE,
    endpoint: u8,
    buf: &[u8],
) -> Result<usize, UsbError> {
    use windows::Win32::Foundation::BOOL;

    let buf_len = buf.len();
    if buf_len == 0 {
        return Ok(0);
    }

    // RegisterIsochBuffer takes a *mut void but we own the slice for the
    // duration of this function, so the cast is sound.
    let mut isoch_handle: IsochBufferHandle = core::ptr::null_mut();

    // SAFETY: usb_handle is valid; buf lives for the entire function.
    let ok = unsafe {
        WinUsb_RegisterIsochBuffer(
            usb_handle.0,
            endpoint,
            buf.as_ptr() as *mut core::ffi::c_void,
            buf_len as u32,
            &mut isoch_handle,
        )
    };
    if !ok.as_bool() {
        return Err(UsbError::from(windows::core::Error::from_win32()));
    }

    // SAFETY: CreateEventW with valid arguments.
    let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
        .map_err(UsbError::from)?;

    let mut ov = Box::new(OVERLAPPED::default());
    ov.hEvent = event;

    // SAFETY: isoch_handle and ov are valid.
    let io_ok = unsafe {
        WinUsb_WriteIsochPipeAsap(
            isoch_handle,
            0,
            buf_len as u32,
            BOOL(0), // ContinueStream = FALSE
            &mut *ov,
        )
    };

    let mut transferred = 0u32;
    let result = if io_ok.as_bool() {
        // SAFETY: ov is boxed; file_handle is valid.
        unsafe {
            GetOverlappedResult(file_handle, &*ov, &mut transferred, false)
                .map_err(UsbError::from)?;
        }
        Ok(transferred as usize)
    } else {
        let err = windows::core::Error::from_win32();
        if err.code() == ERROR_IO_PENDING.to_hresult() {
            // SAFETY: event is valid.
            let wait = unsafe { WaitForSingleObject(event, INFINITE) };
            if wait == WAIT_OBJECT_0 {
                // SAFETY: ov is boxed; file_handle is valid.
                unsafe {
                    GetOverlappedResult(file_handle, &*ov, &mut transferred, false)
                        .map_err(UsbError::from)?;
                }
                Ok(transferred as usize)
            } else {
                Err(UsbError::Timeout)
            }
        } else {
            Err(UsbError::from(err))
        }
    };

    // Always clean up.
    // SAFETY: isoch_handle is valid.
    unsafe { let _ = WinUsb_UnregisterIsochBuffer(isoch_handle); }
    // SAFETY: event was created by CreateEventW.
    unsafe { let _ = CloseHandle(event); }

    result
}
