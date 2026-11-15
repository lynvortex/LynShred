use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{Manager, State};

use shred_core::ssd;
use shred_core::tree::{self, FailedItem, TreeReport};
use shred_core::volume::{self, VolumeInfo};
use shred_core::wipe::{self, ProgressTick, ShredMethod, SHRED_METHODS};

// ───────────────────────── 状态模型 ─────────────────────────

/// 一个待处理目标：普通文件 / 文件夹 / 整个卷。
/// 文件夹与卷在列表中只占一行，避免一次性展开数万条记录。
#[derive(Clone, Debug, serde::Serialize)]
pub struct Target {
    pub path: String,
    /// "file" | "dir" | "volume"
    pub kind: String,
    pub files: usize,
    pub bytes: u64,
    pub note: String,
}

impl Target {
    fn key(&self) -> String {
        format!("{}|{}", self.kind, self.path.to_lowercase())
    }
    fn is_container(&self) -> bool {
        self.kind == "dir" || self.kind == "volume"
    }
}

pub struct AppState {
    pub targets: Mutex<Vec<Target>>,
    pub cancel_flag: Mutex<Option<Arc<AtomicBool>>>,
    pub shredding_in_progress: Mutex<bool>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            targets: Mutex::new(Vec::new()),
            cancel_flag: Mutex::new(Some(Arc::new(AtomicBool::new(false)))),
            shredding_in_progress: Mutex::new(false),
        }
    }
}

fn catch<R, F: FnOnce() -> Result<R, String>>(label: &str, f: F) -> Result<R, String> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(e) => {
            let msg = if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = e.downcast_ref::<&str>() {
                s.to_string()
            } else {
                format!("{:?}", e)
            };
            eprintln!("[LynShred] PANIC in {}: {}", label, msg);
            Err(format!("操作失败 ({})", label))
        }
    }
}

// ───────────────────────── 安全防护 ─────────────────────────

/// 判断 path 是否位于 dir 之下（大小写不敏感，按路径组件逐段匹配）
fn is_under(path: &Path, dir: &Path) -> bool {
    let mut pc = path.components();
    let mut dc = dir.components();
    loop {
        match (dc.next(), pc.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(d), Some(p)) => {
                if !d.as_os_str().eq_ignore_ascii_case(p.as_os_str()) {
                    return false;
                }
            }
        }
    }
}

/// 是否落在系统关键目录内。
///
/// 目录清单来自 Win32 API（`GetWindowsDirectoryW` / `SHGetFolderPathW`），
/// 不读环境变量；若清单为空（API 异常），保守地**判定为关键路径**，宁可拒绝也不误删。
#[cfg(windows)]
fn is_system_critical(path: &Path) -> bool {
    let dirs = crate::sysdir::critical_dirs();
    if dirs.is_empty() {
        return true;
    }
    dirs.iter().any(|d| is_under(path, d))
}

#[cfg(not(windows))]
fn is_system_critical(_path: &Path) -> bool {
    false
}

/// 去掉 canonicalize 附带的 `\\?\` 前缀，让路径以用户熟悉的形式展示。
///
/// 只做前缀剥离，不重新解析路径，因此不会改变目标对象。
fn tidy(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{}", rest));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    p.to_path_buf()
}

/// 保留设备名：即使带扩展名，Win32 也会把它当设备而不是文件
const RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// 拒绝会被 Win32 静默规范化的路径成分。
///
/// `\\?\` 前缀会把「尾随空格/点」「保留设备名」当作字面量处理，剥掉前缀后这些名字
/// 会被 Win32 重新解释，指向的对象随之改变。提前拒绝，避免"显示的是一个、删的是另一个"。
fn validate_path(path: &Path) -> Result<(), String> {
    for c in path.components() {
        let part = c.as_os_str().to_string_lossy();
        if part.ends_with(' ') || part.ends_with('.') {
            return Err(format!(
                "路径包含以空格或点结尾的成分（{}），在 Windows 上会被重新解释，已拒绝添加",
                part
            ));
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_lowercase();
        if RESERVED_NAMES.contains(&stem.as_str()) {
            return Err(format!(
                "路径包含 Windows 保留设备名（{}），已拒绝添加",
                part
            ));
        }
    }
    Ok(())
}

/// 绝对化 + 规范化。返回的路径用于去重、展示与前缀比较。
fn absolute(p: &str) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
    let shown = tidy(&canonical);
    validate_path(&shown)?;
    Ok(shown)
}

