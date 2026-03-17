use rust_usb::{DeviceDescriptor, ConfigDescriptor, UsbError};

#[test]
fn test_parse_valid_device_descriptor() {
    let buf = vec![
        18,   // bLength
        0x01, // bDescriptorType (Device)
        0x00, 0x02, // bcdUSB (2.00)
        0xFF, // bDeviceClass (Vendor)
        0x00, // bDeviceSubClass
        0x00, // bDeviceProtocol
        64,   // bMaxPacketSize0
        0x34, 0x12, // idVendor (0x1234)
        0x78, 0x56, // idProduct (0x5678)
        0x00, 0x01, // bcdDevice (1.00)
        1,    // iManufacturer
        2,    // iProduct
        3,    // iSerialNumber
        1,    // bNumConfigurations
    ];

    let desc = DeviceDescriptor::from_bytes(&buf).expect("Should parse");
    assert_eq!(desc.vendor_id, 0x1234);
    assert_eq!(desc.product_id, 0x5678);
    assert_eq!(desc.device_class, 0xFF);
}

#[test]
fn test_parse_config_descriptor_too_short() {
    let buf = vec![9, 0x02, 0x09, 0x00, 1, 1, 0, 0x80]; // Missing 9th byte (max power)
    let result = ConfigDescriptor::from_bytes(&buf);
    assert!(matches!(result, Err(UsbError::InvalidDescriptor)));
}

#[test]
fn test_parse_config_with_interface_and_endpoint() {
    let buf = vec![
        // Configuration Header (9 bytes)
        9,    // bLength
        0x02, // bDescriptorType (Config)
        25, 0x00, // wTotalLength (9 + 9 + 7 = 25)
        1,    // bNumInterfaces
        1,    // bConfigurationValue
        0,    // iConfiguration
        0x80, // bmAttributes
        50,   // bMaxPower (100mA)

        // Interface Descriptor (9 bytes)
        9,    // bLength
        0x04, // bDescriptorType (Interface)
        0,    // bInterfaceNumber
        0,    // bAlternateSetting
        1,    // bNumEndpoints
        0xFF, // bInterfaceClass
        0x00, // bInterfaceSubClass
        0x00, // bInterfaceProtocol
        0,    // iInterface

        // Endpoint Descriptor (7 bytes)
        7,    // bLength
        0x05, // bDescriptorType (Endpoint)
        0x81, // bEndpointAddress (EP1 IN)
        0x02, // bmAttributes (Bulk)
        64, 0x00, // wMaxPacketSize (64)
        0,    // bInterval
    ];

    let desc = ConfigDescriptor::from_bytes(&buf).expect("Should parse");
    assert_eq!(desc.num_interfaces, 1);
    assert_eq!(desc.interfaces.len(), 1);
    
    let iface = &desc.interfaces[0];
    assert_eq!(iface.interface_number, 0);
    assert_eq!(iface.endpoints.len(), 1);
    
    let ep = &iface.endpoints[0];
    assert_eq!(ep.endpoint_address, 0x81);
    assert_eq!(ep.max_packet_size, 64);
}
