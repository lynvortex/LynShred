//! 卷（磁盘分区）枚举与全盘级操作支持。
//!
//! 全盘粉碎在语义上等同于「对整个卷根目录执行目录树粉碎，但保留卷根本身」，
//! 另外可选用随机数据覆写卷上空闲空间，消除此前已删除文件的数据残留。

use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rand::{rngs::OsRng, RngCore};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use serde::Serialize;

use crate::tree::long_path;

const FILL_CHUNK: usize = 4 * 1024 * 1024;
/// 填充文件前缀。文件名本身随机，此串仅用于启动时清理崩溃残留。
const FILL_PREFIX: &str = "~LynShred.";

/// 打开时禁止解析重解析点，避免写穿到链接目标
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

#[derive(Clone, Debug, Serialize)]
pub struct VolumeInfo {
    /// 卷根，例如 `D:\`
    pub root: String,
    pub label: String,
    pub fs: String,
    /// 固定磁盘 / 可移动磁盘 / 网络驱动器 / 光驱
    pub kind: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// 物理磁盘编号；查询失败时为 None
    pub disk_number: Option<u32>,
    /// 系统卷（含 %SystemRoot%）与包含本程序的卷一律禁止选择
    pub is_system: bool,
    pub contains_app: bool,
    pub selectable: bool,
    pub note: String,
}

/// 卷可粉碎性判定
fn judge(is_system: bool, contains_app: bool) -> (bool, String) {
    if is_system {
        return (false, "系统盘，禁止粉碎".into());
    }
    if contains_app {
        return (false, "本程序运行在此卷，无法在运行中粉碎".into());
    }
    (true, String::new())
}

fn app_drive() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let s = exe.to_string_lossy().to_string();
    let s = s.trim_start_matches(r"\\?\");
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        return Some(s[..2].to_ascii_uppercase());
    }
    None
}

fn same_drive(a: &str, b: &str) -> bool {
    a.len() >= 2 && b.len() >= 2 && a[..2].eq_ignore_ascii_case(&b[..2])
}

// ───────────────────────── Windows ─────────────────────────

#[cfg(windows)]
pub fn list_volumes() -> Vec<VolumeInfo> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
    };

    let mut buf = vec![0u16; 512];
    let len = unsafe { GetLogicalDriveStringsW((buf.len() - 1) as u32, buf.as_mut_ptr()) };
    if len == 0 {
        return Vec::new();
    }
    buf.truncate(len as usize);

    let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let app_drive = app_drive();

    let mut out: Vec<VolumeInfo> = Vec::new();

    // "C:\\\0D:\\\0\0" -> ["C:\\", "D:\\"]
    for chunk in buf.split(|&c| c == 0) {
        if chunk.is_empty() {
            continue;
        }
        let root = String::from_utf16_lossy(chunk);
        if !root.ends_with('\\') {
            continue;
        }
        let mut wide: Vec<u16> = root.encode_utf16().collect();
        wide.push(0);

        let dtype = unsafe { GetDriveTypeW(wide.as_ptr()) };
        // DRIVE_NO_ROOT_DIR(1) / DRIVE_UNKNOWN(0) 跳过
        if dtype <= 1 {
            continue;
        }
        let kind = match dtype {
            2 => "可移动磁盘",
            3 => "本地磁盘",
            4 => "网络驱动器",
            5 => "光驱",
            6 => "内存盘",
            _ => "未知类型",
        };
        // 光驱与网络驱动器不参与全盘粉碎
        if dtype == 5 || dtype == 4 {
            continue;
        }

        let mut label_buf = vec![0u16; 256];
        let mut fs_buf = vec![0u16; 256];
        let mut serial = 0u32;
        let mut max_comp = 0u32;
        let mut flags = 0u32;
        let ok = unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                label_buf.as_mut_ptr(),
                255,
                &mut serial,
                &mut max_comp,
                &mut flags,
                fs_buf.as_mut_ptr(),
                255,
            )
        };
        let label = if ok != 0 {
            let end = label_buf.iter().position(|&c| c == 0).unwrap_or(0);
            String::from_utf16_lossy(&label_buf[..end])
        } else {
            String::new()
        };
        let fs = if ok != 0 {
            let end = fs_buf.iter().position(|&c| c == 0).unwrap_or(0);
            String::from_utf16_lossy(&fs_buf[..end])
        } else {
            String::new()
        };

        let mut free_to_caller = 0u64;
        let mut total = 0u64;
        let mut total_free = 0u64;
        let size_ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free_to_caller,
                &mut total,
                &mut total_free,
            )
        };
        if size_ok == 0 || total == 0 {
            continue;
        }

        let is_system = same_drive(&root, &format!("{}\\", system_drive));
        let contains_app = app_drive
            .as_deref()
            .map(|d| same_drive(&root, d))
            .unwrap_or(false);

        let (selectable, note) = judge(is_system, contains_app);

        out.push(VolumeInfo {
            disk_number: query_disk_number(&root),
            root,
            label,
            fs,
            kind: kind.to_string(),
            total_bytes: total,
            free_bytes: total_free,
            is_system,
            contains_app,
            selectable,
            note,
        });
    }

    out.sort_by(|a, b| a.root.cmp(&b.root));
    out
}

