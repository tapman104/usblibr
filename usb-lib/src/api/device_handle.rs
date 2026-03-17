use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::UsbDevice;
use crate::core::{
    BosDescriptor, ConfigDescriptor, ControlSetup, DeviceDescriptor, HubDescriptor, PipePolicy,
    PipePolicyKind,
};
use crate::error::UsbError;

/// A handle to an open USB device, providing descriptor reads and transfers.
///
/// This handle is thread-safe and can be cloned and shared across threads or
/// async tasks.
#[derive(Clone)]
pub struct DeviceHandle {
    inner: Arc<Mutex<Box<dyn UsbDevice>>>,
}

impl DeviceHandle {
    pub(crate) fn new(dev: Box<dyn UsbDevice>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(dev)),
        }
    }

    pub fn read_device_descriptor(&self) -> Result<DeviceDescriptor, UsbError> {
        self.inner.lock().unwrap().read_device_descriptor()
    }

    pub fn read_config_descriptor(&self, index: u8) -> Result<ConfigDescriptor, UsbError> {
        self.inner.lock().unwrap().read_config_descriptor(index)
    }

    pub fn read_string_descriptor(&self, index: u8, lang: u16) -> Result<String, UsbError> {
        self.inner.lock().unwrap().read_string_descriptor(index, lang)
    }

    pub fn claim_interface(&self, interface: u8) -> Result<(), UsbError> {
        self.inner.lock().unwrap().claim_interface(interface)
    }

    pub fn release_interface(&self, interface: u8) -> Result<(), UsbError> {
        self.inner.lock().unwrap().release_interface(interface)
    }

    pub fn control_transfer(
        &self,
        setup: ControlSetup,
        data: Option<&mut [u8]>,
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        self.inner.lock().unwrap().control_transfer(setup, data, timeout)
    }

    pub fn bulk_read(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        self.inner.lock().unwrap().bulk_read(endpoint, buf, timeout)
    }

    pub fn bulk_write(
        &self,
        endpoint: u8,
        buf: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        self.inner.lock().unwrap().bulk_write(endpoint, buf, timeout)
    }

    pub fn interrupt_read(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        self.inner.lock().unwrap().interrupt_read(endpoint, buf, timeout)
    }

    pub fn interrupt_write(
        &self,
        endpoint: u8,
        buf: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        self.inner.lock().unwrap().interrupt_write(endpoint, buf, timeout)
    }

    pub fn reset_pipe(&self, endpoint: u8) -> Result<(), UsbError> {
        self.inner.lock().unwrap().reset_pipe(endpoint)
    }

    pub fn abort_pipe(&self, endpoint: u8) -> Result<(), UsbError> {
        self.inner.lock().unwrap().abort_pipe(endpoint)
    }

    pub fn reset_device(&self) -> Result<(), UsbError> {
        self.inner.lock().unwrap().reset_device()
    }

    pub fn get_alternate_setting(&self, interface: u8) -> Result<u8, UsbError> {
        self.inner.lock().unwrap().get_alternate_setting(interface)
    }

    pub fn set_alternate_setting(&self, interface: u8, alt: u8) -> Result<(), UsbError> {
        self.inner.lock().unwrap().set_alternate_setting(interface, alt)
    }

    /// Query the live pipe information for `endpoint` from the host controller driver.
    pub fn get_pipe_info(&self, endpoint: u8) -> Result<crate::core::EndpointInfo, UsbError> {
        self.inner.lock().unwrap().get_pipe_info(endpoint)
    }

    /// Read the current value of a pipe policy.
    pub fn get_pipe_policy(
        &self,
        endpoint: u8,
        kind: PipePolicyKind,
    ) -> Result<PipePolicy, UsbError> {
        self.inner.lock().unwrap().get_pipe_policy(endpoint, kind)
    }

    /// Write a pipe policy value.
    pub fn set_pipe_policy(&self, endpoint: u8, policy: PipePolicy) -> Result<(), UsbError> {
        self.inner.lock().unwrap().set_pipe_policy(endpoint, policy)
    }

    /// Read the Binary Object Store (BOS) descriptor from the device.
    pub fn read_bos_descriptor(&self) -> Result<BosDescriptor, UsbError> {
        self.inner.lock().unwrap().read_bos_descriptor()
    }

    /// Read the USB Hub descriptor (hub devices only).
    pub fn read_hub_descriptor(&self) -> Result<HubDescriptor, UsbError> {
        self.inner.lock().unwrap().read_hub_descriptor()
    }

    /// Bulk IN transfer via overlapped I/O with a hard timeout.
    pub fn async_bulk_read(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        self.inner.lock().unwrap().async_bulk_read(endpoint, buf, timeout)
    }

    /// Bulk OUT transfer via overlapped I/O with a hard timeout.
    pub fn async_bulk_write(
        &self,
        endpoint: u8,
        buf: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        self.inner.lock().unwrap().async_bulk_write(endpoint, buf, timeout)
    }

    /// Interrupt IN transfer via overlapped I/O with a hard timeout.
    pub fn async_interrupt_read(
        &self,
        endpoint: u8,
        buf: &mut [u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        self.inner.lock().unwrap().async_interrupt_read(endpoint, buf, timeout)
    }

    /// Interrupt OUT transfer via overlapped I/O with a hard timeout.
    pub fn async_interrupt_write(
        &self,
        endpoint: u8,
        buf: &[u8],
        timeout: Duration,
    ) -> Result<usize, UsbError> {
        self.inner.lock().unwrap().async_interrupt_write(endpoint, buf, timeout)
    }

    /// Read from an isochronous IN endpoint.
    ///
    /// Only functional when the `isochronous` feature is enabled and the
    /// platform backend supports isochronous transfers.
    pub fn isoch_read(&self, endpoint: u8, buf: &mut [u8]) -> Result<usize, UsbError> {
        self.inner.lock().unwrap().isoch_read(endpoint, buf)
    }

    /// Write to an isochronous OUT endpoint.
    ///
    /// Only functional when the `isochronous` feature is enabled and the
    /// platform backend supports isochronous transfers.
    pub fn isoch_write(&self, endpoint: u8, buf: &[u8]) -> Result<usize, UsbError> {
        self.inner.lock().unwrap().isoch_write(endpoint, buf)
    }
}
