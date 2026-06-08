// LynShred 2.0 - Tauri Frontend
let invoke, tauriOpen, tauriMessage, tauriAsk;

function initTauri() {
    try {
        if (window.__TAURI__ && window.__TAURI__.tauri && window.__TAURI__.tauri.invoke) {
            invoke = window.__TAURI__.tauri.invoke;
            tauriOpen = window.__TAURI__.dialog.open;
            tauriMessage = window.__TAURI__.dialog.message;
            tauriAsk = window.__TAURI__.dialog.ask;
            return true;
        }
    } catch (e) {}

    try {
        if (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke) {
            invoke = window.__TAURI_INTERNALS__.invoke;
            tauriOpen = (opts) => invoke('plugin:dialog|open', opts || {});
            tauriMessage = (msg, opts) => invoke('plugin:dialog|message', { message: msg, ...(opts || {}) });
            tauriAsk = (msg, opts) => invoke('plugin:dialog|ask', { message: msg, ...(opts || {}) });
            return true;
        }
    } catch (e) {}

    document.body.innerHTML = '<div style="padding:40px;color:#ff6666;font-family:sans-serif">Tauri API 不可用</div>';
    return false;
}

// ───────────────── State ─────────────────
const state = {
    filePaths: [],
    selectedIndices: [],
    shredding: false,
};

// ───────────────── DOM ─────────────────
const $ = id => document.getElementById(id);
const listen = (event, handler) => {
    if (window.__TAURI__ && window.__TAURI__.event) {
        window.__TAURI__.event.listen(event, handler);
    } else if (window.__TAURI_INTERNALS__) {
        window.addEventListener(event, handler);
    }
};

function setStatus(msg) {
    $('status-bar').textContent = msg;
}

function updateUI() {
    const hasItems = state.filePaths.length > 0;
    $('empty-hint').style.display = hasItems ? 'none' : '';
    $('btn-remove').disabled = state.selectedIndices.length === 0;
    $('btn-clear').disabled = !hasItems;
    $('btn-shred').disabled = !hasItems || state.shredding;
}

// ───────────────── File List ─────────────────
function renderList() {
    const list = $('file-list');
    list.querySelectorAll('.file-item').forEach(el => el.remove());

    state.filePaths.forEach((path, idx) => {
        const div = document.createElement('div');
        div.className = 'file-item';
        if (state.selectedIndices.includes(idx)) {
            div.classList.add('selected');
        }
        div.dataset.idx = idx;
        const span = document.createElement('span');
        span.className = 'fi-name';
        span.textContent = path;
        div.appendChild(span);
        div.onclick = (e) => {
            if (!e.ctrlKey && !e.metaKey) {
                state.selectedIndices = [];
            }
            const i = parseInt(div.dataset.idx);
            const pos = state.selectedIndices.indexOf(i);
            if (pos >= 0) {
                state.selectedIndices.splice(pos, 1);
                div.classList.remove('selected');
            } else {
                state.selectedIndices.push(i);
                div.classList.add('selected');
            }
            updateUI();
        };
        list.appendChild(div);
    });
    updateUI();
}

// ───────────────── Progress Dialog ─────────────────
function showProgress() {
    $('overlay').classList.remove('hidden');
    $('progress-dialog').classList.remove('hidden');
    $('progress-bar-fill').style.width = '0%';
    $('progress-label').textContent = '正在处理文件，请稍候...';
    $('btn-cancel-progress').disabled = false;
}

function hideProgress() {
    $('overlay').classList.add('hidden');
    $('progress-dialog').classList.add('hidden');
}

// ───────────────── Commands ─────────────────
async function addFiles() {
    const files = await tauriOpen({ title: '选择要处理的文件', multiple: true });
    if (!files || files.length === 0) return;
    try {
        const added = await invoke('add_files', { files: Array.isArray(files) ? files : [files] });
        state.filePaths.push(...added);
        renderList();
        setStatus(`已添加 ${added.length} 个文件`);
    } catch (e) {
        await tauriMessage(String(e), { title: '错误', type: 'error' });
    }
}

async function addFolder() {
    const folder = await tauriOpen({ title: '选择要处理的文件夹', directory: true });
    if (!folder) return;
    try {
        const added = await invoke('add_folder', { folder });
        state.filePaths.push(...added);
        renderList();
        setStatus(`已添加 ${added.length} 个文件`);
    } catch (e) {
        await tauriMessage(String(e), { title: '错误', type: 'error' });
    }
}

async function removeSelected() {
    const indices = [...state.selectedIndices].sort((a, b) => b - a);
    try {
        await invoke('remove_selected', { indices });
        for (const i of indices) {
            state.filePaths.splice(i, 1);
        }
        state.selectedIndices = [];
        renderList();
        setStatus('已移除选中文件');
    } catch (e) {
        await tauriMessage(String(e), { title: '错误', type: 'error' });
    }
}

async function clearList() {
    try {
        await invoke('clear_list');
        state.filePaths = [];
        state.selectedIndices = [];
        renderList();
        setStatus('列表已清空');
    } catch (e) {
        await tauriMessage(String(e), { title: '错误', type: 'error' });
    }
}