#[repr(C)]
struct StorageDeviceNumber {
    device_type: u32,
    device_number: u32,
    partition_number: u32,
}

/// IOCTL_STORAGE_GET_DEVICE_NUMBER = CTL_CODE(0x2D, 0x420, METHOD_BUFFERED, FILE_ANY_ACCESS)
#[cfg(windows)]
const IOCTL_STORAGE_GET_DEVICE_NUMBER: u32 = 0x0002_D108;

/// 查询卷所在物理磁盘编号。
///
/// 需要对该卷设备持有读权限：管理员下用 `GENERIC_READ` 成功，
/// 普通权限下退化为 `FILE_READ_ATTRIBUTES`（多数环境会被拒绝）。
/// 两者都失败时返回 None，界面会自动按盘符平铺而非按物理磁盘分组。
#[cfg(windows)]
fn query_disk_number(root: &str) -> Option<u32> {
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;

    for access in [GENERIC_READ, FILE_READ_ATTRIBUTES] {
        if let Some(n) = probe_disk_number(root, access) {
            return Some(n);
        }
    }
    None
}

#[cfg(windows)]
fn probe_disk_number(root: &str, access: u32) -> Option<u32> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let letter = root.chars().next()?;
    let device = format!(r"\\.\{}:", letter);
    let wide: Vec<u16> = std::ffi::OsStr::new(&device)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
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

    let mut sdn = StorageDeviceNumber {
        device_type: 0,
        device_number: 0,
        partition_number: 0,
    };
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            std::ptr::null_mut(),
            0,
            &mut sdn as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<StorageDeviceNumber>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(handle) };

    if ok == 0 {
        None
    } else {
        Some(sdn.device_number)
    }
}

#[cfg(not(windows))]
fn query_disk_number(_root: &str) -> Option<u32> {
    None
}

// ───────────────────────── 非 Windows 兜底 ─────────────────────────

#[cfg(not(windows))]
pub fn list_volumes() -> Vec<VolumeInfo> {
    vec![VolumeInfo {
        root: "/".into(),
        label: String::new(),
        fs: String::new(),
        kind: "本地磁盘".into(),
        total_bytes: 0,
        free_bytes: 0,
        disk_number: None,
        is_system: true,
        contains_app: true,
        selectable: false,
        note: "当前平台不支持全盘粉碎".into(),
    }]
}

// ───────────────────────── 全盘执行 ─────────────────────────

#[derive(Clone, Debug, Default, Serialize)]
pub struct FreeSpaceReport {
    pub bytes_written: u64,
    /// 填充期间创建的文件数，全部已覆写并删除
    pub files: usize,
    pub skipped_reason: Option<String>,
}

