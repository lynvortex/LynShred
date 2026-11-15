use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

/// Check if the drive containing the given path is an SSD (Windows only).
/// Primary method: IOCTL_STORAGE_QUERY_PROPERTY (StorageDeviceSeekPenaltyProperty)
/// on the volume handle — works without administrator privileges.
/// Fallback: `fsutil volume disktype` (requires elevation).
/// Supports both English and Chinese locale output for the fallback path.
pub fn is_ssd_drive(path: &str) -> bool {
    if cfg!(not(target_os = "windows")) {
        return false;
    }

    let p = Path::new(path);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(p),
            Err(_) => return false,
        }
    };

    let drive = match extract_drive_letter(&p) {
        Some(d) => d,
        None => return false,
    };

    if let Some(ssd) = volume_is_ssd(&drive) {
        return ssd;
    }

    fsutil_is_ssd(&drive)
}

/// Extract "C:" style drive letter from a path, tolerating the
/// `\\?\C:\...` and `\\?\UNC\...` prefixes produced by canonicalize().
fn extract_drive_letter(path: &Path) -> Option<String> {
    let s = path.to_str()?;
    let s = s
        .trim_start_matches(r"\\?\UNC\")
        .trim_start_matches(r"\\?\");

    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(s[..2].to_string())
    } else {
        None
    }
}

/// Query seek-penalty property on the volume device via DeviceIoControl.
/// Returns Some(true) for SSD (no seek penalty), Some(false) for HDD,
/// None when the query is unsupported/fails (network drive, permission, etc.).
#[cfg(windows)]
fn volume_is_ssd(drive: &str) -> Option<bool> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
    };

    let vol = format!(r"\\.\{}:", &drive[..1]);
    let wide: Vec<u16> = OsStr::new(&vol).encode_wide().chain(std::iter::once(0)).collect();

    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0, // device query access only
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0u8; 1],
    };
    let mut penalty = DEVICE_SEEK_PENALTY_DESCRIPTOR {
        Version: 0,
        Size: 0,
        IncursSeekPenalty: 0,
    };
    let mut returned: u32 = 0;

    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &mut query as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            &mut penalty as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };

    unsafe { CloseHandle(handle) };

    if ok == 0 {
        None
    } else {
        Some(penalty.IncursSeekPenalty == 0)
    }
}

#[cfg(not(windows))]
fn volume_is_ssd(_drive: &str) -> Option<bool> {
    None
}

/// 通过 `GetSystemDirectoryW` 取系统目录，拼接 `fsutil.exe` 的绝对路径。
/// 绝不按 PATH 查找——否则 PATH 中同名的可执行文件会被优先调用（CWE-426）。
#[cfg(windows)]
fn fsutil_path() -> Option<PathBuf> {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buf = vec![0u16; 260];
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if n == 0 || n > buf.len() {
        return None;
    }
    let dir = String::from_utf16_lossy(&buf[..n]);
    Some(PathBuf::from(format!("{}\\fsutil.exe", dir)))
}

/// Fallback: `fsutil volume disktype <drive>`. Requires administrator.
#[cfg(windows)]
fn fsutil_is_ssd(drive: &str) -> bool {
    use std::os::windows::process::CommandExt;

    let Some(exe) = fsutil_path() else {
        return false;
    };

    std::process::Command::new(exe)
        .args(["volume", "disktype", drive])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(0x08000000)
        .output()
        .map(|output| {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout.contains("Solid State Drive") || stdout.contains("固态驱动器")
            } else {
                false
            }
        })
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn fsutil_is_ssd(_drive: &str) -> bool {
    false
}
