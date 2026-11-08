function getTitleByLang(entry, lang) {
    if (lang === 'native') return entry.title_native || entry.title_romaji || entry.title_english || '';
    if (lang === 'romaji') return entry.title_romaji || entry.title_english || entry.title_native || '';
    return entry.title_english || entry.title_romaji || entry.title_native || '';
}

var _pendingSeriesId = null;
var _selectedMonitorMode = 'all';

function addSeries(anilistId, btn) {
    const entry = JSON.parse(btn.dataset.entry);
    // The shared monitoring dialog can configure one new entry at a time.
    const list = btn.closest('#discovery-results');
    const buttons = list ? list.querySelectorAll('button[data-entry]') : [btn];
    buttons.forEach(button => { button.disabled = true; });
    btn.textContent = '...';

    fetch('/api/library/add', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            anilist_id: entry.id,
            mal_id: entry.id_mal || null,
            title: entry.title_english || entry.title_romaji || entry.title_native,
            title_romaji: entry.title_romaji || '',
            title_english: entry.title_english || '',
            title_native: entry.title_native || '',
            cover_url: entry.cover_url,
            format: entry.format,
            status: entry.status,
            episodes: entry.episodes,
            season_year: entry.season_year,
        })
    })
    .then(async r => {
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) throw new Error(data.message || 'Add failed');
        return data;
    })
    .then(data => {
        if (!btn.isConnected) return;
        if (!data.created && list) {
            // Another tab or a follow already added it. Keep its monitoring.
            window.location.assign('/series/' + data.id);
            return;
        }
        btn.textContent = 'Added';
        btn.classList.add('btn-success');
        _pendingSeriesId = data.id;
        _selectedMonitorMode = 'all';
        const lang = document.documentElement.dataset.titleLanguage || 'english';
        const title = getTitleByLang(entry, lang) || entry.title_romaji || entry.title_english || 'this series';
        document.getElementById('monitor-series-title').textContent = title;
        // Reset mode buttons
        document.querySelectorAll('.monitor-option-btn').forEach(b => b.classList.remove('active'));
        const allBtn = document.querySelector('.monitor-option-btn[data-mode="all"]');
        if (allBtn) allBtn.classList.add('active');
        // Close add modal and show monitoring modal
        const searchModal = document.getElementById('add-modal');
        if (searchModal) searchModal.style.display = 'none';
        document.getElementById('monitor-modal').style.display = 'flex';
        allBtn?.focus();
    })
    .catch(() => {
        btn.textContent = 'Error';
        btn.classList.add('btn-error');
        buttons.forEach(button => { button.disabled = false; });
    });
}

function selectMonitorMode(btn) {
    document.querySelectorAll('.monitor-option-btn').forEach(b => b.classList.remove('active'));
    btn.classList.add('active');
    _selectedMonitorMode = btn.dataset.mode;
}

// Issue #166 — confirmMonitoring moved to declarative hx-post on the
// monitor-confirm-btn in templates/index.html. The button reads its
// payload from `confirmMonitoringVals()` via `hx-vals='js:'`, and the
// server returns `HX-Refresh: true` so htmx reloads the page on
// success. The pre-migration fallback `if (!_pendingSeriesId)
// location.reload()` is preserved by `hx-on::response:error="window.
// location.reload()"` plus the 400 the handler issues on missing id.
function confirmMonitoringVals() {
    return {
        series_id: _pendingSeriesId,
        monitor_mode: _selectedMonitorMode,
        auto_grab: true,
    };
}
