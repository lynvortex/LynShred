use rand::{rngs::OsRng, RngCore};
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;

use crate::error::ShredError;

const CHUNK_SIZE: usize = 512 * 1024;

/// Gutmann-style 35-pass pattern sequence
pub fn gutmann_patterns() -> Vec<Pattern> {
    let mut patterns: Vec<Pattern> = std::iter::repeat(Pattern::Random).take(4).collect();

    let specific = [
        0x55u8, 0xAA, 0x92, 0x49, 0x24,
        0x00, 0x11, 0x22, 0x33, 0x44,
        0x55, 0x66, 0x77, 0x88, 0x99,
        0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
        0xFF, 0x92, 0x49, 0x24, 0x6D,
        0xB6, 0xDB,
    ];

    for &byte in &specific {
        patterns.push(Pattern::Byte(byte));
    }

    patterns.extend(std::iter::repeat(Pattern::Random).take(4));

    assert_eq!(patterns.len(), 35, "Gutmann must have exactly 35 passes");
    patterns
}

#[derive(Clone, Debug, Serialize)]
pub enum Pattern {
    Zeros,
    Ones,
    Random,
    Byte(u8),
}

#[derive(Clone, Debug, Serialize)]
pub struct ShredMethod {
    pub name: &'static str,
    pub passes: usize,
    pub patterns: Vec<Pattern>,
}

pub static SHRED_METHODS: LazyLock<[ShredMethod; 3]> = LazyLock::new(|| [
    ShredMethod {
        name: "US Navy (3 passes)",
        passes: 3,
        patterns: vec![Pattern::Zeros, Pattern::Ones, Pattern::Random],
    },
    ShredMethod {
        name: "DoD 5220.22-M (7 passes)",
        passes: 7,
        patterns: vec![
            Pattern::Random, Pattern::Zeros, Pattern::Zeros,
            Pattern::Ones, Pattern::Random, Pattern::Zeros, Pattern::Random,
        ],
    },
    ShredMethod {
        name: "Gutmann (35 passes)",
        passes: 35,
        patterns: gutmann_patterns(),
    },
]);

pub type ProgressFn = dyn Fn(usize, usize, usize) + Send;

fn fill_buffer(buf: &mut [u8], pattern: &Pattern) {
    match pattern {
        Pattern::Zeros => buf.fill(0x00),
        Pattern::Ones => buf.fill(0xFF),
        Pattern::Random => OsRng.fill_bytes(buf),
        Pattern::Byte(b) => buf.fill(*b),
    }
}

/// Strip read-only, hidden, and system attributes on Windows
#[cfg(windows)]
fn strip_file_attributes(path: &Path) -> Result<(), ShredError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, SetFileAttributesW,
        FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
        INVALID_FILE_ATTRIBUTES,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        return Err(ShredError::Other(format!("无法获取文件属性: {}", path.display())));
    }

    let new_attrs = attrs & !(FILE_ATTRIBUTE_READONLY | FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM);
    if new_attrs != attrs {
        if unsafe { SetFileAttributesW(wide.as_ptr(), new_attrs) } == 0 {
            return Err(ShredError::Permission(format!("无法去除文件特殊属性: {}", path.display())));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn strip_file_attributes(_path: &Path) -> Result<(), ShredError> {
    Ok(())
}

/// Enumerate and wipe NTFS Alternate Data Streams on Windows
#[cfg(windows)]
fn wipe_alternate_data_streams(path: &Path) -> Result<(), ShredError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindFirstStreamW, FindNextStreamW, WIN32_FIND_STREAM_DATA,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

    let mut stream_data: WIN32_FIND_STREAM_DATA = unsafe { std::mem::zeroed() };
    let handle = unsafe {
        FindFirstStreamW(
            wide.as_ptr(),
            0, // FindStreamInfoStandard = 0
            &mut stream_data as *mut _ as *mut std::ffi::c_void,
            0,
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Ok(());
    }

    let mut streams: Vec<OsString> = Vec::new();

    loop {
        let name_len = stream_data.cStreamName.len();
        if name_len > 0 {
            // Find null terminator
            let effective_len = stream_data.cStreamName.iter()
                .position(|&c| c == 0)
                .unwrap_or(name_len);
            if effective_len > 0 {
                let name = OsString::from_wide(&stream_data.cStreamName[..effective_len]);
                if name != "::$DATA" {
                    streams.push(name);
                }
            }
        }

        let mut stream_data_next: WIN32_FIND_STREAM_DATA = unsafe { std::mem::zeroed() };
        if unsafe {
            FindNextStreamW(
                handle,
                &mut stream_data_next as *mut _ as *mut std::ffi::c_void,
            )
        } == 0
        {
            break;
        }
        stream_data = stream_data_next;
    }

    unsafe { FindClose(handle) };

    for stream_name in &streams {
        let mut stream_path = path.as_os_str().to_os_string();
        stream_path.push(stream_name);

        if let Ok(f) = OpenOptions::new().write(true).truncate(false).open(&stream_path) {
            if let Ok(meta) = f.metadata() {
                let size = meta.len() as usize;
                if size > 0 {
                    let mut buf = vec![0u8; std::cmp::min(CHUNK_SIZE, size)];
                    OsRng.fill_bytes(&mut buf);
                    let _ = (&f).seek(SeekFrom::Start(0));
                    let _ = (&f).write_all(&buf);
                    let _ = f.sync_all();
                }
            }
        }
    }

    Ok(())
}

#[cfg(not(windows))]
fn wipe_alternate_data_streams(_path: &Path) -> Result<(), ShredError> {
    Ok(())
}

/// Sync the parent directory to ensure the directory entry change is flushed
fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(f) = OpenOptions::new().read(true).open(parent) {
            let _ = f.sync_all();
            drop(f);
        }
    }
}

