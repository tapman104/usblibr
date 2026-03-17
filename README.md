# rust-usb

A cross-platform Rust library for USB device communication on **Windows**, **Linux**, and **macOS**.

## Features

- **Device enumeration** — list connected USB devices with VID/PID, serial, manufacturer, and product strings
- **Control transfers** — typed `ControlSetup` builder for IN/OUT control requests
- **Bulk & interrupt transfers** — synchronous read/write with configurable timeout
- **Isochronous transfers** — enabled via the `isochronous` feature flag (Windows/WinUSB only)
- **Pipe management** — query, reset, and abort pipes; get/set pipe policies (7 built-in policies)
- **Descriptor reading** — Device, Configuration, String, HID, BOS, Hub, SuperSpeed Endpoint Companion
- **Async transfers** — OVERLAPPED I/O backend with optional Tokio wrappers (`tokio` feature)
- **Hotplug detection** — `CM_Register_Notification` on Windows; udev monitor on Linux; IOKit on macOS
- **Multi-interface support** — per-interface endpoint cache via `HashMap`

## Platform Support

| Feature | Windows | Linux | macOS | Mock |
|---|---|---|---|---|
| Enumeration | ✅ WinUSB | ✅ udev | ✅ IOKit | ✅ In-memory |
| Control Transfers | ✅ | ✅ | ✅ | ✅ |
| Bulk/Interrupt | ✅ | ✅ | ✅ | ✅ |
| Isochronous | ✅ (`isochronous`) | 🚧 | 🚧 | ❌ |
| Hotplug | ✅ | ✅ | ✅ | ❌ |
| Async (Tokio) | ✅ (`tokio`) | ✅ | ✅ | ❌ |

> [!NOTE]
> The `async_transfers` module (enabled via the `tokio` feature) requires a **multi-threaded Tokio runtime** because it utilizes `tokio::task::block_in_place` to bridge synchronous I/O.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
rust-usb = "0.1"
```

## Quick Start

### 1. Listing connected devices

```rust
use rust_usb::UsbContext;

let ctx = UsbContext::new();
let devices = ctx.devices()?;

for device in devices {
    println!("Device at {}: {:04x}:{:04x}", device.path, device.vendor_id, device.product_id);
}
```

### 2. Opening a device and performing I/O

```rust
use std::time::Duration;
use rust_usb::UsbContext;

let ctx = UsbContext::new();
let mut handle = ctx.open("platform-specific-path")?;

// Interfaces must be claimed before performing pipe I/O
handle.claim_interface(0)?;

let mut buf = [0u8; 64];
let timeout = Duration::from_secs(1);

// Bulk Read from EP 0x81
let n = handle.bulk_read(0x81, &mut buf, timeout)?;

// Bulk Write to EP 0x01
handle.bulk_write(0x01, &buf[..n], timeout)?;
```

### 3. Monitoring device arrivals and departures

```rust
use rust_usb::{UsbContext, HotplugEvent};

let ctx = UsbContext::new();

// The handle keeps the subscription alive; drop it to unregister
let _handle = ctx.register_hotplug(|event| {
    match event {
        HotplugEvent::DeviceArrived { path } => println!("Device arrived: {}", path),
        HotplugEvent::DeviceLeft { path } => println!("Device removed: {}", path),
    }
})?;
```

## Examples

Run the bundled example:

```sh
cargo run --example list_devices
```

## Development

See [DEVELOPMENT.md](DEVELOPMENT.md) for architecture decisions, feature flag guidance, and platform-specific notes.

## Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) and open an issue before submitting a PR.

## License

Licensed under the [MIT License](LICENSE).
