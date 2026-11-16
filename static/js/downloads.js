// ── Queue tab ───────────────────────────────────────────────────────────

function queueActionError(message) {
    const error = document.getElementById('queue-error');
    if (error) { error.hidden = !message; error.textContent = message || ''; }
}

function loadQueue() {
    const container = document.getElementById('queue-container') || document.getElementById('source-job-controls');
    if (!container) return;
    fetch(container.id === 'source-job-controls' ? '/downloads?tab=seeding&fragment=sources' : '/downloads?tab=queue&fragment=queue')
        .then(async r => { if (!r.ok) throw new Error('Download clients are unavailable. The last displayed queue is retained.'); return r.text(); })
        .then(html => {
            if (!new DOMParser().parseFromString(html, 'text/html').querySelector('[data-download-fragment]')) throw new Error('The download session is unavailable. The last displayed queue is retained.');
            if (!container.isConnected || container !== document.getElementById(container.id)) return;
            queueActionError('');
            if (container.innerHTML === html) return;
            const wrap = container.querySelector('.rss-table-wrap');
            const top = wrap ? wrap.scrollTop : 0;
            container.innerHTML = html;
            const next = container.querySelector('.rss-table-wrap');
            if (next) next.scrollTop = top;
        }).catch(error => queueActionError(error.message));
}

function runDownloadAction(action, body) {
    return fetch('/api/downloads/' + action, {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify(body)})
        .then(async r => { if (!r.ok) throw new Error((await r.text()) || 'The download action was rejected.'); queueActionError(''); setTimeout(loadQueue, 500); })
        .catch(error => queueActionError(error.message));
}
function pauseTorrent(hash) { return runDownloadAction('pause', {hash}); }
function resumeTorrent(hash) { return runDownloadAction('resume', {hash}); }
function deleteTorrent(hash) {
    window.ryokanConfirm({title:'Remove download',body:'Remove this job from its download client?',yesLabel:'Remove',noLabel:'Cancel',extras:[{id:'deleteFiles',label:'Also permanently delete downloaded files',default:false}]})
        .then(res => { if (res.ok) return runDownloadAction('delete', {hash, delete_files:!!res.extras.deleteFiles}); });
}

// Queue row actions. Each button names its action in `data-dl-action`
// and the row carries the id in `data-hash`; neither is built into an
// inline `onclick`, where HTML-escaping the id doesn't help: the parser
// decodes `&#39;` back to `'` before the handler's JS runs. One document
// listener, bound once since this script re-runs on every boosted
// visit, covers both server-rendered tables.
if (!window.__ryokanDownloadsQueueActions) {
    window.__ryokanDownloadsQueueActions = true;
    document.addEventListener('click', function (ev) {
        const btn = ev.target.closest('#queue-container [data-dl-action], #source-job-controls [data-dl-action]');
        const row = btn && btn.closest('tr[data-hash]');
        if (!row) return;
        const hash = row.dataset.hash;
        switch (btn.dataset.dlAction) {
            case 'resume': resumeTorrent(hash); break;
            case 'pause': pauseTorrent(hash); break;
            case 'copy': window.ryokanCopy(hash, btn); break;
            case 'delete': deleteTorrent(hash); break;
        }
    });
}

// Stop the shared fragment poller when the user leaves Downloads.
window.ryokanRegisterPageInit('downloads-queue', {
    check: function () {
        return !!(document.getElementById('queue-container') || document.getElementById('source-job-controls'));
    },
    mount: function () {
        if (window.__downloadsQueuePoller) {
            // Defensive — if some unexpected path left a handle
            // dangling (e.g., a browser back-forward cache restore
            // skipped the unmount), clear it before starting fresh.
            clearInterval(window.__downloadsQueuePoller);
        }
        window.__downloadsQueuePoller = setInterval(loadQueue, 5000);
    },
    unmount: function () {
        if (window.__downloadsQueuePoller) {
            clearInterval(window.__downloadsQueuePoller);
            window.__downloadsQueuePoller = null;
        }
    },
});

// ── History tab ─────────────────────────────────────────────────────────

function filterHistory() {
    const search = (document.getElementById('history-filter')?.value || '').toLowerCase().trim();
    const state = document.getElementById('history-state-filter')?.value || 'all';
    const rows = document.querySelectorAll('#history-table tbody tr');
    rows.forEach(row => {
        const text = (row.dataset.text || '').toLowerCase();
        const rowState = row.dataset.state || '';
        const matchesState = state === 'all' || rowState === state;
        const matchesSearch = !search || text.includes(search);
        row.style.display = (matchesState && matchesSearch) ? '' : 'none';
    });
}

// ── Blocklist tab ───────────────────────────────────────────────────────
// HTMX migration (issue #129) — removeFromBlocklist() removed; the
// row form's `hx-post` + `hx-target="closest tr"` + `hx-swap="outerHTML"`
// fires the request and strips the row in one declarative shot. The
// `data-ryokan-confirm-*` attrs route through the htmx:confirm bridge
// in base.js so cancelling leaves the row alone (same pattern as
// Phase 1 settings deletes).