/// Rename file to a random name before deletion to break hard-link associations
fn rename_before_delete(path: &Path) -> Result<(), ShredError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let np = random_sibling_path(dir);
    if fs::rename(path, &np).is_ok() {
        fs::remove_file(&np)?;
        sync_parent_dir(&np);
        return Ok(());
    }
    fs::remove_file(path)?;
    sync_parent_dir(path);
    Ok(())
}

fn random_sibling_path(dir: &Path) -> PathBuf {
    let mut buf = [0u8; 8];
    OsRng.fill_bytes(&mut buf);
    dir.join(hex::encode(buf) + ".del")
}

/// 连续随机重命名：每次重命名都会就地覆写目录项中的文件名，
/// 降低 MFT/目录索引中残留原文件名的取证痕迹。已打开的句柄不受影响。
fn scrub_filename(path: &Path, rounds: usize) -> PathBuf {
    let mut current = path.to_path_buf();
    for _ in 0..rounds {
        let dir = current.parent().unwrap_or(Path::new(".")).to_path_buf();
        let np = random_sibling_path(&dir);
        if fs::rename(&current, &np).is_ok() {
            current = np;
        } else {
            break;
        }
    }
    current
}

/// 覆写后读回验证：确认确定性图案确实落盘
fn verify_pass(
    file: &mut std::fs::File,
    pattern: &Pattern,
    target: usize,
) -> Result<(), ShredError> {
    let expected = match pattern {
        Pattern::Zeros => 0x00,
        Pattern::Ones => 0xFF,
        Pattern::Byte(b) => *b,
        Pattern::Random => return Ok(()),
    };

    file.seek(SeekFrom::Start(0))?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut read = 0usize;
    while read < target {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Err(ShredError::Other("覆写验证失败：文件长度意外变化".into()));
        }
        if buf[..n].iter().any(|&b| b != expected) {
            return Err(ShredError::Other("覆写验证失败：数据未完全写入磁盘".into()));
        }
        read += n;
    }
    Ok(())
}

