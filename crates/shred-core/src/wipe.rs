use rand::{rngs::OsRng, RngCore};
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use crate::error::ShredError;

const CHUNK_SIZE: usize = 512 * 1024;

/// Windows 文件属性中的「重解析点」标志（符号链接 / 目录联接 / 云占位符等）
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
/// 打开路径时不解析重解析点，直接指向链接对象本身
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

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

/// 进度事件：整体百分比 + 文件计数 + 当前处理项
#[derive(Clone, Debug, Serialize)]
pub struct ProgressTick {
    pub percent: usize,
    pub files_done: usize,
    pub files_total: usize,
    pub current: String,
}

pub type ProgressFn<'a> = dyn Fn(ProgressTick) + Send + 'a;

fn fill_buffer(buf: &mut [u8], pattern: &Pattern) {
    match pattern {
        Pattern::Zeros => buf.fill(0x00),
        Pattern::Ones => buf.fill(0xFF),
        Pattern::Random => OsRng.fill_bytes(buf),
        Pattern::Byte(b) => buf.fill(*b),
    }
}

/// Strip read-only, hidden, and system attributes on Windows.
/// 目标已不存在时视为成功（并发删除 / 上一轮已清理）。
#[cfg(windows)]
pub fn strip_file_attributes(path: &Path) -> Result<(), ShredError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesW, SetFileAttributesW,
        FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
        INVALID_FILE_ATTRIBUTES,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        let err = io::Error::last_os_error();
        // ERROR_FILE_NOT_FOUND(2) / ERROR_PATH_NOT_FOUND(3) 说明目标已消失
        if matches!(err.raw_os_error(), Some(2) | Some(3)) {
            return Ok(());
        }
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
pub fn strip_file_attributes(_path: &Path) -> Result<(), ShredError> {
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

        if let Ok(mut f) = OpenOptions::new().write(true).truncate(false).open(&stream_path) {
            if let Ok(meta) = f.metadata() {
                let size = meta.len();
                if size > 0 {
                    // 只写流自身的长度，避免把短流扩展成整块缓冲区大小
                    let n = std::cmp::min(CHUNK_SIZE as u64, size) as usize;
                    let mut buf = vec![0u8; n];
                    OsRng.fill_bytes(&mut buf);
                    let _ = f.seek(SeekFrom::Start(0));
                    let _ = f.write_all(&buf);
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
    target: u64,
) -> Result<(), ShredError> {
    let expected = match pattern {
        Pattern::Zeros => 0x00,
        Pattern::Ones => 0xFF,
        Pattern::Byte(b) => *b,
        Pattern::Random => return Ok(()),
    };

    file.seek(SeekFrom::Start(0))?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut read: u64 = 0;
    while read < target {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Err(ShredError::Other("覆写验证失败：文件长度意外变化".into()));
        }
        if buf[..n].iter().any(|&b| b != expected) {
            return Err(ShredError::Other("覆写验证失败：数据未完全写入磁盘".into()));
        }
        read += n as u64;
    }
    Ok(())
}

/// 一次粉碎会话：持有算法、累计进度与取消标志，可对任意多个文件反复调用。
/// 目录树遍历（`tree::shred_tree`）通过闭包把每个文件喂进这里。
pub struct Session<'a> {
    patterns: &'a [Pattern],
    total_byte_passes: u64,
    accumulated: u64,
    files_total: usize,
    files_done: usize,
    cancel: &'a AtomicBool,
    progress: Option<&'a ProgressFn<'a>>,
}

impl<'a> Session<'a> {
    pub fn new(
        method_index: usize,
        total_bytes: u64,
        files_total: usize,
        cancel: &'a AtomicBool,
        progress: Option<&'a ProgressFn<'a>>,
    ) -> Result<Self, ShredError> {
        let method = SHRED_METHODS
            .get(method_index)
            .ok_or_else(|| ShredError::Other("无效的擦除算法".into()))?;
        Ok(Self {
            patterns: &method.patterns,
            total_byte_passes: total_bytes.saturating_mul(method.patterns.len() as u64),
            accumulated: 0,
            files_total,
            files_done: 0,
            cancel,
            progress,
        })
    }

    fn percent(&self) -> usize {
        if self.total_byte_passes > 0 {
            ((self.accumulated.saturating_mul(100)) / self.total_byte_passes) as usize
        } else if self.files_total > 0 {
            (self.files_done.saturating_mul(100)) / self.files_total
        } else {
            100
        }
    }

    fn tick(&self, current: &Path) {
        if let Some(cb) = self.progress {
            cb(ProgressTick {
                percent: self.percent().min(100),
                files_done: self.files_done,
                files_total: self.files_total,
                current: current.display().to_string(),
            });
        }
    }

    /// 粉碎单个文件，返回其原始大小（字节）。
    /// 文件已不存在时视为成功并返回 0。
    pub fn shred(&mut self, path: &Path) -> Result<u64, ShredError> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(ShredError::Cancelled);
        }
        let result = self.shred_inner(path);
        self.files_done += 1;
        self.tick(path);
        result
    }

    fn shred_inner(&mut self, path: &Path) -> Result<u64, ShredError> {
        // Strip special attributes (read-only, hidden, system) before proceeding
        strip_file_attributes(path)?;

        // Open file first, then check metadata (TOCTOU mitigation).
        // read+write：读回验证需要用同一句柄读取已写入的数据
        // FILE_FLAG_OPEN_REPARSE_POINT：即使路径在遍历后被替换为链接，也只打开链接本身，
        // 绝不跟随到链接目标，避免覆写落到用户所选范围之外。
        let mut builder = OpenOptions::new();
        builder.read(true).write(true).truncate(false);
        #[cfg(windows)]
        builder.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);

        let mut file = match builder.open(path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(if e.kind() == io::ErrorKind::PermissionDenied {
                    ShredError::Permission(format!("{}", path.display()))
                } else {
                    ShredError::Io(e)
                })
            }
        };

        let meta = file.metadata().map_err(ShredError::Io)?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                drop(file);
                return Err(ShredError::Other(format!(
                    "{} 是符号链接或目录联接，已拒绝跟随",
                    path.display()
                )));
            }
        }
        let file_size = meta.len();

        if file_size == 0 {
            drop(file);
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(ShredError::Io(e)),
            }
            sync_parent_dir(path);
            return Ok(0);
        }

        // Wipe alternate data streams (Windows NTFS)
        wipe_alternate_data_streams(path)?;

        // 连续随机重命名，冲刷目录项中的原文件名；句柄仍指向同一文件
        let current_path = scrub_filename(path, 3);

        let patterns = self.patterns;
        let mut verified = false;

        for pattern in patterns.iter() {
            if self.cancel.load(Ordering::SeqCst) {
                return Err(ShredError::Cancelled);
            }

            // Re-check file size each pass — catch appended data
            let current_size = file.metadata().map_err(ShredError::Io)?.len();
            let write_target = current_size.max(file_size);

            let mut written: u64 = 0;
            while written < write_target {
                if self.cancel.load(Ordering::SeqCst) {
                    return Err(ShredError::Cancelled);
                }

                let remaining = write_target - written;
                let write_size = std::cmp::min(CHUNK_SIZE as u64, remaining) as usize;
                let mut buf = vec![0u8; write_size];
                fill_buffer(&mut buf, pattern);

                file.seek(SeekFrom::Start(written))?;
                file.write_all(&buf)?;
                written += write_size as u64;
            }

            file.sync_all()?;

            // 首个确定性趟读回验证，确认覆写确实落盘
            if !verified && matches!(pattern, Pattern::Zeros | Pattern::Ones | Pattern::Byte(_)) {
                verify_pass(&mut file, pattern, write_target)?;
                verified = true;
            }

            self.accumulated += write_target;
            self.tick(path);
        }

        drop(file);

        // Rename to random name before deletion to mitigate directory-entry forensics
        let dir = current_path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let renamed = random_sibling_path(&dir);
        match fs::rename(&current_path, &renamed) {
            Ok(()) => {
                fs::remove_file(&renamed)?;
                sync_parent_dir(&renamed);
            }
            Err(_) => {
                match fs::remove_file(&current_path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(ShredError::Io(e)),
                }
                sync_parent_dir(&current_path);
            }
        }

        Ok(file_size)
    }
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
        use crate::tree::shred_tree;

        let dir = std::env::temp_dir().join("shred_test_empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        let file_path = dir.join("empty.txt");
        fs::write(&file_path, b"")?;

        let cancel = AtomicBool::new(false);
        let mut session = Session::new(0, 0, 1, &cancel, None)?;
        let report = shred_tree(&dir, true, &cancel, &mut |p: &Path| session.shred(p)).unwrap();

        assert_eq!(report.files_shredded, 1);
        assert!(report.residual.is_empty(), "{:?}", report.residual);
        assert!(!dir.exists());
        Ok(())
    }

    #[test]
    fn test_shred_regular_file_removed() -> Result<(), ShredError> {
        let dir = std::env::temp_dir().join("shred_test_regular");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        let file_path = dir.join("data.bin");
        fs::write(&file_path, vec![0x41u8; 200_000])?;

        let cancel = AtomicBool::new(false);
        let mut session = Session::new(0, 200_000, 1, &cancel, None)?;
        let bytes = session.shred(&file_path)?;

        assert_eq!(bytes, 200_000);
        assert!(!file_path.exists());
        assert_eq!(fs::read_dir(&dir)?.count(), 0, "不应留下临时残留文件");
        fs::remove_dir(&dir).ok();
        Ok(())
    }
}
