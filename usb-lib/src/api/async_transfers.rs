//! Tokio-backed async wrappers for bulk and interrupt transfers.
//!
//! These functions wrap the blocking OVERLAPPED-based `async_bulk_*` /
//! `async_interrupt_*` methods from [`DeviceHandle`] inside
//! [`tokio::task::spawn_blocking`], yielding control to the Tokio runtime
//! while the I/O is in-flight.
//!
//! # Feature flag
//!
//! This module is compiled only when the `tokio` feature is enabled:
//!
//! ```toml
//! rust-usb = { version = "0.1", features = ["tokio"] }
//! ```
//!
//! # Thread safety
//!
//! [`DeviceHandle`] is not `Clone` and is not `Sync`.  Each async call
//! therefore owns an exclusive reference for the duration of the
//! `spawn_blocking` closure.  To share a handle across concurrent async tasks,
//! wrap it in a `tokio::sync::Mutex<DeviceHandle>` and call these helpers
//! while holding the lock.

use std::time::Duration;

#[cfg(feature = "tokio")]
use tokio::task::spawn_blocking;

use crate::api::device_handle::DeviceHandle;
use crate::error::UsbError;

/// Perform a bulk IN transfer from `endpoint` into `buf`, returning the
/// number of bytes received.
///
/// This function uses [`tokio::task::spawn_blocking`] to yield control to the
/// Tokio runtime while the I/O is in-flight. It works on both multi-threaded
/// and single-threaded runtimes.
pub async fn bulk_read(
    handle: &mut DeviceHandle,
    endpoint: u8,
    buf: &mut Vec<u8>,
    timeout: Duration,
) -> Result<usize, UsbError> {
    let cap = buf.len();
    let mut inner_buf = std::mem::take(buf);
    inner_buf.resize(cap, 0);

    let h = handle.clone();
    let (inner_buf, result) = spawn_blocking(move || {
        let res = h.async_bulk_read(endpoint, &mut inner_buf, timeout);
        (inner_buf, res)
    })
    .await
    .map_err(|e| UsbError::Other(format!("spawn_blocking failed: {e}")))?;

    *buf = inner_buf;
    result
}

/// Perform a bulk OUT transfer of `buf` to `endpoint`, returning the number
/// of bytes sent.
pub async fn bulk_write(
    handle: &mut DeviceHandle,
    endpoint: u8,
    buf: &[u8],
    timeout: Duration,
) -> Result<usize, UsbError> {
    let h = handle.clone();
    let data = buf.to_vec();
    spawn_blocking(move || h.async_bulk_write(endpoint, &data, timeout))
        .await
        .map_err(|e| UsbError::Other(format!("spawn_blocking failed: {e}")))?
}

/// Perform an interrupt IN transfer from `endpoint` into `buf`.
pub async fn interrupt_read(
    handle: &mut DeviceHandle,
    endpoint: u8,
    buf: &mut Vec<u8>,
    timeout: Duration,
) -> Result<usize, UsbError> {
    let cap = buf.len();
    let mut inner_buf = std::mem::take(buf);
    inner_buf.resize(cap, 0);

    let h = handle.clone();
    let (inner_buf, result) = spawn_blocking(move || {
        let res = h.async_interrupt_read(endpoint, &mut inner_buf, timeout);
        (inner_buf, res)
    })
    .await
    .map_err(|e| UsbError::Other(format!("spawn_blocking failed: {e}")))?;

    *buf = inner_buf;
    result
}

/// Perform an interrupt OUT transfer of `buf` to `endpoint`.
pub async fn interrupt_write(
    handle: &mut DeviceHandle,
    endpoint: u8,
    buf: &[u8],
    timeout: Duration,
) -> Result<usize, UsbError> {
    let h = handle.clone();
    let data = buf.to_vec();
    spawn_blocking(move || h.async_interrupt_write(endpoint, &data, timeout))
        .await
        .map_err(|e| UsbError::Other(format!("spawn_blocking failed: {e}")))?
}
