use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::time::Duration;

use nix::libc;
use nix::request_code_readwrite;

use crate::core::{ConfigDescriptor, ControlSetup, DeviceDescriptor, DeviceInfo};
use crate::error::UsbError;

use super::{UsbBackend, UsbDevice};

// -----------------------------------------------------------------------
// Linux kernel USBDEVFS ioctl constants (from linux/usbdevice_fs.h)
// -----------------------------------------------------------------------

const USBDEVFS_CONTROL_IOCTL: u8 = b'U';
const USBDEVFS_CONTROL_NR: u8 = 0;
const USBDEVFS_CLAIMINTERFACE_NR: u8 = 15;
const USBDEVFS_RELEASEINTERFACE_NR: u8 = 16;
const USBDEVFS_IOCTL_NR: u8 = 18;

/// Matches `struct usbdevfs_ctrltransfer` from linux/usbdevice_fs.h
#[repr(C)]
struct UsbdevfsCtrltransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,  // milliseconds
    data: *mut libc::c_void,
}

// ioctl number: USBDEVFS_CONTROL = _IOWR('U', 0, struct usbdevfs_ctrltransfer)
// size of struct = 2+1+1+2+2+2+4+ptr = varies by pointer width; use libc constant.
// We construct the ioctl number at runtime to avoid hard-coding architecture-specific values.
fn ioctl_usbdevfs_control(
    fd: libc::c_int,
    transfer: &mut UsbdevfsCtrltransfer,
) -> nix::Result<libc::c_int> {
    // SAFETY: transfer is a valid pointer; fd is a valid usbdevfs fd.
    unsafe {
        let nr = request_code_readwrite!(
            USBDEVFS_CONTROL_IOCTL,
            USBDEVFS_CONTROL_NR,
            std::mem::size_of::<UsbdevfsCtrltransfer>()
        );
        let ret = libc::ioctl(fd, nr, transfer as *mut UsbdevfsCtrltransfer);
        if ret < 0 {
            Err(nix::errno::Errno::last())
        } else {
            Ok(ret)
        }
    }
}

fn ioctl_claim_interface(fd: libc::c_int, iface: u32) -> nix::Result<()> {
    unsafe {
        let nr = request_code_readwrite!(
            USBDEVFS_CONTROL_IOCTL,
            USBDEVFS_CLAIMINTERFACE_NR,
            std::mem::size_of::<libc::c_uint>()
        );
        let ret = libc::ioctl(fd, nr, &iface as *const u32);
        if ret < 0 {
            Err(nix::errno::Errno::last())
        } else {
            Ok(())
        }
    }
}

fn ioctl_release_interface(fd: libc::c_int, iface: u32) -> nix::Result<()> {
    unsafe {
        let nr = request_code_readwrite!(
            USBDEVFS_CONTROL_IOCTL,
            USBDEVFS_RELEASEINTERFACE_NR,
            std::mem::size_of::<libc::c_uint>()
        );
        let ret = libc::ioctl(fd, nr, &iface as *const u32);
        if ret < 0 {
            Err(nix::errno::Errno::last())
        } else {
            Ok(())
        }
    }
}

// -----------------------------------------------------------------------
// Public backend entry point
// -----------------------------------------------------------------------

pub struct LinuxBackend;

impl UsbBackend for LinuxBackend {
    fn enumerate(&self) -> Result<Vec<DeviceInfo>, UsbError> {
        enumerate_udev_devices()
    }

    fn open(&self, path: &str) -> Result<Box<dyn UsbDevice>, UsbError> {
        let dev = LinuxDevice::open(path)?;
        Ok(Box::new(dev))
    }
}

// -----------------------------------------------------------------------
// Device enumeration via udev
// -----------------------------------------------------------------------

fn enumerate_udev_devices() -> Result<Vec<DeviceInfo>, UsbError> {
    let mut enumerator = udev::Enumerator::new().map_err(|e| UsbError::Io(e))?;
    enumerator
        .match_subsystem("usb")
        .map_err(|e| UsbError::Io(e))?;
    enumerator
        .match_property("DEVTYPE", "usb_device")
        .map_err(|e| UsbError::Io(e))?;

    let devices = enumerator.scan_devices().map_err(|e| UsbError::Io(e))?;

    let mut result = Vec::new();

    for udev_device in devices {
        // devnode is the /dev/bus/usb/BBB/DDD path
        let path = match udev_device.devnode() {
            Some(p) => p.to_string_lossy().to_string(),
            None => continue,
        };

        let vendor_id = parse_hex_attr(&udev_device, "idVendor");
        let product_id = parse_hex_attr(&udev_device, "idProduct");

        let bus_number = udev_device
            .attribute_value("busnum")
            .and_then(|v| v.to_str())
            .and_then(|s| s.trim().parse::<u8>().ok())
            .unwrap_or(0);

        let device_address = udev_device
            .attribute_value("devnum")
            .and_then(|v| v.to_str())
            .and_then(|s| s.trim().parse::<u8>().ok())
            .unwrap_or(0);

        // String attributes from udev (may not always be populated)
        let manufacturer = udev_device
            .attribute_value("manufacturer")
            .map(|v| v.to_string_lossy().to_string());
        let product = udev_device
            .attribute_value("product")
            .map(|v| v.to_string_lossy().to_string());
        let serial_number = udev_device
            .attribute_value("serial")
            .map(|v| v.to_string_lossy().to_string());

        result.push(DeviceInfo {
            vendor_id,
            product_id,
            bus_number,
            device_address,
            path,
            manufacturer,
            product,
            serial_number,
        });
    }

    Ok(result)
}