fn app_on_volume(root: &str) -> bool {
    std::env::current_exe()
        .ok()
        .map(|exe| is_under(&exe, Path::new(root)))
        .unwrap_or(false)
}

/// 系统卷判定：以 Windows 目录所在卷为准，不信任 `%SystemDrive%`。
/// 另外用卷根特征文件兜底，覆盖"Windows 被装到非 C 盘"或环境被伪造的情况。
fn is_system_volume(root: &str) -> bool {
    if crate::sysdir::contains_windows_payload(Path::new(root)) {
        return true;
    }
    match (crate::sysdir::drive_of(root), crate::sysdir::system_drive()) {
        (Some(a), Some(b)) => a == b,
        // 取不到系统盘信息时保守拒绝
        _ => true,
    }
}

// ───────────────────────── 目标收集 ─────────────────────────

/// 若目标已被已有容器目标覆盖，返回 true
fn covered_by_existing(list: &[Target], path: &Path) -> bool {
    list.iter()
        .filter(|t| t.is_container())
        .any(|t| is_under(path, Path::new(&t.path)))
}

/// 加入新目标；新目标若是容器，顺手移除其内部的旧目标，避免重复处理
fn push_target(list: &mut Vec<Target>, target: Target) -> bool {
    let p = PathBuf::from(&target.path);
    if list.iter().any(|t| t.key() == target.key()) {
        return false;
    }
    if covered_by_existing(list, &p) {
        return false;
    }
    if target.is_container() {
        list.retain(|t| !is_under(Path::new(&t.path), &p));
    }
    list.push(target);
    true
}

fn build_file_target(path: &Path) -> Result<Target, String> {
    // 文件同样要拒绝链接：metadata() 会跟随链接，指向的可能是别处的文件
    let link_meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if link_meta.file_type().is_symlink() {
        return Err(format!(
            "{} 是符号链接，为避免误伤链接目标，已拒绝添加",
            path.display()
        ));
    }
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    Ok(Target {
        path: path.to_string_lossy().to_string(),
        kind: "file".into(),
        files: 1,
        bytes: meta.len(),
        note: String::new(),
    })
}

fn estimate_note(est: &tree::Estimate) -> String {
    if est.truncated {
        "目录过大，文件数为抽样估算，进度百分比为近似值".into()
    } else {
        String::new()
    }
}

fn build_dir_target(path: &Path) -> Result<Target, String> {
    // 拒绝把符号链接/junction 当作文件夹：read_dir 会穿过链接，等于越界处理真实目录。
    // 注意这里必须用 symlink_metadata，且必须在 canonicalize 之前判断。
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{} 是符号链接或目录联接，为避免误伤链接目标，已拒绝添加",
            path.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!("{} 不是文件夹", path.display()));
    }
    let est = tree::estimate(
        path,
        &std::sync::atomic::AtomicBool::new(false),
        tree::ESTIMATE_MAX_ENTRIES,
    );
    Ok(Target {
        path: path.to_string_lossy().to_string(),
        kind: "dir".into(),
        files: est.files,
        bytes: est.bytes,
        note: estimate_note(&est),
    })
}

