use std::collections::HashMap;
use std::time::Duration;
use rust_usb::backend::mock::MockBackend;
use rust_usb::backend::UsbBackend;
use rust_usb::{DeviceInfo, DeviceDescriptor, ConfigDescriptor, UsbError};

fn create_mock_device(path: &str) -> (DeviceInfo, DeviceDescriptor, Vec<ConfigDescriptor>, HashMap<(u8, u16), String>) {
    let info = DeviceInfo {
        bus_number: 1,
        device_address: 2,
        vendor_id: 0x1234,
        product_id: 0x5678,
        path: path.to_string(),
        manufacturer: Some("Mock Corp".into()),
        product: Some("Mock Device".into()),
        serial_number: Some("ABC-123".into()),
    };

    let descriptor = DeviceDescriptor {
        bcd_usb: 0x0200,
        device_class: 0xFF,
        device_sub_class: 0x00,
        device_protocol: 0x00,
        max_packet_size0: 64,
        vendor_id: 0x1234,
        product_id: 0x5678,
        bcd_device: 0x0100,
        manufacturer_index: 1,
        product_index: 2,
        serial_number_index: 3,
        num_configurations: 1,
    };

    let config = ConfigDescriptor {
        total_length: 9,
        num_interfaces: 0,
        configuration_value: 1,
        configuration_index: 0,
        attributes: 0x80,
        max_power: 50,
        interfaces: Vec::new(),
    };

    (info, descriptor, vec![config], HashMap::new())
}

#[test]
fn test_mock_enumeration() {
    let backend = MockBackend::new();
    let (info, desc, configs, strings) = create_mock_device("mock/path/1");
    backend.add_device(info.clone(), desc, configs, strings);

    let devices = backend.enumerate().expect("Should enumerate");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].path, "mock/path/1");
    assert_eq!(devices[0].vendor_id, 0x1234);
}

#[test]
fn test_mock_open() {
    let backend = MockBackend::new();
    let (info, desc, configs, strings) = create_mock_device("mock/path/1");
    backend.add_device(info, desc, configs, strings);

    // Open valid path
    let handle = backend.open("mock/path/1");
    assert!(handle.is_ok());

    // Open unknown path
    let handle_err = backend.open("mock/path/unknown");
    assert!(matches!(handle_err, Err(UsbError::DeviceNotFound)));
}

#[test]
fn test_mock_device_operations() {
    let backend = MockBackend::new();
    let (info, desc, configs, strings) = create_mock_device("mock/path/1");
    backend.add_device(info, desc, configs, strings);

    let mut device = backend.open("mock/path/1").expect("Should open");

    // Read descriptor
    let d = device.read_device_descriptor().expect("Should read desc");
    assert_eq!(d.vendor_id, 0x1234);

    // Claim/Release interface
    device.claim_interface(0).expect("Should claim");
    device.claim_interface(0).expect("Should claim silently if already claimed");
    device.release_interface(0).expect("Should release");
    
    let release_err = device.release_interface(0);
    assert!(release_err.is_err(), "Should err on releasing unclaimed interface");

    // Bulk write
    let data = [1, 2, 3, 4];
    let n = device.bulk_write(0x01, &data, Duration::from_secs(1)).expect("Should write");
    assert_eq!(n, 4);

    // Pipe policy (unsupported on mock)
    let policy_err = device.get_pipe_policy(0x01, rust_usb::PipePolicyKind::TransferTimeout);
    assert!(matches!(policy_err, Err(UsbError::Unsupported)));

    let set_policy_err = device.set_pipe_policy(0x01, rust_usb::PipePolicy::TransferTimeout(1000));
    assert!(matches!(set_policy_err, Err(UsbError::Unsupported)));
}
