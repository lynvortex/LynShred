import sys
import os
import subprocess
from pathlib import Path
from typing import List, Union, Tuple

from PySide6.QtWidgets import (
    QApplication, QMainWindow, QWidget, QVBoxLayout, QHBoxLayout,
    QPushButton, QListWidget, QComboBox, QLabel, QProgressDialog,
    QFileDialog, QMessageBox, QStatusBar, QAbstractItemView
)
from PySide6.QtCore import Qt, QThread, Signal, QMutex, QMutexLocker, QPointF
from PySide6.QtGui import QIcon, QDragEnterEvent, QDropEvent, QPainter, QFont, QColor, QPolygonF


# ========== 图标资源处理 ==========
def get_icon() -> QIcon:
    """加载应用程序图标，如果文件不存在则返回空图标"""
    icon_path = Path(__file__).parent / "icon.ico"
    if icon_path.is_file():
        return QIcon(str(icon_path))
    if Path("icon.ico").is_file():
        return QIcon("icon.ico")
    return QIcon()


# ========== 存储介质检测 (仅限 Windows) ==========
def is_ssd_drive(path: str) -> bool:
    """
    检查指定路径所在的驱动器是否为 SSD。
    通过调用 Windows 的 fsutil 命令行工具进行无感检测。
    """
    if sys.platform != "win32":
        return False  # 非 Windows 环境默认不提示
    
    try:
        drive = os.path.splitdrive(os.path.abspath(path))[0]
        if not drive:
            return False
        
        # 执行 fsutil volume disktype X:
        result = subprocess.run(
            ["fsutil", "volume", "disktype", drive],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            creationflags=subprocess.CREATE_NO_WINDOW  # 隐藏 CMD 黑窗口
        )
        # 如果返回结果中包含 Solid State Drive，说明是固态硬盘
        if result.returncode == 0 and "Solid State Drive" in result.stdout:
            return True
    except Exception:
        pass
    return False


# ========== 覆写模式定义 ==========
def gutmann_patterns() -> List[Union[str, bytes]]:
    """返回 Gutmann 风格的 35 遍模式序列。"""
    patterns: List[Union[str, bytes]] = ["random"] * 4

    specific = [
        b'\x55', b'\xAA', b'\x92', b'\x49', b'\x24',
        b'\x00', b'\x11', b'\x22', b'\x33', b'\x44',
        b'\x55', b'\x66', b'\x77', b'\x88', b'\x99',
        b'\xAA', b'\xBB', b'\xCC', b'\xDD', b'\xEE',
        b'\xFF', b'\x92', b'\x49', b'\x24', b'\x6D',
        b'\xB6', b'\xDB'
    ]
    patterns.extend(specific)
    patterns.extend(["random"] * 4)

    assert len(patterns) == 35, f"Gutmann patterns count {len(patterns)} != 35"
    return patterns


SHRED_METHODS = {
    3: {
        "name": "US Navy (3 passes)",
        "patterns": ["zeros", "ones", "random"]
    },
    7: {
        "name": "DoD 5220.22-M (7 passes)",
        "patterns": ["random", "zeros", "zeros", "ones", "random", "zeros", "random"]
    },
    35: {
        "name": "Gutmann (35 passes)",
        "patterns": gutmann_patterns()
    }
}

DEFAULT_CHUNK_SIZE = 512 * 1024  # 512 KiB，兼顾性能和取消响应


