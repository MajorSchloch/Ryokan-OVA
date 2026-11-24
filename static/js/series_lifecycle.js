// The removal dialog shows the folder, file count, and total bytes before confirmation.

function removeSeries(dbId) {
    // Open the dedicated destructive modal. This is a one-off rather than
    // a ryokanConfirm call because library-remove needs to render concrete
    // stakes (path, file count, size) that the generic confirm primitive
    // can't represent. See the #remove-series-modal block above.
    const modal = document.getElementById('remove-series-modal');
    if (!modal || modal.style.display === 'flex') return;
    const cancelBtn = document.getElementById('remove-series-cancel');
    const closeBtn = document.getElementById('remove-series-close');
    const confirmBtn = document.getElementById('remove-series-confirm');
    // A fresh open starts with the sync-exclusion box unticked; a tick
    // from a dialog that was cancelled must not carry over.
    const excludeBox = document.getElementById('remove-series-exclude');
    if (excludeBox) excludeBox.checked = false;

    function close() {
        modal.style.display = 'none';
        cancelBtn.removeEventListener('click', close);
        closeBtn.removeEventListener('click', close);
        confirmBtn.removeEventListener('click', doRemove);
        modal.removeEventListener('click', onBackdrop);
        document.removeEventListener('keydown', onKey);
    }
    function onBackdrop(ev) { if (ev.target === modal) close(); }
    function onKey(ev) { if (ev.key === 'Escape') close(); }
    function doRemove() {
        close();
        performRemoveSeries(dbId);
    }

    cancelBtn.addEventListener('click', close);
    closeBtn.addEventListener('click', close);
    confirmBtn.addEventListener('click', doRemove);
    modal.addEventListener('click', onBackdrop);
    document.addEventListener('keydown', onKey);
    modal.style.display = 'flex';
    // Focus Cancel by default so a stray Enter keypress can't
    // surprise-trigger the destructive action.
    setTimeout(function() { cancelBtn.focus(); }, 10);
}

function performRemoveSeries(dbId) {
    const btn = document.getElementById('btn-track');
    const originalText = btn.textContent;
    btn.disabled = true;
    btn.textContent = '...';

    fetch('/api/library/remove', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({
            id: dbId,
            delete_files: true,
            note: (document.getElementById('remove-series-note') || {}).value || '',
            add_exclusion: !!(document.getElementById('remove-series-exclude') || {}).checked,
        })
    })
    .then(async r => {
        // Backend always replies with JSON now — success body is the
        // cleanup summary, error body is {ok:false, stage, message}.
        // Best-effort parse so a malformed body still shows *something*
        // useful instead of silently collapsing to a generic 'Error'.
        let data = {};
        try { data = await r.json(); } catch (_) {}
        if (!r.ok) {
            const stage = data.stage ? ' [' + data.stage + ']' : '';
            throw new Error((data.message || 'Remove failed') + stage);
        }
        return data;
    })
    .then(data => {
        // A folder another series also uses is kept with its files;
        // say so on the library page this navigates to.
        if (data && data.folder === 'shared') {
            window.ryokanQueueToast({
                title: 'Files kept',
                body: `The files were left in place because ${data.folder_detail || 'another series'} uses the same folder.`,
                kind: 'warn',
                category: 'library',
            });
        }
        // `replace` rather than `href = '/'` because:
        //   * The series we just removed has its detail URL
        //     (`/series/<id>`) at the top of history; leaving it
        //     there means a stray Back press lands on a 404.
        //   * `replace` does not interact with bfcache the way an
        //     assignment can — the destination is a fresh load,
        //     never a snapshot of the pre-delete library.
        // The reported 2026-05-02 symptom of "removed series didn't
        // disappear from /" matched a forward-cache restore: the
        // browser served a snapshot of `/` from before the delete
        // because we'd visited `/` earlier in the session and the
        // navigation looked snapshot-able to the bfcache heuristic.
        window.location.replace('/');
    })
    .catch(err => {
        // Surface the real reason instead of the old generic 'Error'
        // button state — this was the bug where rss_seen's missing
        // ON DELETE CASCADE silently blocked every remove with
        // "FOREIGN KEY constraint failed" and the user had no way to
        // see it without devtools.
        window.ryokanAlert({
            title: 'Remove failed',
            body: err && err.message ? err.message : 'unknown error',
        });
        btn.disabled = false;
        btn.textContent = originalText;
    });
}

// A boosted franchise link can finish swapping the page before this script
// loads. Open the confirmation here, once its handlers are available.
(() => {
    const dbId = document.getElementById('series-data')?.dataset.dbId;
    if (dbId && new URLSearchParams(location.search).get('remove') === '1') {
        removeSeries(Number(dbId));
    }
})();