fn build_volume_target(root: &str) -> Result<Target, String> {
    let norm = volume::normalize_root(root).ok_or_else(|| format!("无效的卷路径: {}", root))?;
    if is_system_volume(&norm) {
        return Err(format!(
            "{} 是系统盘（含 Windows 目录），禁止执行全盘粉碎",
            norm
        ));
    }
    if app_on_volume(&norm) {
        return Err(format!("本程序运行在 {}，无法在运行时粉碎该卷", norm));
    }
    let info = volume::list_volumes()
        .into_iter()
        .find(|v| v.root.eq_ignore_ascii_case(&norm));
    let est = tree::estimate(
        Path::new(&norm),
        &std::sync::atomic::AtomicBool::new(false),
        tree::ESTIMATE_MAX_ENTRIES,
    );
    let mut note = match info {
        Some(v) if !v.note.is_empty() => v.note,
        _ => "整卷粉碎，保留盘符".into(),
    };
    let est_note = estimate_note(&est);
    if !est_note.is_empty() {
        note = format!("{}；{}", note, est_note);
    }
    Ok(Target {
        path: norm,
        kind: "volume".into(),
        files: est.files,
        bytes: est.bytes,
        note,
    })
}

// ───────────────────────── Tauri Commands ─────────────────────────

#[tauri::command]
pub fn get_methods() -> Result<Vec<ShredMethod>, String> {
    Ok(SHRED_METHODS.to_vec())
}

#[tauri::command]
pub fn check_ssd(paths: Vec<String>) -> Result<bool, String> {
    Ok(paths.iter().any(|p| ssd::is_ssd_drive(p)))
}

#[tauri::command]
pub fn list_volumes() -> Result<Vec<VolumeInfo>, String> {
    catch("list_volumes", || Ok(volume::list_volumes()))
}

/// 批量添加的结果：成功加入的目标 + 被拒绝/跳过项的原因。
/// 静默丢弃会让用户以为"已加入"，实际什么都没做，因此逐条回报。
#[derive(serde::Serialize)]
pub struct AddResult {
    pub added: Vec<Target>,
    pub skipped: Vec<String>,
}

