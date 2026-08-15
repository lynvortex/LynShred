use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{Manager, State};
use shred_core::wipe::{self, ShredMethod, SHRED_METHODS};
use shred_core::ssd;

pub struct AppState {
    pub file_paths: Mutex<Vec<String>>,
    pub cancel_flag: Mutex<Option<Arc<AtomicBool>>>,
    pub shredding_in_progress: Mutex<bool>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            file_paths: Mutex::new(Vec::new()),
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

// ── Non-locking helpers (called inside catch) ──

/// 文件列表硬上限，防止枚举超大目录时内存与 UI 不可控
const MAX_FILE_LIST: usize = 50_000;

/// Windows 系统关键目录，禁止加入粉碎列表
#[cfg(windows)]
fn system_critical_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    for var in ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramData", "ALLUSERSPROFILE"] {
        if let Ok(p) = std::env::var(var) {
            if !p.is_empty() {
                dirs.push(std::path::PathBuf::from(p));
            }
        }
    }
    dirs
}

#[cfg(not(windows))]
fn system_critical_dirs() -> Vec<std::path::PathBuf> {
    Vec::new()
}

/// 判断 path 是否位于 dir 之下（大小写不敏感的前缀比较，按路径组件逐段匹配）
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

fn is_system_critical(path: &Path) -> bool {
    system_critical_dirs().iter().any(|d| is_under(path, d))
}

fn add_files_inner(guard: &mut Vec<String>, files: &[String]) -> Result<Vec<String>, String> {
    let mut added = Vec::new();
    for f in files {
        let abs = std::fs::canonicalize(f).unwrap_or_else(|_| Path::new(f).to_path_buf());
        if is_system_critical(&abs) {
            continue;
        }
        let abs_str = abs.to_string_lossy().to_string();
        if !guard.contains(&abs_str) && abs.is_file() && guard.len() < MAX_FILE_LIST {
            guard.push(abs_str.clone());
            added.push(abs_str);
        }
    }
    Ok(added)
}

fn add_folder_inner(guard: &mut Vec<String>, folder: &str) -> Result<Vec<String>, String> {
    let mut added = Vec::new();
    let root = Path::new(folder);
    if root.is_dir() {
        walkdir_files(root, &mut |path: &Path| {
            if guard.len() >= MAX_FILE_LIST {
                return false;
            }
            let abs_str = path.to_string_lossy().to_string();
            if !guard.contains(&abs_str) && !is_system_critical(path) {
                guard.push(abs_str.clone());
                added.push(abs_str);
            }
            true
        })
        .map_err(|e| e.to_string())?;
    }
    Ok(added)
}

