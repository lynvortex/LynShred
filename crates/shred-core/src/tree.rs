//! 目录树结构化粉碎：保证文件粉碎后，目录结构（含空目录）也被彻底移除。
//!
//! 旧实现只把「文件」收集进清单，目录条目从未被记录，因此粉碎完成后会留下
//! 一整棵空目录树。本模块用显式栈做后序遍历，在文件全部处理完后自底向上
//! 删除每个目录，最终连根目录一并移除。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::ShredError;

/// 目录嵌套深度硬上限，防御异常结构导致的栈膨胀
pub const MAX_DEPTH: usize = 512;

/// 卷根下不参与粉碎的系统保留目录（需要 SYSTEM 权限，删除只会失败）
pub const VOLUME_RESERVED: &[&str] = &["System Volume Information", "Config.Msi", "$WinREAgent"];

/// 卷根下由系统托管的文件：属于预期跳过，不应计入「失败」，
/// 否则整卷粉碎的结果永远是「存在未处理项」，会诱导用户反复重试。
pub const VOLUME_RESERVED_FILES: &[&str] = &[
    "pagefile.sys",
    "swapfile.sys",
    "hiberfil.sys",
    "DumpStack.log",
    "DumpStack.log.tmp",
];

/// 目标目录估算结果的条目上限，避免超大目录把 IPC 调用卡死
pub const ESTIMATE_MAX_ENTRIES: usize = 200_000;
/// 估算的时间预算：超时即返回「已截断」的近似值
const ESTIMATE_BUDGET: Duration = Duration::from_millis(2000);

/// 在 Windows 上为绝对路径补上 `\\?\` 前缀，突破 MAX_PATH(260) 限制。
/// 非 Windows 或相对路径原样返回。
pub fn long_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let raw = path.as_os_str().to_string_lossy();
        if raw.starts_with(r"\\?\") || !path.is_absolute() {
            return path.to_path_buf();
        }
        if let Some(rest) = raw.strip_prefix(r"\\") {
            // UNC：\\server\share -> \\?\UNC\server\share
            return PathBuf::from(format!(r"\\?\UNC\{}", rest));
        }
        return PathBuf::from(format!(r"\\?\{}", raw));
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FailedItem {
    pub path: String,
    pub error: String,
}

/// 一次目录树粉碎的完整结果
#[derive(Clone, Debug, Default, Serialize)]
pub struct TreeReport {
    pub files_shredded: usize,
    pub bytes_shredded: u64,
    pub files_failed: Vec<FailedItem>,
    pub dirs_removed: usize,
    pub links_removed: usize,
    pub skipped: Vec<String>,
    /// 无法删除、确实残留在磁盘上的路径
    pub residual: Vec<String>,
    pub cancelled: bool,
}

/// 判断某个目录项是否属于卷根保留区（只在卷根这一层判断）
fn is_volume_reserved(name: &str) -> bool {
    VOLUME_RESERVED
        .iter()
        .any(|r| r.eq_ignore_ascii_case(name))
}

/// 删除符号链接 / junction 本身（不跟随目标，目标内容不受影响）
fn remove_link(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            // 目录型 reparse point 必须用 remove_dir 摘除
            match fs::remove_dir(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        }
    }
}

/// 删除一个（预期已空的）目录；失败时再尝试按文件删除，
/// 以覆盖「云同步占位符 / 目录型 reparse point」等特殊情况。
fn remove_dir_entry(path: &Path) -> io::Result<()> {
    let _ = crate::wipe::strip_file_attributes(path);
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(fe) if fe.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(fe) => {
                // 优先回传更有信息量的错误（目录非空 / 拒绝访问）
                if e.kind() == io::ErrorKind::DirectoryNotEmpty {
                    Err(e)
                } else {
                    Err(fe)
                }
            }
        },
    }
}

