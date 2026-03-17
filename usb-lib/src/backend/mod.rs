use std::time::Duration;

use crate::core::{
    BosDescriptor, ConfigDescriptor, ControlSetup, DeviceDescriptor, DeviceInfo, EndpointInfo,
    HubDescriptor, PipePolicy, PipePolicyKind,
};
use crate::error::UsbError;

// Platform backend modules — each compiled only on its target OS.
#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "windows")]
pub mod windows;

pub mod mock;

/// Trait implemented by each platform backend.
/// The backend is responsible for enumeration and opening devices.
pub trait UsbBackend: Send + Sync {
    /// Enumerate all connected USB devices visible to this backend.
    fn enumerate(&self) -> Result<Vec<DeviceInfo>, UsbError>;

    /// Open a device by its platform path and return a boxed device handle.
    fn open(&self, path: &str) -> Result<Box<dyn UsbDevice>, UsbError>;
}

/// Trait representing an open USB device capable of descriptor reads and transfers.
pub trait UsbDevice: Send {
    fn read_device_descriptor(&self) -> Result<DeviceDescriptor, UsbError>;

    fn read_config_descriptor(&self, index: u8) -> Result<ConfigDescriptor, UsbError>;

    /// Read a USB string descriptor.
    /// `lang` should be 0x0409 (English) for normal strings, or 0x0000 (index 0)
    /// to enumerate supported language IDs.
    fn read_string_descriptor(&self, index: u8, lang: u16) -> Result<String, UsbError>;

    fn claim_interface(&mut self, interface: u8) -> Result<(), UsbError>;

    fn release_interface(&mut self, interface: u8) -> Result<(), UsbError>;

    fn control_transfer(
        &self,
        setup: ControlSetup,
        data: Option<&mut [u8]>,
        timeout: Duration,
    ) -> Result<usize, UsbError>;

    // -----------------------------------------------------------------------
    // Phase 3 — Data transfers
    // Default implementations return `Unsupported` so that platform backends
    // can be promoted incrementally without breaking the build.
    // -----------------------------------------------------------------------