class ShredWorker(QThread):
    """异步粉碎工作线程"""
    progress = Signal(int)         # 0-100
    status_message = Signal(str)   # 状态信息
    finished = Signal(bool, str)   # 成功/失败 + 消息

    def __init__(self, file_paths: List[str], passes_key: int, parent=None):
        super().__init__(parent)
        self.file_paths = file_paths[:]
        self.passes_key = passes_key
        self._is_cancelled = False
        self._mutex = QMutex()

    def cancel(self):
        with QMutexLocker(self._mutex):
            self._is_cancelled = True

    def is_cancelled(self) -> bool:
        with QMutexLocker(self._mutex):
            return self._is_cancelled

    def _make_repeated_buffer(self, pattern: bytes, size: int) -> bytes:
        if not pattern:
            raise ValueError("Empty pattern bytes")
        repeat_count = (size // len(pattern)) + 1
        buf = (pattern * repeat_count)[:size]
        return buf

    def _write_pattern(self, f, pattern: Union[str, bytes], file_size: int) -> int:
        written = 0
        f.seek(0)

        if isinstance(pattern, str):
            if pattern == "zeros":
                unit = b"\x00"
                chunk = unit * DEFAULT_CHUNK_SIZE
                while written < file_size:
                    if self.is_cancelled():
                        return written
                    remaining = file_size - written
                    write_size = min(DEFAULT_CHUNK_SIZE, remaining)
                    f.write(chunk[:write_size])
                    written += write_size

            elif pattern == "ones":
                unit = b"\xFF"
                chunk = unit * DEFAULT_CHUNK_SIZE
                while written < file_size:
                    if self.is_cancelled():
                        return written
                    remaining = file_size - written
                    write_size = min(DEFAULT_CHUNK_SIZE, remaining)
                    f.write(chunk[:write_size])
                    written += write_size

            elif pattern == "random":
                while written < file_size:
                    if self.is_cancelled():
                        return written
                    remaining = file_size - written
                    write_size = min(DEFAULT_CHUNK_SIZE, remaining)
                    f.write(os.urandom(write_size))
                    written += write_size
            else:
                raise ValueError(f"Unknown pattern: {pattern}")

        else:
            buf = self._make_repeated_buffer(pattern, DEFAULT_CHUNK_SIZE)
            while written < file_size:
                if self.is_cancelled():
                    return written
                remaining = file_size - written
                write_size = min(DEFAULT_CHUNK_SIZE, remaining)
                if write_size == DEFAULT_CHUNK_SIZE:
                    f.write(buf)
                else:
                    f.write(buf[:write_size])
                written += write_size

        return written

    def _shred_one_file(self, file_path: str, patterns: List[Union[str, bytes]]) -> Tuple[bool, str, int]:
        if not os.path.isfile(file_path):
            return False, f"文件不存在: {file_path}", 0

        try:
            file_size = os.path.getsize(file_path)
        except OSError as e:
            return False, f"无法读取文件大小: {file_path}: {str(e)}", 0

        if file_size == 0:
            try:
                os.remove(file_path)
                return True, f"空文件已删除: {os.path.basename(file_path)}", 0
            except Exception as e:
                return False, f"删除空文件失败 {file_path}: {str(e)}", 0

        total_written = 0

        try:
            with open(file_path, 'r+b') as f:
                for pass_idx, pattern in enumerate(patterns, start=1):
                    if self.is_cancelled():
                        return False, "操作已取消", total_written

                    self.status_message.emit(
                        f"正在粉碎: {os.path.basename(file_path)} (第 {pass_idx}/{len(patterns)} 遍)"
                    )
                    written = self._write_pattern(f, pattern, file_size)
                    total_written += written

                    if self.is_cancelled():
                        return False, "操作已取消", total_written

                    f.flush()
                    os.fsync(f.fileno())

                    if written < file_size:
                        return False, "操作已取消", total_written

            os.remove(file_path)
            return True, "", total_written

        except PermissionError:
            return False, f"权限不足，无法访问文件: {file_path}", total_written
        except OSError as e:
            return False, f"文件系统错误 {file_path}: {str(e)}", total_written
        except Exception as e:
            return False, f"未知错误 {file_path}: {str(e)}", total_written

    def run(self):
        total_files = len(self.file_paths)
        if total_files == 0:
            self.finished.emit(False, "没有文件需要处理")
            return

        method = SHRED_METHODS[self.passes_key]
        patterns = method["patterns"]
        passes = len(patterns)

        total_bytes_to_write = 0
        valid_files = []
        for f in self.file_paths:
            if os.path.isfile(f):
                try:
                    size = os.path.getsize(f)
                    total_bytes_to_write += size * passes
                    valid_files.append(f)
                except OSError:
                    pass

        if not valid_files:
            self.finished.emit(False, "未找到可处理的有效文件")
            return

        if total_bytes_to_write == 0:
            for f in valid_files:
                try:
                    os.remove(f)
                except Exception:
                    pass
            self.progress.emit(100)
            self.finished.emit(True, "所有空文件已删除")
            return

        accumulated_bytes = 0
        last_progress = -1

        for file_path in valid_files:
            if self.is_cancelled():
                self.finished.emit(False, "操作已取消")
                return

            self.status_message.emit(f"正在处理: {os.path.basename(file_path)}")
            success, err_msg, written_bytes = self._shred_one_file(file_path, patterns)

            accumulated_bytes += written_bytes
            progress = int((accumulated_bytes / total_bytes_to_write) * 100)
            if progress != last_progress:
                self.progress.emit(min(progress, 100))
                last_progress = progress

            if not success:
                self.finished.emit(False, err_msg)
                return

        self.progress.emit(100)
        self.finished.emit(True, f"成功处理 {len(valid_files)} 个文件")


# ========== 自定义列表控件（支持空白占位提示） ==========
class DropListWidget(QListWidget):
    """自定义列表框，当没有文件时在中间渲染‘拖放文件提示和图标’"""
    def __init__(self, parent=None):
        super().__init__(parent)

    def paintEvent(self, event):
        # 1. 先调用基类的原生绘制逻辑
        super().paintEvent(event)
        
        # 2. 如果列表为空，执行自定义图文渲染
        if self.count() == 0:
            painter = QPainter(self.viewport())
            try:
                painter.setRenderHint(QPainter.RenderHint.Antialiasing)

                # 获取正确的中央点坐标（已修正为通过 center() 分别获取 x, y）
                rect = self.viewport().rect()
                center_pt = rect.center()
                center_x = center_pt.x()
                center_y = center_pt.y() - 20

                # 绘制平面风格的文件图标
                painter.setPen(Qt.PenStyle.NoPen)
                painter.setBrush(QColor(160, 160, 160, 150))

                w, h = 45, 55
                x, y = center_x - w // 2, center_y - h // 2
                
                fold = 12
                points = QPolygonF([
                    QPointF(x, y),
                    QPointF(x + w - fold, y),
                    QPointF(x + w, y + fold),
                    QPointF(x + w, y + h),
                    QPointF(x, y + h)
                ])
                painter.drawPolygon(points)
                
                # 绘制折角立体细节
                painter.setBrush(QColor(130, 130, 130, 180))
                fold_points = QPolygonF([
                    QPointF(x + w - fold, y),
                    QPointF(x + w - fold, y + fold),
                    QPointF(x + w, y + fold)
                ])
                painter.drawPolygon(fold_points)

                # 绘制引导文字
                painter.setPen(QColor(140, 140, 140))
                painter.setFont(QFont("Microsoft YaHei", 11))
                
                text = "将文件拖放至此"
                text_rect = painter.fontMetrics().boundingRect(text)
                painter.drawText(center_x - text_rect.width() // 2, center_y + h // 2 + 25, text)
            finally:
                # 极其关键：显式结束绘制，防止重绘异常污染 Paint Engine 状态
                painter.end()


# ========== 主窗体界面 ==========
class ShredderWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self.file_paths: List[str] = []
        self.worker = None
        self.progress_dialog = None
        self._init_ui()

    def _init_ui(self):
        self.setWindowTitle("LynShred 1.0.0")
        self.setMinimumSize(550, 450)
        self.setWindowIcon(get_icon())

        screen = QApplication.primaryScreen().availableGeometry()
        self.resize(650, 550)
        self.move((screen.width() - 650) // 2, (screen.height() - 550) // 2)

        central = QWidget()
        layout = QVBoxLayout(central)

        # 按钮区
        btn_layout = QHBoxLayout()
        self.btn_add_files = QPushButton("添加文件")
        self.btn_add_files.clicked.connect(self.add_files)

        self.btn_add_folder = QPushButton("添加文件夹")
        self.btn_add_folder.clicked.connect(self.add_folder)

        self.btn_remove = QPushButton("移除选中")
        self.btn_remove.clicked.connect(self.remove_selected)

        self.btn_clear = QPushButton("清空列表")
        self.btn_clear.clicked.connect(self.clear_list)

        btn_layout.addWidget(self.btn_add_files)
        btn_layout.addWidget(self.btn_add_folder)
        btn_layout.addWidget(self.btn_remove)
        btn_layout.addWidget(self.btn_clear)
        layout.addLayout(btn_layout)

        # 文件列表
        self.list_widget = DropListWidget()
        self.list_widget.setSelectionMode(QAbstractItemView.SelectionMode.ExtendedSelection)
        self.list_widget.setAlternatingRowColors(True)
        layout.addWidget(self.list_widget)

        # 算法选择
        method_layout = QHBoxLayout()
        method_layout.addWidget(QLabel("擦除算法:"))

        self.method_combo = QComboBox()
        for key in (3, 7, 35):
            self.method_combo.addItem(SHRED_METHODS[key]["name"], key)
        method_layout.addWidget(self.method_combo)

        method_layout.addStretch()

        self.btn_shred = QPushButton("开始处理")
        self.btn_shred.setStyleSheet("QPushButton { color: red; font-weight: bold; }")
        self.btn_shred.clicked.connect(self.start_shredding)
        method_layout.addWidget(self.btn_shred)

        layout.addLayout(method_layout)

        self.setCentralWidget(central)

        # 状态栏常驻标签配置
        self.status = QStatusBar()
        self.setStatusBar(self.status)
        
        self.permanent_label = QLabel("©绘萤者 开源地址:https://github.com/lynvortex/lynshred  ")
        self.permanent_label.setStyleSheet("color: gray;")
        self.status.addWidget(self.permanent_label)
        
        self.setAcceptDrops(True)

    def _add_paths(self, paths: List[str]):
        added = 0
        for p in paths:
            abs_path = os.path.abspath(p)
            if abs_path not in self.file_paths and os.path.isfile(abs_path):
                self.file_paths.append(abs_path)
                self.list_widget.addItem(abs_path)
                added += 1

        if added == 0 and paths:
            QMessageBox.information(self, "提示", "所选路径中没有可用的文件，或已存在于列表中")

    def add_files(self):
        files, _ = QFileDialog.getOpenFileNames(self, "选择要处理的文件")
        if files:
            self._add_paths(files)

    def add_folder(self):
        folder = QFileDialog.getExistingDirectory(self, "选择要处理的文件夹（将递归包含所有文件）")
        if folder:
            all_files = []
            for root, _, files in os.walk(folder):
                for f in files:
                    all_files.append(os.path.join(root, f))

            if not all_files:
                QMessageBox.information(self, "提示", "所选文件夹中没有文件")
                return

            self._add_paths(all_files)

    def remove_selected(self):
        selected = self.list_widget.selectedItems()
        if not selected:
            return

        for item in selected:
            path = item.text()
            if path in self.file_paths:
                self.file_paths.remove(path)
            self.list_widget.takeItem(self.list_widget.row(item))

    def clear_list(self):
        self.file_paths.clear()
        self.list_widget.clear()

    def start_shredding(self):
        if not self.file_paths:
            QMessageBox.warning(self, "警告", "请先添加要处理的文件")
            return

        has_ssd = any(is_ssd_drive(path) for path in self.file_paths)

        if has_ssd:
            reply = QMessageBox.warning(
                self,
                "存储介质提示",
                "检测到当前列表中包含固态硬盘（SSD）上的文件！\n\n"
                "由于现代 SSD 的磨损均衡机制、日志型文件系统或快照环境的存在，"
                "普通的覆写删除可能无法彻底清除原始数据。\n"
                "针对高度敏感的数据，建议使用全盘加密或物理销毁。\n\n"
                "是否忽略此风险并继续？",
                QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
                QMessageBox.StandardButton.No
            )
            if reply != QMessageBox.StandardButton.Yes:
                return

        passes = self.method_combo.currentData()
        method_name = SHRED_METHODS[passes]["name"]

        reply = QMessageBox.warning(
            self,
            "确认操作",
            f"即将使用 {method_name} 处理 {len(self.file_paths)} 个文件。\n"
            "此操作不可逆！\n\n确定要继续吗？",
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
            QMessageBox.StandardButton.No
        )
        if reply != QMessageBox.StandardButton.Yes:
            return

        self.set_controls_enabled(False)

        self.progress_dialog = QProgressDialog("正在处理文件，请稍候...", "取消", 0, 100, self)
        self.progress_dialog.setWindowTitle("处理中")
        self.progress_dialog.setAutoClose(False)
        self.progress_dialog.setAutoReset(False)
        self.progress_dialog.canceled.connect(self.cancel_shredding)
        self.progress_dialog.show()

        self.worker = ShredWorker(self.file_paths, passes)
        self.worker.progress.connect(self.progress_dialog.setValue)
        self.worker.status_message.connect(self.status.showMessage)
        self.worker.finished.connect(self.on_shred_finished)
        self.worker.start()

    def cancel_shredding(self):
        if self.worker and self.worker.isRunning():
            self.worker.cancel()
            self.progress_dialog.setLabelText("正在取消... 请稍候")
            self.progress_dialog.setCancelButton(None)

    def on_shred_finished(self, success: bool, msg: str):
        if self.progress_dialog:
            self.progress_dialog.close()

        self.set_controls_enabled(True)

        if success:
            QMessageBox.information(self, "完成", msg)
            self.file_paths.clear()
            self.list_widget.clear()
        else:
            QMessageBox.critical(self, "错误", msg)

        self.status.showMessage(msg, 5000)
        self.worker = None
        self.progress_dialog = None

    def set_controls_enabled(self, enabled: bool):
        self.btn_add_files.setEnabled(enabled)
        self.btn_add_folder.setEnabled(enabled)
        self.btn_remove.setEnabled(enabled)
        self.btn_clear.setEnabled(enabled)
        self.method_combo.setEnabled(enabled)
        self.btn_shred.setEnabled(enabled)

    def dragEnterEvent(self, event: QDragEnterEvent):
        if event.mimeData().hasUrls():
            event.acceptProposedAction()

    def dropEvent(self, event: QDropEvent):
        paths = []
        for url in event.mimeData().urls():
            local_path = url.toLocalFile()
            if os.path.isfile(local_path):
                paths.append(local_path)
            elif os.path.isdir(local_path):
                for root, _, files in os.walk(local_path):
                    for f in files:
                        paths.append(os.path.join(root, f))

        if paths:
            self._add_paths(paths)


if __name__ == "__main__":
    app = QApplication(sys.argv)

    app_icon = get_icon()
    if not app_icon.isNull():
        app.setWindowIcon(app_icon)

    app.setStyle("Fusion")

    win = ShredderWindow()
    win.show()

    sys.exit(app.exec())