/// 流式遍历目录树，每发现一个文件调用一次回调；回调返回 false 时提前终止。
/// 跳过符号链接/junction，防止遍历越出所选目录；按真实嵌套深度限制 64 层。
fn walkdir_files<F>(dir: &Path, cb: &mut F) -> Result<(), String>
where
    F: FnMut(&Path) -> bool,
{
    let max_depth = 64usize;
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(dir.to_path_buf(), 0)];

    while let Some((current, depth)) = stack.pop() {
        if depth > max_depth {
            return Err("目录嵌套过深，已超过 64 层限制".into());
        }
        let entries = match std::fs::read_dir(&current) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            let ftype = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ftype.is_symlink() {
                continue;
            } else if ftype.is_dir() {
                stack.push((path, depth + 1));
            } else if ftype.is_file() {
                if !cb(&path) {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

// ── Tauri Commands ──

#[tauri::command]
pub fn get_methods() -> Result<Vec<ShredMethod>, String> {
    Ok(SHRED_METHODS.to_vec())
}

#[tauri::command]
pub fn check_ssd(paths: Vec<String>) -> Result<bool, String> {
    Ok(paths.iter().any(|p| ssd::is_ssd_drive(p)))
}

#[tauri::command]
pub fn add_files(state: State<AppState>, files: Vec<String>) -> Result<Vec<String>, String> {
    let mut guard = state.file_paths.lock().map_err(|e| e.to_string())?;
    catch("add_files", || add_files_inner(&mut guard, &files))
}

#[tauri::command]
pub fn add_folder(state: State<AppState>, folder: String) -> Result<Vec<String>, String> {
    let mut guard = state.file_paths.lock().map_err(|e| e.to_string())?;
    catch("add_folder", || add_folder_inner(&mut guard, &folder))
}

/// 拖放添加：自动判断路径是文件还是文件夹，批量添加到列表
#[tauri::command]
pub fn add_dropped_paths(
    state: State<AppState>,
    paths: Vec<String>,
) -> Result<Vec<String>, String> {
    let mut guard = state.file_paths.lock().map_err(|e| e.to_string())?;
    catch("add_dropped_paths", || {
        let mut all_added: Vec<String> = Vec::new();
        for p in &paths {
            let path = std::path::Path::new(p);
            if path.is_dir() {
                match add_folder_inner(&mut guard, p) {
                    Ok(added) => all_added.extend(added),
                    Err(e) => eprintln!("[LynShred] add_folder falló: {}", e),
                }
            } else if path.is_file() {
                match add_files_inner(&mut guard, &[p.clone()]) {
                    Ok(added) => all_added.extend(added),
                    Err(e) => eprintln!("[LynShred] add_file falló: {}", e),
                }
            }
        }
        Ok(all_added)
    })
}

#[tauri::command]
pub fn remove_selected(state: State<AppState>, indices: Vec<usize>) -> Result<(), String> {
    let mut guard = state.file_paths.lock().map_err(|e| e.to_string())?;
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
    let mut guard = state.file_paths.lock().map_err(|e| e.to_string())?;
    catch("clear_list", || {
        guard.clear();
        Ok(())
    })
}

#[tauri::command]
pub fn start_shredding(
    app: tauri::AppHandle,
    state: State<AppState>,
    method_index: usize,
) -> Result<(), String> {
    // Lock shredding guard outside catch to prevent concurrent shredding
    {
        let mut in_progress = state.shredding_in_progress.lock().map_err(|e| e.to_string())?;
        if *in_progress {
            return Err("已有粉碎任务正在进行中".into());
        }
        *in_progress = true;
    }

    let paths = {
        let guard = state.file_paths.lock().map_err(|e| e.to_string())?;
        if guard.is_empty() {
            // Reset flag
            let mut in_progress = state.shredding_in_progress.lock().map_err(|e| e.to_string())?;
            *in_progress = false;
            return Err("请先添加要处理的文件".into());
        }
        guard.clone()
    };

    // 粉碎前最终防线：拒绝系统关键目录下的任何路径
    if let Some(offender) = paths.iter().map(|p| p.as_str()).find(|p| is_system_critical(Path::new(p))) {
        let mut in_progress = state.shredding_in_progress.lock().map_err(|e| e.to_string())?;
        *in_progress = false;
        return Err(format!("列表中包含系统关键路径，已拒绝执行: {}", offender));
    }

    // Create a shared cancel flag
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut guard = state.cancel_flag.lock().map_err(|e| e.to_string())?;
        *guard = Some(cancel.clone());
    }

    let cancel_clone = cancel.clone();
    let app_handle_progress = app.clone();
    let app_handle_finished = app.clone();
    let app_handle_cleanup = app.clone();

    std::thread::spawn(move || {
        let progress_cb = move |_pass: usize, _total: usize, pct: usize| {
            let _ = app_handle_progress.emit_all("shred-progress", serde_json::json!({
                "percent": pct.min(100)
            }));
        };

        // 捕获工作线程 panic，确保 shredding_in_progress 总能被复位
        let result = match panic::catch_unwind(AssertUnwindSafe(|| {
            wipe::shred_files(&paths, method_index, &cancel_clone, Some(&progress_cb))
        })) {
            Ok(r) => r,
            Err(_) => Err(shred_core::ShredError::Other("处理线程发生异常".into())),
        };

        let was_cancelled = cancel_clone.load(Ordering::SeqCst);

        match result {
            Ok(shredded_list) => {
                let _ = app_handle_finished.emit_all("shred-finished", serde_json::json!({
                    "success": true,
                    "message": format!("成功处理 {} 个文件", shredded_list.len()),
                    "shredded": shredded_list,
                    "total": paths.len(),
                }));
            }
            Err(e) => {
                let is_cancel = matches!(&e, shred_core::ShredError::Cancelled);
                let msg = if is_cancel {
                    if was_cancelled {
                        "操作已取消".into()
                    } else {
                        e.to_string()
                    }
                } else {
                    e.to_string()
                };
                let _ = app_handle_finished.emit_all("shred-finished", serde_json::json!({
                    "success": false,
                    "message": msg,
                    "shredded": [],
                    "total": paths.len(),
                }));
            }
        }

        // Reset shredding flag
        if let Some(state) = app_handle_cleanup.try_state::<AppState>() {
            if let Ok(mut guard) = state.shredding_in_progress.lock() {
                *guard = false;
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub fn cancel_shredding(state: State<AppState>) -> Result<(), String> {
    let mut guard = state.cancel_flag.lock().map_err(|e| e.to_string())?;
    if let Some(flag) = guard.as_mut() {
        flag.store(true, Ordering::SeqCst);
    }
    Ok(())
}