/// 用随机数据填满并覆写卷上的空闲空间，消除已删除文件的数据残留。
///
/// 采用「填充文件」策略而非直接写裸设备：只可能占用文件系统判定为空闲的簇，
/// 绝不会触碰任何已用数据，因此不会损坏卷结构。
/// 每个填充块写满后立即用随机数据覆写一遍再删除。
pub fn wipe_free_space<C>(root: &Path, cancel: &AtomicBool, on_progress: C) -> FreeSpaceReport
where
    C: Fn(u64),
{
    let mut report = FreeSpaceReport::default();
    let root = long_path(root);

    // 清理上次崩溃/断电遗留的填充文件
    cleanup_stale_fills(&root);

    let path = unique_fill_path(&root);

    // create_new：文件已存在即失败，绝不复用或截断任何既有文件；
    // FILE_FLAG_OPEN_REPARSE_POINT：即便该名字被他人预置为链接，也不会跟随到链接目标。
    let mut builder = OpenOptions::new();
    builder.create_new(true).write(true);
    #[cfg(windows)]
    builder.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);

    let mut file = match builder.open(&path) {
        Ok(f) => f,
        Err(e) => {
            report.skipped_reason = Some(format!("无法在卷根创建填充文件：{}", e));
            return report;
        }
    };

    let mut total: u64 = 0;
    let mut buf = vec![0u8; FILL_CHUNK];
    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        OsRng.fill_bytes(&mut buf);
        match file.write_all(&buf) {
            Ok(()) => {
                total += buf.len() as u64;
                on_progress(total);
            }
            Err(_) => break, // 卷已写满，空闲空间覆盖完毕
        }
    }
    let _ = file.sync_all();

    // 用同一个句柄原地覆写一遍：不重新打开路径，就不存在「路径被替换为链接」的窗口
    if total > 0 && !cancel.load(Ordering::SeqCst) {
        let _ = file.seek(SeekFrom::Start(0));
        let mut done: u64 = 0;
        while done < total {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            let n = std::cmp::min(FILL_CHUNK as u64, total - done) as usize;
            OsRng.fill_bytes(&mut buf[..n]);
            if file.write_all(&buf[..n]).is_err() {
                break;
            }
            done += n as u64;
        }
        let _ = file.sync_all();
    }
    drop(file);

    let _ = fs::remove_file(&path);

    report.files = 1;
    report.bytes_written = total;
    report
}

fn unique_fill_path(root: &Path) -> PathBuf {
    let mut b = [0u8; 16];
    OsRng.fill_bytes(&mut b);
    root.join(format!("{}{}.tmp", FILL_PREFIX, hex::encode(b)))
}

/// 删除上次运行遗留的填充文件（仅限本程序前缀，且只处理普通文件）
fn cleanup_stale_fills(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(FILL_PREFIX) || !name.ends_with(".tmp") {
            continue;
        }
        if let Ok(ft) = entry.file_type() {
            if ft.is_file() && !ft.is_symlink() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// 卷根路径规范化：`D:` / `D:\` / `d:/` 均归一为 `D:\`
pub fn normalize_root(input: &str) -> Option<String> {
    let t = input.trim();
    let bytes = t.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return None;
    }
    // 只接受「盘符 + 可选分隔符」，拒绝 C:/Users 这类具体路径混入整卷目标
    let rest = &t[2..];
    if !(rest.is_empty() || rest == "/" || rest == "\\") {
        return None;
    }
    Some(format!("{}:\\", (bytes[0] as char).to_ascii_uppercase()))
}

pub fn volume_root_path(root: &str) -> PathBuf {
    PathBuf::from(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volumes_are_enumerable_and_system_volume_is_locked() {
        let vols = list_volumes();
        for v in &vols {
            println!(
                "{} | label={:?} fs={:?} kind={:?} disk={:?} total={} free={} system={} selectable={} note={:?}",
                v.root, v.label, v.fs, v.kind, v.disk_number, v.total_bytes, v.free_bytes,
                v.is_system, v.selectable, v.note
            );
        }
        assert!(!vols.is_empty(), "至少应枚举到一个卷");
        assert!(
            vols.iter().any(|v| v.is_system),
            "应识别出系统卷"
        );
        for v in vols.iter().filter(|v| v.is_system) {
            assert!(!v.selectable, "系统卷必须不可选");
            assert!(!v.note.is_empty(), "系统卷应给出原因");
        }
    }

    #[test]
    fn normalize_root_handles_common_forms() {
        const BS: char = '\\';
        assert_eq!(normalize_root("D:"), Some(format!("D:{}", BS)));
        assert_eq!(normalize_root("d:/"), Some(format!("D:{}", BS)));
        assert_eq!(normalize_root(&format!("e:{}", BS)), Some(format!("E:{}", BS)));
        assert_eq!(normalize_root("  e:  "), Some(format!("E:{}", BS)));
        assert!(normalize_root("C:/Users").is_none(), "具体路径不能当作卷根");
        assert!(normalize_root("").is_none());
        assert!(normalize_root("12").is_none());
    }

}