fn describe(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::DirectoryNotEmpty => "目录仍非空（其中的文件未能删除）".to_string(),
        io::ErrorKind::PermissionDenied => "拒绝访问（文件被占用或需要更高权限）".to_string(),
        _ => e.to_string(),
    }
}

enum Frame {
    /// 进入目录：粉碎其中文件、把子目录压栈
    Enter(PathBuf, usize),
    /// 离开目录：此时子目录已全部处理完，可自底向上删除
    Exit(PathBuf, bool),
}

/// 递归粉碎一棵目录树。
///
/// - `remove_root` 为 true 时连根目录一起删除（单文件夹粉碎）；
///   为 false 时保留根目录（全盘粉碎保留盘符）。
/// - `shred_file` 由调用方注入，用于实际覆写单个文件。
/// - 遍历过程中不跟随符号链接 / junction，只删除链接条目本身；
///   根目录本身若是链接则直接拒绝（`read_dir` 会穿过 junction，等于越界删除）。
pub fn shred_tree<F>(
    root: &Path,
    remove_root: bool,
    cancel: &AtomicBool,
    shred_file: &mut F,
) -> Result<TreeReport, ShredError>
where
    F: FnMut(&Path) -> Result<u64, ShredError>,
{
    let mut report = TreeReport::default();
    let root_long = long_path(root);

    // 根目录必须单独校验：子项用 file_type().is_symlink() 判断，但根走的是 read_dir，
    // 若根是 junction/symlink，读到的将是链接指向的目录，删除会越出用户所选范围。
    match fs::symlink_metadata(&root_long) {
        Ok(md) => {
            if md.file_type().is_symlink() {
                return Err(ShredError::Other(format!(
                    "{} 是符号链接或目录联接，已拒绝处理",
                    root.display()
                )));
            }
            if !md.is_dir() {
                return Err(ShredError::Other(format!("{} 不是目录", root.display())));
            }
        }
        Err(e) => {
            return Err(ShredError::Other(format!(
                "无法读取目标属性: {} ({})",
                root.display(),
                describe(&e)
            )))
        }
    }

    let mut stack: Vec<Frame> = vec![Frame::Enter(root_long, 0)];

    while let Some(frame) = stack.pop() {
        if cancel.load(Ordering::SeqCst) {
            report.cancelled = true;
            break;
        }

        match frame {
            Frame::Enter(dir, depth) => {
                if depth > MAX_DEPTH {
                    report.residual.push(format!(
                        "{} (嵌套超过 {} 层，已跳过)",
                        dir.display(),
                        MAX_DEPTH
                    ));
                    continue;
                }

                let entries = match fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(e) => {
                        // 读不到就无法清理其内部，明确记为残留
                        report
                            .residual
                            .push(format!("{} ({})", dir.display(), describe(&e)));
                        continue;
                    }
                };

                let mut subdirs: Vec<PathBuf> = Vec::new();

                for entry in entries.flatten() {
                    if cancel.load(Ordering::SeqCst) {
                        break;
                    }
                    let path = entry.path();

                    // 链接只摘除条目本身，绝不进入其目标，防止越界删除
                    if let Ok(ft) = entry.file_type() {
                        if ft.is_symlink() {
                            match remove_link(&path) {
                                Ok(()) => report.links_removed += 1,
                                Err(e) => report
                                    .residual
                                    .push(format!("{} ({})", path.display(), describe(&e))),
                            }
                            continue;
                        }
                        if ft.is_dir() {
                            if depth == 0 {
                                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                                    if is_volume_reserved(name) {
                                        report.skipped.push(path.display().to_string());
                                        continue;
                                    }
                                }
                            }
                            subdirs.push(path);
                            continue;
                        }
                        if !ft.is_file() {
                            continue;
                        }
                        // 卷根下由系统托管的文件属于预期跳过，不计为失败
                        if depth == 0 {
                            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                                if VOLUME_RESERVED_FILES
                                    .iter()
                                    .any(|f| f.eq_ignore_ascii_case(name))
                                {
                                    report
                                        .skipped
                                        .push(format!("{}（系统托管文件）", path.display()));
                                    continue;
                                }
                            }
                        }
                    }

                    match shred_file(&path) {
                        Ok(bytes) => {
                            report.files_shredded += 1;
                            report.bytes_shredded += bytes;
                        }
                        Err(ShredError::Cancelled) => {
                            report.cancelled = true;
                            break;
                        }
                        Err(e) => report.files_failed.push(FailedItem {
                            path: path.display().to_string(),
                            error: e.to_string(),
                        }),
                    }
                }

                if report.cancelled {
                    // 已取消：不再删除任何目录，避免留下「目录没了、文件还在」的怪状态
                    break;
                }

                // 后序：先压 Exit，再压子目录，LIFO 保证子目录全部处理完才轮到 Exit
                stack.push(Frame::Exit(dir, depth == 0));
                for d in subdirs.into_iter().rev() {
                    stack.push(Frame::Enter(d, depth + 1));
                }
            }
            Frame::Exit(dir, is_root) => {
                if is_root && !remove_root {
                    continue;
                }
                match remove_dir_entry(&dir) {
                    Ok(()) => report.dirs_removed += 1,
                    Err(e) => report
                        .residual
                        .push(format!("{} ({})", dir.display(), describe(&e))),
                }
            }
        }
    }

    Ok(report)
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Estimate {
    pub files: usize,
    pub bytes: u64,
    /// 触及条目/时间上限，数值为近似值
    pub truncated: bool,
}