#[tauri::command]
pub fn add_files(state: State<AppState>, files: Vec<String>) -> Result<AddResult, String> {
    let mut guard = state.targets.lock().map_err(|e| e.to_string())?;
    catch("add_files", || {
        let mut out = AddResult { added: Vec::new(), skipped: Vec::new() };
        for f in &files {
            let abs = match absolute(f) {
                Ok(a) => a,
                Err(e) => {
                    out.skipped.push(e);
                    continue;
                }
            };
            if is_system_critical(&abs) {
                out.skipped.push(format!("{} 位于系统关键目录，已拒绝", abs.display()));
                continue;
            }
            if !abs.is_file() {
                continue;
            }
            match build_file_target(&abs) {
                Ok(t) => {
                    if push_target(&mut guard, t.clone()) {
                        out.added.push(t);
                    } else {
                        out.skipped.push(format!("{} 已存在或已被上层目标覆盖", abs.display()));
                    }
                }
                Err(e) => out.skipped.push(e),
            }
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn add_folder(state: State<AppState>, folder: String) -> Result<AddResult, String> {
    let mut guard = state.targets.lock().map_err(|e| e.to_string())?;
    catch("add_folder", || {
        let mut out = AddResult { added: Vec::new(), skipped: Vec::new() };
        let abs = match absolute(&folder) {
            Ok(a) => a,
            Err(e) => {
                out.skipped.push(e);
                return Ok(out);
            }
        };
        if is_system_critical(&abs) {
            out.skipped
                .push(format!("{} 位于系统关键目录，已拒绝", abs.display()));
            return Ok(out);
        }
        match build_dir_target(&abs) {
            Ok(t) => {
                if push_target(&mut guard, t.clone()) {
                    out.added.push(t);
                } else {
                    out.skipped
                        .push(format!("{} 已存在或已被上层目标覆盖", abs.display()));
                }
            }
            Err(e) => out.skipped.push(e),
        }
        Ok(out)
    })
}

/// 拖放添加：自动判断路径是文件还是文件夹，批量添加到列表
#[tauri::command]
pub fn add_dropped_paths(
    state: State<AppState>,
    paths: Vec<String>,
) -> Result<AddResult, String> {
    let mut guard = state.targets.lock().map_err(|e| e.to_string())?;
    catch("add_dropped_paths", || {
        let mut out = AddResult { added: Vec::new(), skipped: Vec::new() };
        for p in &paths {
            let abs = match absolute(p) {
                Ok(a) => a,
                Err(e) => {
                    out.skipped.push(e);
                    continue;
                }
            };
            if is_system_critical(&abs) {
                out.skipped
                    .push(format!("{} 位于系统关键目录，已拒绝", abs.display()));
                continue;
            }
            let built = if abs.is_dir() {
                build_dir_target(&abs)
            } else if abs.is_file() {
                build_file_target(&abs)
            } else {
                continue;
            };
            match built {
                Ok(t) => {
                    if push_target(&mut guard, t.clone()) {
                        out.added.push(t);
                    } else {
                        out.skipped
                            .push(format!("{} 已存在或已被上层目标覆盖", abs.display()));
                    }
                }
                Err(e) => out.skipped.push(e),
            }
        }
        Ok(out)
    })
}

/// 全盘粉碎：添加一个或多个卷作为目标
#[tauri::command]
pub fn add_volumes(state: State<AppState>, roots: Vec<String>) -> Result<AddResult, String> {
    let mut guard = state.targets.lock().map_err(|e| e.to_string())?;
    catch("add_volumes", || {
        let mut out = AddResult { added: Vec::new(), skipped: Vec::new() };
        for r in &roots {
            match build_volume_target(r) {
                Ok(t) => {
                    if push_target(&mut guard, t.clone()) {
                        out.added.push(t);
                    } else {
                        out.skipped.push(format!("{} 已在列表中", r));
                    }
                }
                Err(e) => out.skipped.push(e),
            }
        }
        if out.added.is_empty() && out.skipped.is_empty() {
            out.skipped.push("没有可添加的卷".into());
        }
        Ok(out)
    })
}

#[tauri::command]
pub fn remove_selected(state: State<AppState>, indices: Vec<usize>) -> Result<(), String> {
    let mut guard = state.targets.lock().map_err(|e| e.to_string())?;
    catch("remove_selected", || {
        let mut sorted = indices;
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        sorted.dedup();
        for i in sorted {
            if i < guard.len() {
                guard.remove(i);
            }
        }
        Ok(())
    })
}

#[tauri::command]
pub fn clear_list(state: State<AppState>) -> Result<(), String> {
    let mut guard = state.targets.lock().map_err(|e| e.to_string())?;
    catch("clear_list", || {
        guard.clear();
        Ok(())
    })
}

// ───────────────────────── 执行 ─────────────────────────

#[derive(Default)]
struct Run {
    files_shredded: usize,
    bytes_shredded: u64,
    dirs_removed: usize,
    links_removed: usize,
    files_failed: Vec<FailedItem>,
    residual: Vec<String>,
    skipped: Vec<String>,
    volumes: Vec<VolumeRun>,
    cancelled: bool,
}

#[derive(serde::Serialize)]
struct VolumeRun {
    root: String,
    files_shredded: usize,
    dirs_removed: usize,
    residual: usize,
    free_space_written: u64,
    free_space_skipped: Option<String>,
}

impl Run {
    fn absorb(&mut self, r: TreeReport) {
        self.files_shredded += r.files_shredded;
        self.bytes_shredded += r.bytes_shredded;
        self.dirs_removed += r.dirs_removed;
        self.links_removed += r.links_removed;
        self.files_failed.extend(r.files_failed);
        self.residual.extend(r.residual);
        self.skipped.extend(r.skipped);
        self.cancelled |= r.cancelled;
    }

    fn summary(&self) -> String {
        let mut parts = vec![format!("已粉碎 {} 个文件", self.files_shredded)];
        parts.push(format!("删除 {} 个目录", self.dirs_removed));
        if self.links_removed > 0 {
            parts.push(format!("摘除 {} 个链接", self.links_removed));
        }
        let free: u64 = self.volumes.iter().map(|v| v.free_space_written).sum();
        if free > 0 {
            parts.push(format!("覆写空闲空间 {}", human_bytes(free)));
        }

        let mut msg = parts.join("，");
        if !self.files_failed.is_empty() {
            msg.push_str(&format!(
                "；{} 个文件因占用或权限不足未能处理",
                self.files_failed.len()
            ));
        }
        if !self.residual.is_empty() {
            msg.push_str(&format!("；仍有 {} 项残留", self.residual.len()));
        } else if self.files_failed.is_empty() {
            msg.push_str("；目标已彻底删除，无残留");
        }
        msg
    }
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", b, UNITS[0])
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

#[tauri::command]
pub fn start_shredding(
    app: tauri::AppHandle,
    state: State<AppState>,
    method_index: usize,
    wipe_free_space: bool,
) -> Result<(), String> {
    if method_index >= SHRED_METHODS.len() {
        return Err("无效的擦除算法".into());
    }

    {
        let mut in_progress = state.shredding_in_progress.lock().map_err(|e| e.to_string())?;
        if *in_progress {
            return Err("已有粉碎任务正在进行中".into());
        }
        *in_progress = true;
    }

    let targets = {
        let guard = state.targets.lock().map_err(|e| e.to_string())?;
        if guard.is_empty() {
            let mut in_progress = state.shredding_in_progress.lock().map_err(|e| e.to_string())?;
            *in_progress = false;
            return Err("请先添加要处理的目标".into());
        }
        guard.clone()
    };

    // 执行前最终防线：目标可能在"加入列表"与此刻之间被替换（TOCTOU），
    // 因此这里的判定必须重新做一遍，不能复用加入时的结论。
    for t in &targets {
        let path = Path::new(&t.path);
        let blocked = if t.kind == "volume" {
            is_system_volume(&t.path) || app_on_volume(&t.path)
        } else {
            is_system_critical(path)
        };
        if blocked {
            let mut in_progress = state.shredding_in_progress.lock().map_err(|e| e.to_string())?;
            *in_progress = false;
            return Err(format!("拒绝执行：{} 不允许被粉碎", t.path));
        }
        if t.kind == "dir" {
            match std::fs::symlink_metadata(path) {
                Ok(md) if md.file_type().is_symlink() => {
                    let mut in_progress =
                        state.shredding_in_progress.lock().map_err(|e| e.to_string())?;
                    *in_progress = false;
                    return Err(format!(
                        "拒绝执行：{} 在执行前变成了符号链接或目录联接",
                        t.path
                    ));
                }
                Ok(md) if !md.is_dir() => {
                    let mut in_progress =
                        state.shredding_in_progress.lock().map_err(|e| e.to_string())?;
                    *in_progress = false;
                    return Err(format!("拒绝执行：{} 已不是文件夹", t.path));
                }
                Err(_) => {
                    // 已不存在，交给执行阶段按"文件已消失"处理
                }
                _ => {}
            }
        }
    }

    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut guard = state.cancel_flag.lock().map_err(|e| e.to_string())?;
        *guard = Some(cancel.clone());
    }

    let cancel_clone = cancel.clone();
    let app_progress = app.clone();
    let app_finished = app.clone();
    let app_cleanup = app.clone();

    std::thread::spawn(move || {
        let emit = |app: &tauri::AppHandle, payload: serde_json::Value| {
            let _ = app.emit_all("shred-progress", payload);
        };
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            run_shredding(
                &targets,
                method_index,
                wipe_free_space,
                &|payload| emit(&app_progress, payload),
                &cancel_clone,
            )
        }))
        .unwrap_or_else(|_| {
            let mut r = Run::default();
            r.files_failed.push(FailedItem {
                path: "-".into(),
                error: "处理线程发生异常".into(),
            });
            r
        });

        let cancelled = cancel_clone.load(Ordering::SeqCst) || result.cancelled;
        let total: usize = targets.iter().map(|t| t.files).sum();

        let _ = app_finished.emit_all(
            "shred-finished",
            serde_json::json!({
                "success": result.files_failed.is_empty() && result.residual.is_empty() && !cancelled,
                "cancelled": cancelled,
                "message": if cancelled { "操作已取消".to_string() } else { result.summary() },
                "filesShredded": result.files_shredded,
                "bytesShredded": result.bytes_shredded,
                "dirsRemoved": result.dirs_removed,
                "linksRemoved": result.links_removed,
                "filesFailed": result.files_failed,
                "residual": result.residual.iter().take(50).cloned().collect::<Vec<_>>(),
                "residualCount": result.residual.len(),
                "skipped": result.skipped.iter().take(20).cloned().collect::<Vec<_>>(),
                "volumes": result.volumes,
                "total": total,
            }),
        );

        if let Some(state) = app_cleanup.try_state::<AppState>() {
            if let Ok(mut guard) = state.shredding_in_progress.lock() {
                *guard = false;
            }
        }
    });

    Ok(())
}

/// 执行一次粉碎任务。事件通过 `emit` 回调上报，便于脱离 Tauri 运行时测试。
fn run_shredding(
    targets: &[Target],
    method_index: usize,
    wipe_free_space: bool,
    emit: &(dyn Fn(serde_json::Value) + Sync),
    cancel: &AtomicBool,
) -> Run {
    let mut run = Run::default();

    emit(serde_json::json!({
        "percent": 0, "stage": "scan", "current": "正在统计目标…",
        "filesDone": 0, "filesTotal": 0
    }));

    let total_bytes: u64 = targets.iter().map(|t| t.bytes).sum();
    let total_files: usize = targets.iter().map(|t| t.files).sum();

    let has_volume = targets.iter().any(|t| t.kind == "volume");
    let do_free = wipe_free_space && has_volume;
    // 有空闲空间擦除时，主粉碎阶段占总进度 85%
    let shred_share: usize = if do_free { 85 } else { 100 };
    let free_share: usize = 100 - shred_share;
    let volume_total_bytes: u64 = targets
        .iter()
        .filter(|t| t.kind == "volume")
        .map(|t| t.bytes)
        .sum();

    let progress_fn = |tick: ProgressTick| {
        let pct = tick.percent * shred_share / 100;
        emit(serde_json::json!({
            "percent": pct.min(100),
            "stage": "shred",
            "current": tick.current,
            "filesDone": tick.files_done,
            "filesTotal": tick.files_total,
        }));
    };

    let mut session = match wipe::Session::new(
        method_index,
        total_bytes,
        total_files,
        cancel,
        Some(&progress_fn),
    ) {
        Ok(s) => s,
        Err(e) => {
            run.files_failed.push(FailedItem {
                path: "-".into(),
                error: e.to_string(),
            });
            return run;
        }
    };

    // 先处理卷，再处理独立文件夹，最后是散装文件
    let ordered: Vec<&Target> = targets
        .iter()
        .filter(|t| t.kind == "volume")
        .chain(targets.iter().filter(|t| t.kind == "dir"))
        .chain(targets.iter().filter(|t| t.kind == "file"))
        .collect();

    for t in ordered {
        if cancel.load(Ordering::SeqCst) {
            run.cancelled = true;
            break;
        }
        let path = PathBuf::from(&t.path);

        match t.kind.as_str() {
            "volume" => {
                // 整卷粉碎：保留卷根（盘符），清空其下一切内容
                let report = match tree::shred_tree(&path, false, cancel, &mut |p: &Path| {
                    session.shred(p)
                }) {
                    Ok(r) => r,
                    Err(e) => {
                        run.files_failed.push(FailedItem {
                            path: t.path.clone(),
                            error: e.to_string(),
                        });
                        continue;
                    }
                };
                let mut vr = VolumeRun {
                    root: t.path.clone(),
                    files_shredded: report.files_shredded,
                    dirs_removed: report.dirs_removed,
                    residual: report.residual.len(),
                    free_space_written: 0,
                    free_space_skipped: None,
                };
                let mut residual_here = report.residual.len();
                run.absorb(report);

                if do_free && !turn_cancelled(cancel) {
                    let free_report = volume::wipe_free_space(&path, cancel, |written| {
                        let pct = if volume_total_bytes > 0 {
                            shred_share
                                + ((written.saturating_mul(free_share as u64) / volume_total_bytes)
                                    as usize)
                                    .min(free_share)
                        } else {
                            100
                        };
                        emit(serde_json::json!({
                            "percent": pct.min(99),
                            "stage": "free-space",
                            "current": format!("正在覆写空闲空间… 已写入 {}", human_bytes(written)),
                            "filesDone": 0,
                            "filesTotal": 0,
                        }));
                    });
                    vr.free_space_written = free_report.bytes_written;
                    if let Some(reason) = &free_report.skipped_reason {
                        run.skipped
                            .push(format!("{} 空闲空间未覆写：{}", t.path, reason));
                        vr.free_space_skipped = Some(reason.clone());
                    }
                }

                // 复核：卷根下除系统保留目录外不应再有内容
                if !run.cancelled {
                    if let Ok(rd) = std::fs::read_dir(&path) {
                        for e in rd.flatten() {
                            let name = e.file_name().to_string_lossy().to_string();
                            if tree::VOLUME_RESERVED
                                .iter()
                                .any(|r| r.eq_ignore_ascii_case(&name))
                            {
                                continue;
                            }
                            residual_here += 1;
                            run.residual.push(e.path().display().to_string());
                        }
                    }
                    vr.residual = residual_here;
                }
                run.volumes.push(vr);
            }
            "dir" => {
                // 单文件夹粉碎：文件全部粉碎后自底向上删除目录，连根目录一并移除
                let report = match tree::shred_tree(&path, true, cancel, &mut |p: &Path| {
                    session.shred(p)
                }) {
                    Ok(r) => r,
                    Err(e) => {
                        run.files_failed.push(FailedItem {
                            path: t.path.clone(),
                            error: e.to_string(),
                        });
                        continue;
                    }
                };
                run.absorb(report);
                // 根目录必须不存在，否则复核实际残留并如实上报
                if !run.cancelled && path.exists() {
                    for r in tree::scan_residual(&path) {
                        run.residual.push(r);
                    }
                }
            }
            _ => match session.shred(&path) {
                Ok(bytes) => {
                    run.files_shredded += 1;
                    run.bytes_shredded += bytes;
                }
                Err(shred_core::ShredError::Cancelled) => {
                    run.cancelled = true;
                    break;
                }
                Err(e) => run.files_failed.push(FailedItem {
                    path: t.path.clone(),
                    error: e.to_string(),
                }),
            },
        }
    }

    if !run.cancelled {
        emit(serde_json::json!({
            "percent": 100, "stage": "done", "current": "完成",
            "filesDone": run.files_shredded, "filesTotal": total_files
        }));
    }

    run
}

fn turn_cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::SeqCst)
}

