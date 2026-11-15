//! 权威系统路径。
//!
//! 旧实现从 `SystemRoot` / `ProgramFiles` / `ProgramData` / `SystemDrive` 等
//! 环境变量推导"系统关键目录"与"系统盘"。环境变量由父进程传入，可被完全控制，
//! 一旦被篡改（清空 `SystemRoot` 即可让保护表变为空），破坏性操作的唯一静态防线
//! 就会失效且不报错。
//!
//! 本模块改用 Win32 API 获取权威路径，并在拿不到结果时**保守拒绝**（fail-closed）。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 系统盘根下用以识别"这个卷装了 Windows"的特征文件
const WINDOWS_PAYLOAD: &[&str] = &[
    "Windows/System32/ntoskrnl.exe",
    "Windows/System32/config/SAM",
    "Windows/explorer.exe",
];

#[cfg(windows)]
mod imp {
    use super::*;

    const CSIDL_PROGRAM_FILES: i32 = 0x0026;
    const CSIDL_COMMON_APPDATA: i32 = 0x0023;
    const CSIDL_PROGRAM_FILESX86: i32 = 0x002A;
    const MAX_PATH: usize = 260;

    pub fn windows_dir() -> Option<PathBuf> {
        use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

        let mut buf = vec![0u16; MAX_PATH];
        let n = unsafe { GetWindowsDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 || n >= buf.len() {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..n])))
    }

    fn known_folder(csidl: i32) -> Option<PathBuf> {
        use windows_sys::Win32::UI::Shell::SHGetFolderPathW;

        let mut buf = vec![0u16; MAX_PATH];
        let hr = unsafe {
            SHGetFolderPathW(
                std::ptr::null_mut(),
                csidl,
                std::ptr::null_mut(),
                0,
                buf.as_mut_ptr(),
            )
        };
        if hr < 0 {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
        if end == 0 {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..end])))
    }

    pub fn critical_dirs() -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        if let Some(w) = windows_dir() {
            out.push(w);
        }
        for id in [CSIDL_PROGRAM_FILES, CSIDL_PROGRAM_FILESX86, CSIDL_COMMON_APPDATA] {
            if let Some(p) = known_folder(id) {
                if !out.iter().any(|d| d.as_os_str() == p.as_os_str()) {
                    out.push(p);
                }
            }
        }
        out
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn windows_dir() -> Option<PathBuf> {
        None
    }

    pub fn critical_dirs() -> Vec<PathBuf> {
        Vec::new()
    }
}

static WINDOWS_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
static CRITICAL: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// Windows 目录（权威来源，不读环境变量）。进程生命周期内只查询一次。
pub fn windows_dir() -> Option<&'static PathBuf> {
    WINDOWS_DIR.get_or_init(imp::windows_dir).as_ref()
}

/// 系统关键目录：Windows 目录、Program Files、Program Files (x86)、ProgramData。
pub fn critical_dirs() -> &'static [PathBuf] {
    CRITICAL.get_or_init(imp::critical_dirs).as_slice()
}

/// 取路径所在卷的盘符（小写），非盘符路径返回 None
pub fn drive_of(p: &str) -> Option<char> {
    let mut it = p.chars();
    let c = it.next()?;
    if it.next() == Some(':') {
        Some(c.to_ascii_lowercase())
    } else {
        None
    }
}

/// 系统盘的盘符，取自 Windows 目录所在卷。取不到时返回 None（调用方应保守拒绝）。
pub fn system_drive() -> Option<char> {
    let w = windows_dir()?;
    drive_of(&w.to_string_lossy())
}

/// 判断卷根上是否真的装着 Windows。
///
/// 用于在环境变量不可信、或 Windows 被挂载到非 C 盘的场景下兜底：
/// 只要卷根下存在 `Windows\System32\ntoskrnl.exe` 之类的特征文件，就视为系统卷。
pub fn contains_windows_payload(root: &Path) -> bool {
    WINDOWS_PAYLOAD.iter().any(|rel| {
        root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
            .exists()
    })
}