/// 统计文件数量与总字节数，用于进度分母。不分配清单，内存恒定。
///
/// 带条目数上限与时间预算：超大目录不可能把调用方（IPC 线程）无限期卡住，
/// 超限后返回 `truncated = true` 的近似值，调用方据此降级展示。
pub fn estimate(root: &Path, cancel: &AtomicBool, max_entries: usize) -> Estimate {
    let mut out = Estimate::default();
    let started = Instant::now();
    let max_entries = max_entries.max(1);

    match fs::symlink_metadata(root) {
        Ok(m) if m.file_type().is_symlink() => {
            out.truncated = true;
            return out;
        }
        Ok(m) if m.is_file() => {
            out.files = 1;
            out.bytes = m.len();
            return out;
        }
        Ok(m) if m.is_dir() => {}
        _ => {
            out.truncated = true;
            return out;
        }
    }

    let mut stack: Vec<(PathBuf, usize)> = vec![(long_path(root), 0)];
    let mut visited = 0usize;

    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        if cancel.load(Ordering::SeqCst) {
            out.truncated = true;
            break;
        }
        if visited >= max_entries || started.elapsed() > ESTIMATE_BUDGET {
            out.truncated = true;
            break;
        }

        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited >= max_entries || started.elapsed() > ESTIMATE_BUDGET {
                out.truncated = true;
                break;
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if depth == 0 {
                    if let Some(name) = entry.file_name().to_str() {
                        if is_volume_reserved(name) {
                            continue;
                        }
                    }
                }
                stack.push((entry.path(), depth + 1));
            } else if ft.is_file() {
                if depth == 0 {
                    if let Some(name) = entry.file_name().to_str() {
                        if VOLUME_RESERVED_FILES
                            .iter()
                            .any(|f| f.eq_ignore_ascii_case(name))
                        {
                            continue;
                        }
                    }
                }
                out.files += 1;
                if let Ok(m) = entry.metadata() {
                    out.bytes += m.len();
                }
            }
        }
    }

    out
}