fn parse_hex_attr(dev: &udev::Device, attr: &str) -> u16 {
    dev.attribute_value(attr)
        .and_then(|v| v.to_str())
        .map(|s| s.trim())
        .and_then(|s| u16::from_str_radix(s, 16).ok())
        .unwrap_or(0)
}

// -----------------------------------------------------------------------
// LinuxDevice — wraps a usbdevfs file descriptor
// -----------------------------------------------------------------------

struct LinuxDevice {
    file: File,
}

impl LinuxDevice {
    /// Open a device by its /dev/bus/usb/BBB/DDD path.
    ///
    /// First tries read+write; falls back to read-only (descriptor reads still work).
    fn open(path: &str) -> Result<Self, UsbError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .or_else(|_| {
                // Fallback: read-only — descriptor reads work, host-to-device OUT transfers will fail.
                log::warn!("usbdevfs: read+write open failed for {path}; retrying read-only");
                OpenOptions::new()
                    .read(true)
                    .open(path)
            })
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    UsbError::PermissionDenied
                } else {
                    UsbError::Io(e)
                }
            })?;

        Ok(Self { file })
    }

    fn raw_control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        buf: &mut [u8],
        timeout_ms: u32,
    ) -> Result<usize, UsbError> {
        let mut transfer = UsbdevfsCtrltransfer {
            request_type,
            request,
            value,
            index,
            length: buf.len() as u16,
            timeout: timeout_ms,
            data: buf.as_mut_ptr() as *mut libc::c_void,
        };

        let fd = self.file.as_raw_fd();
        let n = ioctl_usbdevfs_control(fd, &mut transfer).map_err(|errno| {
            match errno {
                nix::errno::Errno::EPIPE => UsbError::Stall,
                nix::errno::Errno::ETIMEDOUT => UsbError::Timeout,
                nix::errno::Errno::ENODEV => UsbError::DeviceNotFound,
                other => UsbError::Other(other.to_string()),
            }
        })?;

        Ok(n as usize)
    }
}

impl UsbDevice for LinuxDevice {
    fn read_device_descriptor(&self) -> Result<DeviceDescriptor, UsbError> {
        let mut buf = [0u8; 18];
        // GET_DESCRIPTOR: type=0x01 (Device), index=0, lang=0
        let n = self.raw_control(0x80, 0x06, 0x0100, 0x0000, &mut buf, 5000)?;
        if n < 18 {
            return Err(UsbError::InvalidDescriptor);
        }
        DeviceDescriptor::from_bytes(&buf)
    }

    fn read_config_descriptor(&self, index: u8) -> Result<ConfigDescriptor, UsbError> {
        // First pass: read 9-byte header to get wTotalLength
        let mut hdr = [0u8; 9];
        let req_value = (0x02u16 << 8) | index as u16;
        let n = self.raw_control(0x80, 0x06, req_value, 0x0000, &mut hdr, 5000)?;
        if n < 9 {
            return Err(UsbError::InvalidDescriptor);
        }
        let total_len = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
        if total_len < 9 {
            return Err(UsbError::InvalidDescriptor);
        }

        // Second pass: read full descriptor
        let mut full = vec![0u8; total_len];
        self.raw_control(0x80, 0x06, req_value, 0x0000, &mut full, 5000)?;
        ConfigDescriptor::from_bytes(&full)
    }

    fn read_string_descriptor(&self, index: u8, lang: u16) -> Result<String, UsbError> {
        let mut buf = [0u8; 255];
        let req_value = (0x03u16 << 8) | index as u16;
        let n = self.raw_control(0x80, 0x06, req_value, lang, &mut buf, 5000)?;
        if n < 2 {
            return Err(UsbError::InvalidDescriptor);
        }
        let str_len = buf[0] as usize;
        if str_len < 2 || str_len > n {
            return Err(UsbError::InvalidDescriptor);
        }
        // String content starts at byte 2, UTF-16LE
        let chars: Vec<u16> = buf[2..str_len]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(String::from_utf16_lossy(&chars).to_string())
    }

    fn claim_interface(&mut self, interface: u8) -> Result<(), UsbError> {
        let fd = self.file.as_raw_fd();
        ioctl_claim_interface(fd, interface as u32).map_err(|errno| match errno {
            nix::errno::Errno::EBUSY => UsbError::Other("interface already claimed".into()),
            nix::errno::Errno::ENODEV => UsbError::DeviceNotFound,
            other => UsbError::Other(other.to_string()),
        })
    }

    fn release_interface(&mut self, interface: u8) -> Result<(), UsbError> {
        let fd = self.file.as_raw_fd();
        ioctl_release_interface(fd, interface as u32).map_err(|errno| match errno {
            nix::errno::Errno::ENODEV => UsbError::DeviceNotFound,
            other => UsbError::Other(other.to_string()),
        })
    }

    fn control_transfer(
        &self,
        setup: ControlSetup,
        data: Option<&mut [u8]>,
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;

        // If direction is IN (bit 7 of request_type set) we need a receive buffer.
        // If direction is OUT, we send data from the caller's slice.
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
            // OUT transfer — if caller supplies data, use it; otherwise send zero bytes.
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
