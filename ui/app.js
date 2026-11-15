// LynShred 2.3.0 - Tauri Frontend
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
    targets: [],          // { path, kind, files, bytes, note }
    selectedIndices: [],
    shredding: false,
    wipeFreeSpace: true,
};

const volumeState = { list: [], selected: new Set() };

// ───────────────── DOM ─────────────────
const $ = (id) => document.getElementById(id);
const listen = (event, handler) => {
    if (window.__TAURI__ && window.__TAURI__.event) {
        window.__TAURI__.event.listen(event, handler);
    } else if (window.__TAURI_INTERNALS__) {
        window.addEventListener(event, handler);
    }
};

// ───────────────── 格式化 ─────────────────
function fmtBytes(b) {
    if (!b) return '0 B';
    const u = ['B', 'KB', 'MB', 'GB', 'TB'];
    let v = b, i = 0;
    while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
    return i === 0 ? `${b} B` : `${v.toFixed(2)} ${u[i]}`;
}
function fmtNum(n) { return (n || 0).toLocaleString('zh-CN'); }
function kindLabel(k) { return k === 'volume' ? '整卷' : k === 'dir' ? '文件夹' : '文件'; }

function setStatus(msg) { $('status-bar').textContent = msg; }

/**
 * 把路径中的账户名替换掉，避免结果截图 / 日志泄露本机用户名。
 * 只替换用户名那一段，其余保持精确，不影响定位。
 */
function maskPath(p) {
    if (!p) return '';
    const lower = p.toLowerCase();
    const i = lower.indexOf('\\users\\');
    if (i >= 0) {
        const head = p.slice(0, i + 7);
        const rest = p.slice(i + 7);
        const j = rest.indexOf('\\');
        return head + '<用户>' + (j >= 0 ? rest.slice(j) : '');
    }
    if (lower.startsWith('/home/')) {
        const rest = p.slice(6);
        const j = rest.indexOf('/');
        return '/home/<用户>' + (j >= 0 ? rest.slice(j) : '');
    }
    return p;
}

/** 统一处理 add_* 命令的返回：{ added, skipped } */
function applyAddResult(res, label) {
    const added = (res && res.added) || [];
    const skipped = (res && res.skipped) || [];

    if (added.length > 0) {
        state.targets.push(...added);
        renderList();
    }

    if (skipped.length > 0) {
        setStatus(`已加入 ${added.length} 项，拒绝 ${skipped.length} 项`);
        const lines = skipped.slice(0, 10).map((x) => '· ' + maskPath(String(x)));
        if (skipped.length > 10) lines.push(`… 另有 ${skipped.length - 10} 项`);
        tauriMessage(
            `以下项目未被加入（共 ${skipped.length} 项）：\n\n` + lines.join('\n'),
            { title: '部分项目未加入', type: 'warning' }
        );
    } else if (added.length > 0) {
        setStatus(`${label}：已加入 ${added.length} 项`);
    } else {
        setStatus(`${label}：未加入任何项目`);
    }
}

// ───────────────── 模态框管理 ─────────────────
const MODALS = ['progress-dialog', 'volume-dialog', 'report-dialog'];

function refreshOverlay() {
    const any = MODALS.some((id) => !$(id).classList.contains('hidden'));
    $('overlay').classList.toggle('hidden', !any);
}
function openModal(id) { $(id).classList.remove('hidden'); refreshOverlay(); }
function closeModal(id) { $(id).classList.add('hidden'); refreshOverlay(); }

// ───────────────── 目标列表 ─────────────────
function updateUI() {
    const hasItems = state.targets.length > 0;
    $('empty-hint').style.display = hasItems ? 'none' : '';
    $('btn-remove').disabled = state.selectedIndices.length === 0 || state.shredding;
    $('btn-clear').disabled = !hasItems || state.shredding;
    $('btn-shred').disabled = !hasItems || state.shredding;
    $('btn-add-files').disabled = state.shredding;
    $('btn-add-folder').disabled = state.shredding;
    $('btn-add-volume').disabled = state.shredding;
}