/// 自上而下清点实际残留（粉碎结束后复核用）
pub fn scan_residual(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let root = long_path(root);
    if fs::symlink_metadata(&root).is_err() {
        return out;
    }
    let mut stack: Vec<(PathBuf, usize)> = vec![(root, 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => {
                out.push(dir.display().to_string());
                continue;
            }
        };
        for entry in entries.flatten() {
            let p = entry.path();
            out.push(p.display().to_string());
            if out.len() > 500 {
                return out;
            }
            if let Ok(ft) = entry.file_type() {
                if ft.is_dir() && !ft.is_symlink() {
                    stack.push((p, depth + 1));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn build_tree(root: &Path) {
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::create_dir_all(root.join("a/empty")).unwrap();
        fs::create_dir_all(root.join("d/e")).unwrap();
        fs::write(root.join("a/b/c/deep.txt"), b"deep").unwrap();
        fs::write(root.join("top.txt"), b"top-level").unwrap();
        fs::write(root.join("d/e/leaf.bin"), vec![7u8; 4096]).unwrap();
    }

    #[test]
    fn shred_tree_removes_every_directory() {
        let root = std::env::temp_dir().join("lynshred_tree_test_a");
        let _ = fs::remove_dir_all(&root);
        build_tree(&root);
        assert!(root.exists());

        let cancel = AtomicBool::new(false);
        let mut sink = |p: &Path| -> Result<u64, ShredError> {
            let len = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            fs::remove_file(p)?;
            Ok(len)
        };

        let report = shred_tree(&root, true, &cancel, &mut sink).unwrap();
        assert_eq!(report.files_shredded, 3);
        assert!(report.files_failed.is_empty(), "{:?}", report.files_failed);
        assert!(report.residual.is_empty(), "{:?}", report.residual);
        assert!(!root.exists(), "根目录应被彻底删除");
    }

    #[test]
    fn shred_tree_keeps_root_when_asked() {
        let root = std::env::temp_dir().join("lynshred_tree_test_b");
        let _ = fs::remove_dir_all(&root);
        build_tree(&root);

        let cancel = AtomicBool::new(false);
        let mut sink = |p: &Path| -> Result<u64, ShredError> {
            fs::remove_file(p)?;
            Ok(0)
        };

        let report = shred_tree(&root, false, &cancel, &mut sink).unwrap();
        assert!(report.residual.is_empty(), "{:?}", report.residual);
        assert!(root.is_dir(), "根目录应保留");
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            0,
            "根目录下不应残留任何内容"
        );
        fs::remove_dir(&root).ok();
    }

    #[test]
    fn shred_tree_reports_residual_when_file_cannot_be_removed() {
        let root = std::env::temp_dir().join("lynshred_tree_test_c");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("locked")).unwrap();
        fs::write(root.join("locked/keep.txt"), b"x").unwrap();

        let cancel = AtomicBool::new(false);
        let mut sink = |_p: &Path| -> Result<u64, ShredError> {
            Err(ShredError::Other("模拟失败".into()))
        };

        let report = shred_tree(&root, true, &cancel, &mut sink).unwrap();
        assert_eq!(report.files_failed.len(), 1);
        assert!(!report.residual.is_empty(), "应报告残留目录");
        assert!(root.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn shred_tree_refuses_symlinked_root() {
        let base = std::env::temp_dir().join("lynshred_tree_test_link");
        let _ = fs::remove_dir_all(&base);
        let target = base.join("real");
        let link = base.join("link");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), b"must survive").unwrap();

        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&target, &link).is_ok();
        #[cfg(not(windows))]
        let made = std::os::unix::fs::symlink(&target, &link).is_ok();

        if !made {
            // 无创建符号链接权限（未开启开发者模式）时跳过
            fs::remove_dir_all(&base).ok();
            return;
        }

        let cancel = AtomicBool::new(false);
        let mut sink = |p: &Path| -> Result<u64, ShredError> {
            fs::remove_file(p)?;
            Ok(0)
        };

        let err = shred_tree(&link, true, &cancel, &mut sink).unwrap_err();
        assert!(err.to_string().contains("符号链接"), "应拒绝链接根: {}", err);
        assert!(target.join("keep.txt").is_file(), "链接目标必须完好");

        fs::remove_dir_all(&base).ok();
    }
}