#[tauri::command]
pub fn cancel_shredding(state: State<AppState>) -> Result<(), String> {
    let mut guard = state.cancel_flag.lock().map_err(|e| e.to_string())?;
    if let Some(flag) = guard.as_mut() {
        flag.store(true, Ordering::SeqCst);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lynshred_cmd_{}", name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn dir_target(p: &Path) -> Target {
        let est = tree::estimate(p, &AtomicBool::new(false), tree::ESTIMATE_MAX_ENTRIES);
        Target {
            path: p.to_string_lossy().to_string(),
            kind: "dir".into(),
            files: est.files,
            bytes: est.bytes,
            note: String::new(),
        }
    }

    fn volume_target(p: &Path) -> Target {
        let est = tree::estimate(p, &AtomicBool::new(false), tree::ESTIMATE_MAX_ENTRIES);
        Target {
            path: p.to_string_lossy().to_string(),
            kind: "volume".into(),
            files: est.files,
            bytes: est.bytes,
            note: String::new(),
        }
    }

    #[test]
    fn folder_target_leaves_no_empty_directories() {
        let root = sandbox("folder");
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::create_dir_all(root.join("only_empty")).unwrap();
        fs::write(root.join("a/b/data.bin"), vec![9u8; 64 * 1024]).unwrap();
        fs::write(root.join("top.txt"), b"hi").unwrap();

        let cancel = AtomicBool::new(false);
        let run = run_shredding(&[dir_target(&root)], 0, false, &|_| {}, &cancel);

        assert_eq!(run.files_shredded, 2);
        assert!(run.files_failed.is_empty(), "{:?}", run.files_failed);
        assert!(run.residual.is_empty(), "{:?}", run.residual);
        assert!(!root.exists(), "文件夹目标必须连根删除");
    }

    #[test]
    fn volume_target_keeps_root_but_empties_it() {
        let root = sandbox("volume");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("a/x.bin"), vec![3u8; 16 * 1024]).unwrap();

        let cancel = AtomicBool::new(false);
        let run = run_shredding(&[volume_target(&root)], 0, false, &|_| {}, &cancel);

        assert!(run.residual.is_empty(), "{:?}", run.residual);
        assert!(root.is_dir(), "整卷语义下根保留");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn cancelled_run_leaves_structure_untouched() {
        let root = sandbox("cancel");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/f.bin"), vec![1u8; 32 * 1024]).unwrap();

        let cancel = AtomicBool::new(true);
        let run = run_shredding(&[dir_target(&root)], 0, false, &|_| {}, &cancel);

        assert!(run.cancelled);
        assert!(root.join("a/f.bin").exists(), "取消后文件必须原样保留");
        assert!(root.exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn progress_percent_is_monotonic_and_ends_at_100() {
        let root = sandbox("progress");
        fs::create_dir_all(root.join("d")).unwrap();
        for i in 0..5 {
            fs::write(root.join(format!("d/f{}.bin", i)), vec![2u8; 40 * 1024]).unwrap();
        }

        let seen = std::sync::Mutex::new(Vec::new());
        let cancel = AtomicBool::new(false);
        let run = run_shredding(&[dir_target(&root)], 0, false, &|v| {
            if let Some(p) = v.get("percent").and_then(|x| x.as_u64()) {
                seen.lock().unwrap().push(p);
            }
        }, &cancel);

        let seen = seen.into_inner().unwrap();
        assert!(!seen.is_empty());
        assert_eq!(*seen.last().unwrap(), 100, "最终进度必须是 100");
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "进度不能回退: {:?}", seen);
        assert_eq!(run.files_shredded, 5);
        assert!(!root.exists());
    }

    #[test]
    fn system_dirs_come_from_win32_not_environment() {
        let dirs = crate::sysdir::critical_dirs();
        assert!(!dirs.is_empty(), "必须能取得权威系统目录，而不是依赖环境变量");

        let win = crate::sysdir::windows_dir().expect("应能取得 Windows 目录");
        assert!(win.is_dir(), "Windows 目录必须真实存在: {}", win.display());
        assert!(
            dirs.iter().any(|d| d.as_os_str().eq_ignore_ascii_case(win.as_os_str())),
            "Windows 目录必须包含在关键目录清单中"
        );
        assert!(is_system_critical(&win.join("System32")), "System32 必须被判定为关键路径");
    }

    #[test]
    fn ordinary_user_path_is_not_system_critical() {
        let t = std::env::temp_dir();
        assert!(!is_system_critical(&t), "临时目录不应被判定为系统关键路径: {}", t.display());
    }

    #[test]
    fn system_volume_is_detected_from_windows_dir() {
        let drive = crate::sysdir::system_drive().expect("应能取得系统盘盘符");
        let root = format!("{}:\\", drive.to_ascii_uppercase());
        assert!(crate::sysdir::contains_windows_payload(Path::new(&root)), "卷根应能探测到 Windows 特征文件");
        assert!(is_system_volume(&root), "系统盘必须被拒绝");
    }

    #[test]
    fn rejects_paths_with_trailing_space_or_reserved_names() {
        assert!(validate_path(Path::new("C:\\data\\evil. ")).is_err(), "尾随空格应被拒绝");
        assert!(validate_path(Path::new("C:\\data\\trailing.")).is_err(), "尾随点应被拒绝");
        assert!(validate_path(Path::new("C:\\data\\con.txt")).is_err(), "保留设备名应被拒绝");
        assert!(validate_path(Path::new("C:\\data\\normal_dir")).is_ok());
    }
}
