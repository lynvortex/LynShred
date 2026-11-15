//! 针对「粉碎文件夹后残留空目录」这一缺陷的回归测试。
//!
//! 旧实现只把文件收进清单，目录条目从未被处理，粉碎完成后会留下一整棵空目录树。
//! 这里用真实磁盘上的目录树验证：粉碎后根目录及其所有子目录都必须消失，
//! 且不波及目标之外的同级目录。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use shred_core::tree::{self, shred_tree};
use shred_core::wipe::Session;

fn sandbox(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("lynshred_it_{}", name));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

/// 构造一棵「看起来像真实用户目录」的树：多层嵌套、纯空目录、深层文件
fn build_realistic_tree(root: &Path) {
    // 多层嵌套 + 每个层级都放文件
    fs::create_dir_all(root.join("work/src/deep/nested")).unwrap();
    fs::write(root.join("work/src/main.rs"), b"fn main() {}").unwrap();
    fs::write(root.join("work/src/deep/lib.rs"), vec![0x33u8; 8 * 1024]).unwrap();
    fs::write(root.join("work/src/deep/nested/tail.bin"), vec![0x44u8; 30 * 1024]).unwrap();

    // 纯空目录（旧实现残留的典型来源）
    fs::create_dir_all(root.join("cache/empty_a/empty_b")).unwrap();
    fs::create_dir_all(root.join("logs")).unwrap();

    // 只有空文件的目录
    fs::create_dir_all(root.join("tmp")).unwrap();
    fs::write(root.join("tmp/zero.txt"), b"").unwrap();

    // 根层级散装文件
    fs::write(root.join("readme.md"), b"hello").unwrap();
    fs::write(root.join(".hidden"), b"hidden").unwrap();
}

#[test]
fn shredding_a_folder_leaves_no_empty_directories_behind() {
    let root = sandbox("folder");
    build_realistic_tree(&root);

    // 目标之外的同级目录，必须完好无损
    let neighbour = root.parent().unwrap().join("lynshred_it_neighbour");
    let _ = fs::remove_dir_all(&neighbour);
    fs::create_dir_all(neighbour.join("keep")).unwrap();
    fs::write(neighbour.join("keep/important.txt"), b"do not touch").unwrap();

    assert!(root.join("cache/empty_a/empty_b").is_dir());

    let cancel = AtomicBool::new(false);
    let mut session = Session::new(0, 0, 0, &cancel, None).unwrap();
    let report = shred_tree(&root, true, &cancel, &mut |p: &Path| session.shred(p)).unwrap();

    assert!(report.files_failed.is_empty(), "不应有失败项: {:?}", report.files_failed);
    assert!(report.residual.is_empty(), "不应有残留: {:?}", report.residual);
    assert_eq!(report.files_shredded, 6, "6 个文件都应被粉碎");
    assert!(!root.exists(), "目标文件夹必须被彻底删除，不留空目录");
    assert_eq!(tree::scan_residual(&root).len(), 0, "复扫应为空");

    // 邻居未被波及
    assert!(neighbour.join("keep/important.txt").is_file(), "目标之外的目录不能受影响");

    fs::remove_dir_all(&neighbour).ok();
}

#[test]
fn empty_directory_only_tree_is_fully_removed() {
    let root = sandbox("empty_tree");
    fs::create_dir_all(root.join("a/b/c/d")).unwrap();
    fs::create_dir_all(root.join("e")).unwrap();

    let cancel = AtomicBool::new(false);
    let mut session = Session::new(0, 0, 0, &cancel, None).unwrap();
    let report = shred_tree(&root, true, &cancel, &mut |p: &Path| session.shred(p)).unwrap();

    assert_eq!(report.files_shredded, 0);
    assert!(report.residual.is_empty(), "{:?}", report.residual);
    assert!(!root.exists(), "纯空目录树也必须被完整移除");
}

#[test]
fn volume_mode_keeps_root_but_clears_everything_inside() {
    let root = sandbox("volume");
    build_realistic_tree(&root);

    let cancel = AtomicBool::new(false);
    let mut session = Session::new(0, 0, 0, &cancel, None).unwrap();
    let report = shred_tree(&root, false, &cancel, &mut |p: &Path| session.shred(p)).unwrap();

    assert!(report.residual.is_empty(), "{:?}", report.residual);
    assert!(root.is_dir(), "整卷语义下卷根必须保留");
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        0,
        "卷根下不应残留任何文件或目录"
    );

    fs::remove_dir_all(&root).ok();
}

#[test]
fn deep_path_beyond_max_path_is_handled() {
    let root = sandbox("longpath");
    // 逐段创建，总长度超过 260 字符的传统上限
    let mut deep = root.clone();
    for i in 0..20 {
        deep = deep.join(format!("directory_segment_{:02}_padding", i));
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("buried.txt"), vec![0x5Au8; 4096]).unwrap();

    assert!(
        deep.to_string_lossy().len() > 260,
        "测试前提：路径应超过 MAX_PATH，实际 {}",
        deep.to_string_lossy().len()
    );

    let cancel = AtomicBool::new(false);
    let mut session = Session::new(0, 0, 0, &cancel, None).unwrap();
    let report = shred_tree(&root, true, &cancel, &mut |p: &Path| session.shred(p)).unwrap();

    assert_eq!(report.files_shredded, 1);
    assert!(report.residual.is_empty(), "超长路径也应彻底清理: {:?}", report.residual);
    assert!(!root.exists());
}

#[test]
fn estimate_matches_actual_file_count() {
    let root = sandbox("estimate");
    build_realistic_tree(&root);

    let est = tree::estimate(&root, &AtomicBool::new(false), tree::ESTIMATE_MAX_ENTRIES);
    assert!(!est.truncated, "小目录不应触发截断");
    assert_eq!(est.files, 6);
    assert!(est.bytes > 38 * 1024, "字节数应包含所有文件内容");

    fs::remove_dir_all(&root).ok();
}
