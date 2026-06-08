use std::os::windows::process::CommandExt;
use std::path::Path;

/// Check if the drive containing the given path is an SSD (Windows only).
/// Uses `fsutil volume disktype` to detect SSD drives.
/// Supports both English and Chinese locale output.
pub fn is_ssd_drive(path: &str) -> bool {
    if cfg!(not(target_os = "windows")) {
        return false;
    }

    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => return false,
        }
    };

    let drive = match path.to_str() {
        Some(s) if s.len() >= 2 => {
            let drive_letter = &s[..2];
            if drive_letter.ends_with(':') {
                drive_letter
            } else {
                return false;
            }
        }
        _ => return false,
    };

    std::process::Command::new("fsutil")
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