function renderList() {
    const list = $('file-list');
    list.querySelectorAll('.file-item').forEach((el) => el.remove());

    state.targets.forEach((t, idx) => {
        const div = document.createElement('div');
        div.className = 'file-item';
        if (state.selectedIndices.includes(idx)) div.classList.add('selected');
        div.dataset.idx = idx;

        const tag = document.createElement('span');
        tag.className = 'fi-tag ' + t.kind;
        tag.textContent = kindLabel(t.kind);

        const name = document.createElement('span');
        name.className = 'fi-name';
        name.textContent = maskPath(t.path);
        name.title = t.note ? `${maskPath(t.path)}\n${t.note}` : maskPath(t.path);

        const meta = document.createElement('span');
        meta.className = 'fi-meta';
        meta.textContent = `${fmtNum(t.files)} 个文件 · ${fmtBytes(t.bytes)}`;

        div.appendChild(tag);
        div.appendChild(name);
        div.appendChild(meta);

        div.onclick = (e) => {
            if (!e.ctrlKey && !e.metaKey) state.selectedIndices = [];
            const i = parseInt(div.dataset.idx, 10);
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
    syncTotals();
}

function syncTotals() {
    const files = state.targets.reduce((a, t) => a + (t.files || 0), 0);
    const bytes = state.targets.reduce((a, t) => a + (t.bytes || 0), 0);
    const vols = state.targets.filter((t) => t.kind === 'volume').length;
    if (state.targets.length === 0) return;
    const head = vols > 0
        ? `${state.targets.length} 个目标（含 ${vols} 个整卷）`
        : `${state.targets.length} 个目标`;
    setStatus(`${head}，共 ${fmtNum(files)} 个文件 · ${fmtBytes(bytes)}`);
}

// ───────────────── 添加目标 ─────────────────
async function addFiles() {
    const files = await tauriOpen({ title: '选择要粉碎的文件', multiple: true });
    if (!files || files.length === 0) return;
    try {
        const res = await invoke('add_files', { files: Array.isArray(files) ? files : [files] });
        applyAddResult(res, '文件');
    } catch (e) {
        await tauriMessage(String(e), { title: '错误', type: 'error' });
    }
}

async function addFolder() {
    const folder = await tauriOpen({ title: '选择要粉碎的文件夹', directory: true });
    if (!folder) return;
    try {
        const res = await invoke('add_folder', { folder });
        applyAddResult(res, '文件夹');
    } catch (e) {
        await tauriMessage(String(e), { title: '错误', type: 'error' });
    }
}

async function removeSelected() {
    const indices = [...state.selectedIndices].sort((a, b) => b - a);
    try {
        await invoke('remove_selected', { indices });
        for (const i of indices) state.targets.splice(i, 1);
        state.selectedIndices = [];
        renderList();
        setStatus('已移除选中目标');
    } catch (e) {
        await tauriMessage(String(e), { title: '错误', type: 'error' });
    }
}

async function clearList() {
    try {
        await invoke('clear_list');
        state.targets = [];
        state.selectedIndices = [];
        renderList();
        $('status-bar').textContent = '©绘萤者 开源地址:https://github.com/lynvortex/lynshred';
    } catch (e) {
        await tauriMessage(String(e), { title: '错误', type: 'error' });
    }
}

// ───────────────── 全盘粉碎面板 ─────────────────
async function openVolumeDialog() {
    let vols = [];
    try {
        vols = await invoke('list_volumes');
    } catch (e) {
        await tauriMessage(String(e), { title: '错误', type: 'error' });
        return;
    }
    volumeState.list = vols;
    volumeState.selected = new Set();
    renderVolumes();
    openModal('volume-dialog');
}

function renderVolumes() {
    const box = $('volume-list');
    box.innerHTML = '';

    if (volumeState.list.length === 0) {
        const p = document.createElement('div');
        p.className = 'volume-empty';
        p.textContent = '未检测到可粉碎的卷。';
        box.appendChild(p);
        updateVolumeOk();
        return;
    }

    const groups = new Map();
    volumeState.list.forEach((v) => {
        const k = v.disk_number === null || v.disk_number === undefined ? 'unknown' : String(v.disk_number);
        if (!groups.has(k)) groups.set(k, []);
        groups.get(k).push(v);
    });

    // 普通权限下读不到卷的物理磁盘归属，此时按盘符平铺，不显示虚假的分组头
    const flat = groups.size === 1 && groups.has('unknown');

    groups.forEach((vols, disk) => {
        const selectable = vols.filter((v) => v.selectable);

        if (!flat) {
            const head = document.createElement('div');
            head.className = 'volume-group-head';

            const allOn = selectable.length > 0 && selectable.every((v) => volumeState.selected.has(v.root));

            const cb = document.createElement('input');
            cb.type = 'checkbox';
            cb.checked = allOn;
            cb.disabled = selectable.length === 0;
            cb.onchange = () => {
                selectable.forEach((v) => {
                    if (cb.checked) volumeState.selected.add(v.root);
                    else volumeState.selected.delete(v.root);
                });
                renderVolumes();
            };

            const label = document.createElement('span');
            label.textContent = `物理磁盘 ${disk}`;

            head.appendChild(cb);
            head.appendChild(label);
            box.appendChild(head);
        }

        vols.forEach((v) => {
            const row = document.createElement('label');
            row.className = 'volume-row' + (v.selectable ? '' : ' disabled');

            const c = document.createElement('input');
            c.type = 'checkbox';
            c.disabled = !v.selectable;
            c.checked = volumeState.selected.has(v.root);
            c.onchange = () => {
                if (c.checked) volumeState.selected.add(v.root);
                else volumeState.selected.delete(v.root);
                renderVolumes();
            };

            const root = document.createElement('span');
            root.className = 'volume-root';
            root.textContent = v.root;

            const name = document.createElement('span');
            name.className = 'volume-name';
            name.textContent = v.label || '（无卷标）';

            const info = document.createElement('span');
            info.className = 'volume-info';
            info.textContent = `${v.fs || '未知'} · ${v.kind} · ${fmtBytes(v.total_bytes)}，可用 ${fmtBytes(v.free_bytes)}`;

            const note = document.createElement('span');
            note.className = 'volume-note';
            note.textContent = v.note || '';

            row.appendChild(c);
            row.appendChild(root);
            row.appendChild(name);
            row.appendChild(info);
            row.appendChild(note);
            box.appendChild(row);
        });
    });

    updateVolumeOk();
}

function updateVolumeOk() {
    const n = volumeState.selected.size;
    const btn = $('btn-volume-ok');
    btn.disabled = n === 0;
    btn.textContent = n > 0 ? `加入列表（${n}）` : '加入列表';
}

async function confirmVolumes() {
    const roots = [...volumeState.selected];
    if (roots.length === 0) return;
    state.wipeFreeSpace = $('opt-free-space').checked;
    try {
        const res = await invoke('add_volumes', { roots });
        const added = (res && res.added) || [];
        if (added.length > 0) {
            state.targets.push(...added);
            renderList();
            closeModal('volume-dialog');
        }
        applyAddResult(res, '整卷');
    } catch (e) {
        await tauriMessage(String(e), { title: '错误', type: 'error' });
    }
}

// ───────────────── 执行 ─────────────────
async function startShredding() {
    if (state.targets.length === 0) return;

    const sel = $('method-select');
    if (sel.selectedIndex < 0 || !sel.options[sel.selectedIndex]) {
        await tauriMessage('擦除算法尚未加载，请重启应用后再试。', { title: '错误', type: 'error' });
        return;
    }
    const methodIdx = sel.selectedIndex;
    const methodName = sel.options[methodIdx].text;
    const paths = state.targets.map((t) => t.path);
    const volTargets = state.targets.filter((t) => t.kind === 'volume');
    const totalFiles = state.targets.reduce((a, t) => a + (t.files || 0), 0);

    try {
        const hasSsd = await invoke('check_ssd', { paths });
        if (hasSsd) {
            const ok = await tauriAsk(
                '检测到目标位于固态硬盘（SSD）上！\n\n由于 SSD 的磨损均衡机制，普通覆写无法保证清除原始数据。高度敏感的数据请使用全盘加密或物理销毁。\n\n是否忽略此风险并继续？',
                { title: '存储介质提示', type: 'warning' }
            );
            if (!ok) return;
        }
    } catch (e) {}

    if (volTargets.length > 0) {
        const list = volTargets.map((t) => `  ${t.path}  （${fmtNum(t.files)} 个文件 · ${fmtBytes(t.bytes)}）`).join('\n');
        const freeNote = state.wipeFreeSpace ? '\n\n粉碎后会覆写卷上空闲空间。' : '';
        const ok1 = await tauriAsk(
            `即将执行【全盘粉碎】，以下卷上的所有文件与目录都会被永久删除：\n\n${list}${freeNote}\n\n卷本身与盘符会保留。此操作不可逆！`,
            { title: '全盘粉碎确认', type: 'warning' }
        );
        if (!ok1) return;

        const ok2 = await tauriAsk(
            `请再次确认：\n\n${volTargets.map((t) => t.path).join('  ')}\n\n上述卷上的全部数据将被永久销毁，无法通过任何方式恢复。\n\n确定继续吗？`,
            { title: '最终确认', type: 'warning' }
        );
        if (!ok2) return;
    }

    const ok = await tauriAsk(
        `将使用 ${methodName} 处理 ${state.targets.length} 个目标（共 ${fmtNum(totalFiles)} 个文件）。\n\n粉碎完成后目标文件夹及其所有子目录都会被一并删除，不会残留空目录。\n\n此操作不可逆！\n\n确定要继续吗？`,
        { title: '确认操作', type: 'warning' }
    );
    if (!ok) return;

    state.shredding = true;
    updateUI();

    $('progress-bar-fill').style.width = '0%';
    $('progress-label').textContent = '正在准备...';
    $('progress-current').textContent = '\u00a0';
    $('btn-cancel-progress').disabled = false;
    openModal('progress-dialog');

    try {
        await invoke('start_shredding', {
            methodIndex: methodIdx,
            wipeFreeSpace: state.wipeFreeSpace,
        });
    } catch (e) {
        closeModal('progress-dialog');
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

// ───────────────── 结果 ─────────────────
function showReport(data) {
    $('report-title').textContent = data.cancelled ? '已取消' : (data.success ? '完成' : '完成（存在未处理项）');
    $('report-summary').textContent = data.message || '';

    const detail = $('report-detail');
    detail.innerHTML = '';

    const addBlock = (title, items, cls) => {
        if (!items || items.length === 0) return;
        const h = document.createElement('div');
        h.className = 'report-h ' + (cls || '');
        h.textContent = title;
        detail.appendChild(h);
        const ul = document.createElement('ul');
        ul.className = 'report-list ' + (cls || '');
        items.forEach((it) => {
            const li = document.createElement('li');
            li.textContent = typeof it === 'string'
                ? maskPath(it)
                : `${maskPath(it.path)} — ${it.error}`;
            ul.appendChild(li);
        });
        detail.appendChild(ul);
    };

    addBlock(`未能处理的文件（${(data.filesFailed || []).length}）`, data.filesFailed, 'warn');
    addBlock(`残留项（${data.residualCount || 0}）`, data.residual, 'bad');
    addBlock('已跳过的项', data.skipped, 'muted');

    openModal('report-dialog');
}

// ───────────────── Event Listeners ─────────────────
function bindTauriEvents() {
    listen('shred-progress', (event) => {
        const d = typeof event.payload === 'object' ? event.payload : JSON.parse(event.payload);
        const pct = d.percent || 0;
        $('progress-bar-fill').style.width = pct + '%';

        let label;
        if (d.stage === 'scan') label = '正在统计目标...';
        else if (d.stage === 'free-space') label = `覆写空闲空间... ${pct}%`;
        else if (d.stage === 'done') label = '完成';
        else label = `处理中... ${pct}%（已处理 ${fmtNum(d.filesDone)} / ${fmtNum(d.filesTotal)} 个文件）`;
        $('progress-label').textContent = label;

        if (d.current && d.stage === 'shred') {
            const shown = maskPath(d.current);
            const c = shown.length > 70 ? '…' + shown.slice(-69) : shown;
            $('progress-current').textContent = c;
        } else if (d.stage === 'free-space' && d.current) {
            $('progress-current').textContent = d.current;
        }
    });

    listen('shred-finished', async (event) => {
        const data = typeof event.payload === 'object' ? event.payload : JSON.parse(event.payload);
        closeModal('progress-dialog');
        state.shredding = false;
        updateUI();

        if (data.cancelled) {
            setStatus('操作已取消');
        } else {
            setStatus(data.message);
        }

        // 成功且无残留时清空列表；有残留则保留，便于用户复查后重试
        if (data.success) {
            try { await invoke('clear_list'); } catch (e) {}
            state.targets = [];
            state.selectedIndices = [];
            renderList();
        }

        showReport(data);
    });
}

// ───────────────── Drag & Drop ─────────────────
const fileListEl = $('file-list');
fileListEl.addEventListener('dragover', (e) => { e.preventDefault(); fileListEl.classList.add('drag-over'); });
fileListEl.addEventListener('dragleave', () => { fileListEl.classList.remove('drag-over'); });
fileListEl.addEventListener('drop', (e) => { e.preventDefault(); });

if (window.__TAURI__ && window.__TAURI__.window) {
    window.__TAURI__.window.getCurrent().onFileDropEvent(async (evt) => {
        const p = evt.payload;
        try {
            if (p.type === 'hover') {
                fileListEl.classList.add('drag-over');
            } else if (p.type === 'drop') {
                fileListEl.classList.remove('drag-over');
                const paths = p.paths;
                if (!paths || paths.length === 0) return;
                setStatus(`正在添加 ${paths.length} 个项目...`);
                const res = await invoke('add_dropped_paths', { paths });
                applyAddResult(res, '拖放');
            } else {
                fileListEl.classList.remove('drag-over');
            }
        } catch (e) {
            fileListEl.classList.remove('drag-over');
            await tauriMessage(String(e), { title: '错误', type: 'error' });
        }
    });
}

// ───────────────── Init ─────────────────
async function init() {
    try {
        const methods = await invoke('get_methods');
        const sel = $('method-select');
        sel.innerHTML = '';
        methods.forEach((m) => {
            const opt = document.createElement('option');
            opt.textContent = m.name;
            opt.value = m.passes;
            sel.appendChild(opt);
        });
    } catch (e) {}

    $('btn-add-files').onclick = addFiles;
    $('btn-add-folder').onclick = addFolder;
    $('btn-add-volume').onclick = openVolumeDialog;
    $('btn-remove').onclick = removeSelected;
    $('btn-clear').onclick = clearList;
    $('btn-shred').onclick = startShredding;
    $('btn-cancel-progress').onclick = cancelShredding;

    $('btn-volume-cancel').onclick = () => closeModal('volume-dialog');
    $('btn-volume-ok').onclick = confirmVolumes;
    $('btn-report-ok').onclick = () => closeModal('report-dialog');

    renderList();
    setStatus('©绘萤者 开源地址:https://github.com/lynvortex/lynshred');
}

window.addEventListener('DOMContentLoaded', () => {
    if (!initTauri()) return;
    bindTauriEvents();
    init();
});