fn shred_one_file(
    path: &Path,
    patterns: &[Pattern],
    total_byte_passes: u64,
    accumulated: &mut u64,
    cancel_flag: &AtomicBool,
    progress_cb: Option<&ProgressFn>,
) -> Result<(), ShredError> {
    // Strip special attributes (read-only, hidden, system) before proceeding
    strip_file_attributes(path)?;

    // Open file first, then check metadata (TOCTOU mitigation).
    // read+write：读回验证需要用同一句柄读取已写入的数据
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                ShredError::Permission(format!("{}", path.display()))
            } else {
                ShredError::Io(e)
            }
        })?;

    let file_size = file.metadata().map_err(ShredError::Io)?.len() as usize;

    if file_size == 0 {
        drop(file);
        fs::remove_file(path)?;
        *accumulated += 0;
        return Ok(());
    }

    let total_passes = patterns.len();

    // Wipe alternate data streams (Windows NTFS)
    wipe_alternate_data_streams(path)?;

    // 连续随机重命名，冲刷目录项中的原文件名；句柄仍指向同一文件
    let current_path = scrub_filename(path, 3);

    let mut verified = false;

    for (pass_idx, pattern) in patterns.iter().enumerate() {
        if cancel_flag.load(Ordering::SeqCst) {
            return Err(ShredError::Cancelled);
        }

        // Re-check file size each pass — catch appended data
        let current_size = file.metadata().map_err(ShredError::Io)?.len() as usize;
        let write_target = current_size.max(file_size);

        let mut written = 0usize;
        while written < write_target {
            if cancel_flag.load(Ordering::SeqCst) {
                return Err(ShredError::Cancelled);
            }

            let remaining = write_target - written;
            let write_size = std::cmp::min(CHUNK_SIZE, remaining);
            let mut buf = vec![0u8; write_size];
            fill_buffer(&mut buf, pattern);

            file.seek(SeekFrom::Start(written as u64))?;
            file.write_all(&buf)?;
            written += write_size;
        }

        file.sync_all()?;

        // 首个确定性趟读回验证，确认覆写确实落盘
        if !verified && matches!(pattern, Pattern::Zeros | Pattern::Ones | Pattern::Byte(_)) {
            verify_pass(&mut file, pattern, write_target)?;
            verified = true;
        }

        *accumulated += write_target as u64;
        let pct = if total_byte_passes > 0 {
            ((*accumulated * 100) / total_byte_passes) as usize
        } else {
            100
        };

        if let Some(cb) = progress_cb {
            cb(pass_idx + 1, total_passes, pct.min(100));
        }
    }

    drop(file);

    // Rename to random name before deletion to mitigate directory-entry forensics
    rename_before_delete(&current_path)?;

    Ok(())
}

/// Shred multiple files using the specified method.
/// Progress callback receives (current_pass, total_passes, overall_percent).
/// Returns a list of successfully shredded file paths.
pub fn shred_files(
    paths: &[String],
    method_index: usize,
    cancel_flag: &AtomicBool,
    progress_cb: Option<&ProgressFn>,
) -> Result<Vec<String>, ShredError> {
    if paths.is_empty() {
        return Err(ShredError::Other("没有文件需要处理".into()));
    }

    if method_index >= SHRED_METHODS.len() {
        return Err(ShredError::Other("无效的擦除算法".into()));
    }

    let method = &SHRED_METHODS[method_index];
    let patterns = &method.patterns;

    let mut total_byte_passes = 0u64;
    let mut valid_paths: Vec<&str> = Vec::new();
    for p in paths {
        let path = Path::new(p);
        if path.is_file() {
            if let Ok(meta) = path.metadata() {
                total_byte_passes += meta.len() * patterns.len() as u64;
            }
            valid_paths.push(p.as_str());
        }
    }

    if valid_paths.is_empty() {
        return Err(ShredError::Other("未找到可处理的有效文件".into()));
    }

    if total_byte_passes == 0 {
        for p in &valid_paths {
            let _ = fs::remove_file(p);
        }
        return Ok(valid_paths.into_iter().map(String::from).collect());
    }

    let mut accumulated = 0u64;
    let mut shredded: Vec<String> = Vec::new();

    for file_path in &valid_paths {
        if cancel_flag.load(Ordering::SeqCst) {
            // Return partial results — some files were already shredded
            if shredded.is_empty() {
                return Err(ShredError::Cancelled);
            }
            return Ok(shredded);
        }

        let path = Path::new(file_path);
        match shred_one_file(path, patterns, total_byte_passes, &mut accumulated, cancel_flag, progress_cb) {
            Ok(()) => {
                shredded.push(file_path.to_string());
            }
            Err(e) => {
                // Return what was successfully shredded so far, plus the error
                if shredded.is_empty() {
                    return Err(e);
                }
                // Partial success — return shredded list; caller can check len vs valid_paths
                shredded.push(file_path.to_string());
                return Err(e);
            }
        }
    }

    Ok(shredded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gutmann_patterns_count() {
        let patterns = gutmann_patterns();
        assert_eq!(patterns.len(), 35);
    }

    #[test]
    fn test_shred_empty_file() -> Result<(), ShredError> {
        let dir = std::env::temp_dir().join("shred_test_empty");
        let _ = fs::create_dir_all(&dir);
        let file_path = dir.join("empty.txt");
        fs::write(&file_path, b"")?;

        let cancel = AtomicBool::new(false);
        let paths = vec![file_path.to_string_lossy().to_string()];
        let result = shred_files(&paths, 0, &cancel, None)?;
        assert_eq!(result.len(), 1);
        assert!(!file_path.exists());
        fs::remove_dir(&dir).ok();
        Ok(())
    }
}
