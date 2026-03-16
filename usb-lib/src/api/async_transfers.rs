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
//! usb-lib = { version = "0.1", features = ["tokio"] }
//! ```
//!
//! # Thread safety
//!
//! [`DeviceHandle`] is not `Clone` and is not `Sync`.  Each async call
//! therefore owns an exclusive reference for the duration of the
//! `spawn_blocking` closure.  To share a handle across concurrent async tasks,
//! wrap it in a `tokio::sync::Mutex<DeviceHandle>` and call these helpers
//! while holding the lock.

#![allow(clippy::unused_async)] // block_in_place needs an async context but has no .await

use std::time::Duration;

use crate::error::UsbError;

// We re-export DeviceHandle so callers only need one import.
use crate::api::device_handle::DeviceHandle;

/// Perform a bulk IN transfer from `endpoint` into `buf`, returning the
/// number of bytes received.
///
/// The call blocks a Tokio blocking thread (via `spawn_blocking`) while the
/// underlying OVERLAPPED wait is in progress.
///
/// # Errors
///
/// Returns [`UsbError::Timeout`] if the transfer does not complete within
/// `timeout`, or another [`UsbError`] variant on I/O failure.
pub async fn bulk_read(
    handle: &mut DeviceHandle,
    endpoint: u8,
    buf: &mut Vec<u8>,
    timeout: Duration,
) -> Result<usize, UsbError> {
    // We need ownership to send into spawn_blocking.
    // Swap the buffer out temporarily so we can move it into the closure.
    let cap = buf.len();
    let mut inner_buf = std::mem::take(buf);
    inner_buf.resize(cap, 0);

    let result = tokio::task::block_in_place(|| {
        handle.async_bulk_read(endpoint, &mut inner_buf, timeout)
    });

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
    tokio::task::block_in_place(|| handle.async_bulk_write(endpoint, buf, timeout))
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

    let result = tokio::task::block_in_place(|| {
        handle.async_interrupt_read(endpoint, &mut inner_buf, timeout)
    });

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
    tokio::task::block_in_place(|| handle.async_interrupt_write(endpoint, buf, timeout))
}
