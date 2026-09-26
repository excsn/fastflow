#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Granted,
    Denied,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    ScreenRecording,
    InputMonitoring,
}

impl Permission {
    pub const ALL: [Permission; 2] = [Permission::ScreenRecording, Permission::InputMonitoring];

    pub fn label(self) -> &'static str {
        match self {
            Permission::ScreenRecording => "Screen Recording",
            Permission::InputMonitoring => "Input Monitoring",
        }
    }

    pub fn tcc_service(self) -> &'static str {
        match self {
            Permission::ScreenRecording => "ScreenCapture",
            Permission::InputMonitoring => "ListenEvent",
        }
    }

    pub fn settings_url(self) -> &'static str {
        match self {
            Permission::ScreenRecording => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            Permission::InputMonitoring => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use super::{Grant, Permission};

    // IOHIDRequestType and IOHIDAccessType from IOKit/hid/IOHIDLib.h
    const IOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;
    const IOHID_ACCESS_TYPE_GRANTED: u32 = 0;
    const IOHID_ACCESS_TYPE_DENIED: u32 = 1;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOHIDCheckAccess(request_type: u32) -> u32;
        fn IOHIDRequestAccess(request_type: u32) -> bool;
    }

    pub fn check(p: Permission) -> Grant {
        match p {
            // Preflight cannot tell denied from never asked.
            Permission::ScreenRecording => match unsafe { CGPreflightScreenCaptureAccess() } {
                true => Grant::Granted,
                false => Grant::Unknown,
            },
            Permission::InputMonitoring => {
                match unsafe { IOHIDCheckAccess(IOHID_REQUEST_TYPE_LISTEN_EVENT) } {
                    IOHID_ACCESS_TYPE_GRANTED => Grant::Granted,
                    IOHID_ACCESS_TYPE_DENIED => Grant::Denied,
                    _ => Grant::Unknown,
                }
            }
        }
    }

    pub fn request(p: Permission) -> bool {
        match p {
            Permission::ScreenRecording => unsafe { CGRequestScreenCaptureAccess() },
            Permission::InputMonitoring => unsafe {
                IOHIDRequestAccess(IOHID_REQUEST_TYPE_LISTEN_EVENT)
            },
        }
    }
}

#[cfg(target_os = "macos")]
pub use sys::{check, request};

/// Removes this app's entry for `p` so the next request prompts afresh.
/// A rebuilt ad-hoc binary no longer matches its old entry. Toggling the entry does not update it.
#[cfg(target_os = "macos")]
pub fn reset(p: Permission, bundle_id: &str) -> std::io::Result<std::process::Output> {
    std::process::Command::new("/usr/bin/tccutil")
        .args(["reset", p.tcc_service(), bundle_id])
        .output()
}
