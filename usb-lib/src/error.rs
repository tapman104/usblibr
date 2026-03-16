use std::io;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UsbError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid descriptor data")]
    InvalidDescriptor,

    #[error("device not found")]
    DeviceNotFound,

    #[error("permission denied — try running as administrator or check device access rules")]
    PermissionDenied,

    #[error("transfer timed out")]
    Timeout,

    #[error("endpoint stall — call reset_pipe and retry")]
    Stall,

    #[error("invalid handle — device may have been disconnected")]
    InvalidHandle,

    #[error("operation not supported on this platform")]
    Unsupported,

    #[error("USB error: {0}")]
    Other(String),
}

#[cfg(target_os = "windows")]
impl From<windows::core::Error> for UsbError {
    fn from(e: windows::core::Error) -> Self {
        // Windows errors from Win32 APIs are wrapped as HRESULT_FROM_WIN32(code),
        // producing 0x8007xxxx values.  Match against those full HRESULT codes.
        #[allow(clippy::cast_sign_loss)] // intentional: high-bit set on failure HRESULTs
        match e.code().0 as u32 {
            // HRESULT_FROM_WIN32(ERROR_ACCESS_DENIED)
            0x8007_0005 => UsbError::PermissionDenied,
            // HRESULT_FROM_WIN32(ERROR_INVALID_HANDLE)
            0x8007_0006 => UsbError::InvalidHandle,
            // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND | ERROR_NO_SUCH_DEVICE | ERROR_DEVICE_NOT_CONNECTED)
            0x8007_0002 | 0x8007_01B1 | 0x8007_048F => UsbError::DeviceNotFound,
            // HRESULT_FROM_WIN32(ERROR_SEM_TIMEOUT)
            0x8007_0079 => UsbError::Timeout,
            // HRESULT_FROM_WIN32(ERROR_BAD_COMMAND) — endpoint stall
            0x8007_0016 => UsbError::Stall,
            _ => UsbError::Other(e.to_string()),
        }
    }
}