async function startShredding() {
    if (state.filePaths.length === 0) return;

    const methodIdx = $('method-select').selectedIndex;

    try {
        const hasSsd = await invoke('check_ssd', { paths: state.filePaths });
        if (hasSsd) {
            const ok = await tauriAsk(
                '检测到当前列表中包含固态硬盘（SSD）上的文件！\n\n由于现代 SSD 的磨损均衡机制，普通的覆写删除可能无法彻底清除原始数据。针对高度敏感的数据，建议使用全盘加密或物理销毁。\n\n是否忽略此风险并继续？',
                { title: '存储介质提示', type: 'warning' }
            );
            if (!ok) return;
        }
    } catch (e) {}

    const methodName = $('method-select').options[$('method-select').selectedIndex].text;
    const ok = await tauriAsk(
        `即将使用 ${methodName} 处理 ${state.filePaths.length} 个文件。\n\n此操作不可逆！\n\n确定要继续吗？`,
        { title: '确认操作', type: 'warning' }
    );
    if (!ok) return;

    state.shredding = true;
    updateUI();

    showProgress();

    try {
        await invoke('start_shredding', { methodIndex: methodIdx });
    } catch (e) {
        hideProgress();
        state.shredding = false;
        updateUI();
        await tauriMessage(String(e), { title: '错误', type: 'error' });
    }
}

async function cancelShredding() {
    $('btn-cancel-progress').disabled = true;
    $('progress-label').textContent = '正在取消... 请稍候';
    try {
        await invoke('cancel_shredding');
    } catch (e) {}
}

// ───────────────── Event Listeners ─────────────────
function bindTauriEvents() {
    listen('shred-progress', (event) => {
        const data = typeof event.payload === 'object' ? event.payload : JSON.parse(event.payload);
        const pct = data.percent || 0;
        $('progress-bar-fill').style.width = pct + '%';
        $('progress-label').textContent = `处理中... ${pct}%`;
    });

    listen('shred-finished', async (event) => {
        const data = typeof event.payload === 'object' ? event.payload : JSON.parse(event.payload);
        hideProgress();
        state.shredding = false;
        updateUI();

        if (data.success) {
            await tauriMessage(data.message, { title: '完成', type: 'info' });
            state.filePaths = [];
            renderList();
        } else {
            if (data.message !== '操作已取消') {
                await tauriMessage(data.message, { title: '错误', type: 'error' });
            }
        }
        setStatus(data.message);
    });
}

// ───────────────── Drag & Drop ─────────────────
document.addEventListener('dragenter', (e) => { e.preventDefault(); });
document.addEventListener('dragleave', (e) => { e.preventDefault(); });
document.addEventListener('dragover', (e) => { e.preventDefault(); });

document.addEventListener('drop', async (e) => {
    e.preventDefault();
    const items = e.dataTransfer.items;
    if (!items || items.length === 0) return;

    const filePaths = [];
    const folderPaths = [];

    // Use FileSystemEntry API to distinguish files from folders
    for (let i = 0; i < items.length; i++) {
        const entry = items[i].webkitGetAsEntry ? items[i].webkitGetAsEntry() : null;
        if (!entry) {
            // Fallback: just get the path
            const f = items[i].getAsFile();
            if (f && f.path) filePaths.push(f.path);
            continue;
        }
        if (entry.isDirectory) {
            folderPaths.push(entry.fullPath);
        } else if (entry.isFile) {
            if (entry.fullPath) {
                // webkitGetAsEntry returns fullPath starting with /
                const f = items[i].getAsFile();
                if (f && f.path) filePaths.push(f.path);
            }
        }
    }

    if (filePaths.length === 0 && folderPaths.length === 0) return;

    let totalAdded = 0;

    try {
        if (filePaths.length > 0) {
            const added = await invoke('add_files', { files: filePaths });
            state.filePaths.push(...added);
            totalAdded += added.length;
        }
    } catch (e) {}

    try {
        if (folderPaths.length > 0) {
            // For each folder path, we need its real filesystem path, not virtual path
            // webkitGetAsEntry gives us virtual paths, so let's use the dataTransfer files instead
            const allPaths = [];
            for (const f of e.dataTransfer.files) {
                if (f.path) allPaths.push(f.path);
            }
            // Check if any are directories by trying folder add
            for (const p of allPaths) {
                try {
                    const added = await invoke('add_folder', { folder: p });
                    state.filePaths.push(...added);
                    totalAdded += added.length;
                } catch (e2) {}
            }
        }
    } catch (e) {}

    if (totalAdded > 0) {
        renderList();
        setStatus(`已添加 ${totalAdded} 个文件`);
    }
});

// ───────────────── Init ─────────────────
async function init() {
    try {
        const methods = await invoke('get_methods');
        const sel = $('method-select');
        sel.innerHTML = '';
        methods.forEach(m => {
            const opt = document.createElement('option');
            opt.textContent = m.name;
            opt.value = m.passes;
            sel.appendChild(opt);
        });
    } catch (e) {}

    $('btn-add-files').onclick = addFiles;
    $('btn-add-folder').onclick = addFolder;
    $('btn-remove').onclick = removeSelected;
    $('btn-clear').onclick = clearList;
    $('btn-shred').onclick = startShredding;
    $('btn-cancel-progress').onclick = cancelShredding;

    renderList();
    setStatus('©绘萤者 开源地址:https://github.com/lynvortex/lynshred');
}

window.addEventListener('DOMContentLoaded', () => {
    if (!initTauri()) return;
    bindTauriEvents();
    init();
});