    /// Read bytes from a bulk IN endpoint.
    /// `endpoint` is the full USB endpoint address (e.g. `0x81` for EP1-IN).
    fn bulk_read(
        &self,
        _endpoint: u8,
        _buf: &mut [u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Write bytes to a bulk OUT endpoint.
    /// `endpoint` is the full USB endpoint address (e.g. `0x01` for EP1-OUT).
    fn bulk_write(
        &self,
        _endpoint: u8,
        _buf: &[u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Read bytes from an interrupt IN endpoint.
    fn interrupt_read(
        &self,
        _endpoint: u8,
        _buf: &mut [u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Write bytes to an interrupt OUT endpoint.
    fn interrupt_write(
        &self,
        _endpoint: u8,
        _buf: &[u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        Err(UsbError::Unsupported)
    }

    // -----------------------------------------------------------------------
    // Phase 3 — Pipe management
    // -----------------------------------------------------------------------

    /// Clear a halted (stalled) pipe and reset the endpoint data toggle.
    fn reset_pipe(&self, _endpoint: u8) -> Result<(), UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Abort all pending I/O on a pipe without resetting the data toggle.
    fn abort_pipe(&self, _endpoint: u8) -> Result<(), UsbError> {
        Err(UsbError::Unsupported)
    }

    // -----------------------------------------------------------------------
    // Phase 3 — Device control
    // -----------------------------------------------------------------------

    /// Cycle the device through a bus reset.
    fn reset_device(&self) -> Result<(), UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Return the active alternate setting for an interface (must be claimed).
    fn get_alternate_setting(&self, _interface: u8) -> Result<u8, UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Select an alternate setting for an interface (must be claimed first).
    fn set_alternate_setting(&mut self, _interface: u8, _alt: u8) -> Result<(), UsbError> {
        Err(UsbError::Unsupported)
    }

    // -----------------------------------------------------------------------
    // Phase 3 — Pipe information and policy
    // -----------------------------------------------------------------------

    /// Query the runtime pipe information for a given endpoint address.
    /// Returns an [`EndpointInfo`] populated from the driver's pipe descriptor.
    fn get_pipe_info(&self, _endpoint: u8) -> Result<EndpointInfo, UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Read the current value of a pipe policy.
    ///
    /// `endpoint` — full USB endpoint address (e.g. `0x81` for EP1-IN).
    /// `kind`     — which policy to read.
    fn get_pipe_policy(
        &self,
        _endpoint: u8,
        _kind: PipePolicyKind,
    ) -> Result<PipePolicy, UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Write a pipe policy value.
    ///
    /// `endpoint` — full USB endpoint address.
    /// `policy`   — the policy variant carrying its value.
    fn set_pipe_policy(&self, _endpoint: u8, _policy: PipePolicy) -> Result<(), UsbError> {
        Err(UsbError::Unsupported)
    }

    // -----------------------------------------------------------------------
    // Advanced descriptors
    // -----------------------------------------------------------------------

    /// Read the Binary Object Store (BOS) descriptor from the device.
    ///
    /// Only available on USB 2.0+ devices with a BOS descriptor.
    fn read_bos_descriptor(&self) -> Result<BosDescriptor, UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Read the Hub descriptor via a class-specific GET_DESCRIPTOR request.
    ///
    /// Only succeeds on USB hub devices (bDeviceClass = 0x09).
    fn read_hub_descriptor(&self) -> Result<HubDescriptor, UsbError> {
        Err(UsbError::Unsupported)
    }

    // -----------------------------------------------------------------------
    // Overlapped (OS-async) bulk and interrupt transfers
    //
    // These methods use the OS-level overlapped I/O mechanism (OVERLAPPED on
    // Windows) to submit a transfer and wait for completion with a timeout.
    // They are distinct from `bulk_read`/`bulk_write` which apply a pipe
    // timeout policy and issue synchronous I/O internally.
    // -----------------------------------------------------------------------

    /// Submit a bulk IN transfer using overlapped I/O and wait up to `timeout`.
    fn async_bulk_read(
        &self,
        _endpoint: u8,
        _buf: &mut [u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Submit a bulk OUT transfer using overlapped I/O and wait up to `timeout`.
    fn async_bulk_write(
        &self,
        _endpoint: u8,
        _buf: &[u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Submit an interrupt IN transfer using overlapped I/O and wait up to `timeout`.
    fn async_interrupt_read(
        &self,
        _endpoint: u8,
        _buf: &mut [u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Submit an interrupt OUT transfer using overlapped I/O and wait up to `timeout`.
    fn async_interrupt_write(
        &self,
        _endpoint: u8,
        _buf: &[u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        Err(UsbError::Unsupported)
    }

    // -----------------------------------------------------------------------
    // Isochronous transfers (feature = "isochronous")
    //
    // Uses WinUsb_RegisterIsochBuffer / WinUsb_ReadIsochPipeAsap /
    // WinUsb_WriteIsochPipeAsap on Windows.  Non-Windows targets and builds
    // without the feature always return `UsbError::Unsupported`.
    // -----------------------------------------------------------------------

    /// Read from an isochronous IN endpoint.
    ///
    /// `endpoint` — full USB endpoint address (e.g. `0x81` for EP1-IN).
    /// `buf`      — destination buffer; length determines the transfer size.
    ///
    /// Enabled by the `isochronous` feature flag.
    fn isoch_read(&self, _endpoint: u8, _buf: &mut [u8]) -> Result<usize, UsbError> {
        Err(UsbError::Unsupported)
    }

    /// Write to an isochronous OUT endpoint.
    ///
    /// `endpoint` — full USB endpoint address (e.g. `0x01` for EP1-OUT).
    /// `buf`      — data to transmit.
    ///
    /// Enabled by the `isochronous` feature flag.
    fn isoch_write(&self, _endpoint: u8, _buf: &[u8]) -> Result<usize, UsbError> {
        Err(UsbError::Unsupported)
    }
}

// Platform selection — only one backend is compiled per target OS.
#[cfg(target_os = "linux")]
pub use self::linux::LinuxBackend as PlatformBackend;

#[cfg(target_os = "windows")]
pub use self::windows::WindowsBackend as PlatformBackend;

#[cfg(target_os = "macos")]
pub use self::macos::MacOsBackend as PlatformBackend;
