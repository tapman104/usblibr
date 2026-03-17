use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::core::{ConfigDescriptor, ControlSetup, DeviceDescriptor, DeviceInfo};
use crate::error::UsbError;

use super::{UsbBackend, UsbDevice};

/// A mock backend for testing without physical USB hardware.
pub struct MockBackend {
    devices: Arc<Mutex<HashMap<String, MockDeviceInfo>>>,
}

struct MockDeviceInfo {
    info: DeviceInfo,
    descriptor: DeviceDescriptor,
    configs: Vec<ConfigDescriptor>,
    strings: HashMap<(u8, u16), String>,
}

impl MockBackend {
    /// Create a new mock backend with no devices.
    pub fn new() -> Self {
        Self {
            devices: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Add a simulated device to the backend.
    pub fn add_device(
        &self,
        info: DeviceInfo,
        descriptor: DeviceDescriptor,
        configs: Vec<ConfigDescriptor>,
        strings: HashMap<(u8, u16), String>,
    ) {
        let mut devices = self.devices.lock().unwrap();
        devices.insert(
            info.path.clone(),
            MockDeviceInfo {
                info,
                descriptor,
                configs,
                strings,
            },
        );
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl UsbBackend for MockBackend {
    fn enumerate(&self) -> Result<Vec<DeviceInfo>, UsbError> {
        let devices = self.devices.lock().unwrap();
        Ok(devices.values().map(|d| d.info.clone()).collect())
    }

    fn open(&self, path: &str) -> Result<Box<dyn UsbDevice>, UsbError> {
        let devices = self.devices.lock().unwrap();
        let dev_info = devices.get(path).ok_or(UsbError::DeviceNotFound)?;

        Ok(Box::new(MockDevice {
            descriptor: dev_info.descriptor.clone(),
            configs: dev_info.configs.clone(),
            strings: dev_info.strings.clone(),
            claimed_interfaces: Mutex::new(Vec::new()),
        }))
    }
}

pub struct MockDevice {
    descriptor: DeviceDescriptor,
    configs: Vec<ConfigDescriptor>,
    strings: HashMap<(u8, u16), String>,
    claimed_interfaces: Mutex<Vec<u8>>,
}

impl UsbDevice for MockDevice {
    fn read_device_descriptor(&self) -> Result<DeviceDescriptor, UsbError> {
        Ok(self.descriptor.clone())
    }

    fn read_config_descriptor(&self, index: u8) -> Result<ConfigDescriptor, UsbError> {
        self.configs
            .get(index as usize)
            .cloned()
            .ok_or(UsbError::InvalidDescriptor)
    }

    fn read_string_descriptor(&self, index: u8, lang: u16) -> Result<String, UsbError> {
        self.strings
            .get(&(index, lang))
            .cloned()
            .ok_or(UsbError::InvalidDescriptor)
    }

    fn claim_interface(&mut self, interface: u8) -> Result<(), UsbError> {
        let mut claimed = self.claimed_interfaces.lock().unwrap();
        if !claimed.contains(&interface) {
            claimed.push(interface);
        }
        Ok(())
    }

    fn release_interface(&mut self, interface: u8) -> Result<(), UsbError> {
        let mut claimed = self.claimed_interfaces.lock().unwrap();
        if let Some(pos) = claimed.iter().position(|&i| i == interface) {
            claimed.remove(pos);
            Ok(())
        } else {
            Err(UsbError::Other("Interface not claimed".into()))
        }
    }

    fn control_transfer(
        &self,
        _setup: ControlSetup,
        _data: Option<&mut [u8]>,
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        // Basic mock: just return success for anything.
        // Real mocks would inspect the setup packet.
        Ok(0)
    }

    fn bulk_read(
        &self,
        _endpoint: u8,
        _buf: &mut [u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        Ok(0)
    }

    fn bulk_write(
        &self,
        _endpoint: u8,
        _buf: &[u8],
        _timeout: Duration,
    ) -> Result<usize, UsbError> {
        Ok(_buf.len())
    }
}
