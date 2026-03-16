# usblibr

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

| Feature | Windows | Linux | macOS |
|---|---|---|---|
| Enumeration | ✅ WinUSB | ✅ udev | ✅ IOKit |
| Control transfers | ✅ | ✅ USBDEVFS | ✅ IOUSBDevice |
| Bulk/interrupt | ✅ | ✅ | ✅ |
| Isochronous | ✅ (feature flag) | 🚧 | 🚧 |
| Hotplug | ✅ | ✅ | ✅ |
| Async (Tokio) | ✅ (feature flag) | ✅ | ✅ |

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
usblibr = "0.1"
```

Optional features:

```toml
[dependencies]
usblibr = { version = "0.1", features = ["isochronous", "tokio"] }
```

## Quick Start

```rust
use usblibr::UsbContext;

fn main() -> anyhow::Result<()> {
    let ctx = UsbContext::new()?;

    for device in ctx.list_devices()? {
        println!(
            "VID={:04x} PID={:04x}  {}",
            device.vendor_id,
            device.product_id,
            device.product.as_deref().unwrap_or("(unknown)")
        );
    }

    Ok(())
}
```

Open a device and perform a control transfer:

```rust
use usblibr::{UsbContext, ControlSetup, Direction, RequestType, Recipient};

let ctx = UsbContext::new()?;
let devices = ctx.list_devices()?;
let info = devices.into_iter().find(|d| d.vendor_id == 0x1234).unwrap();

let mut handle = ctx.open_device(&info)?;
handle.claim_interface(0)?;

let setup = ControlSetup::new(
    Direction::DeviceToHost,
    RequestType::Standard,
    Recipient::Device,
    0x06, // GET_DESCRIPTOR
    0x0100,
    0x0000,
);

let mut buf = [0u8; 18];
let n = handle.control_transfer_in(&setup, &mut buf, 1000)?;
println!("Received {} bytes", n);
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
