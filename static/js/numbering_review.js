// This action is rendered only for a saved, freshly verified source review.
// The preview endpoint rechecks the owner and exact saved source independently.
function chooseReviewedSourceFiles(button) {
    var seriesId = Number(button.dataset.seriesId);
    var hash = button.dataset.infoHash || '';
    if (!Number.isSafeInteger(seriesId) || seriesId <= 0 || !/^[a-f0-9]{40}$/.test(hash)) return;
    if (typeof window.openGrabPicker !== 'function') return;
    window.openGrabPicker('magnet:?xt=urn:btih:' + hash, {
        seriesId: seriesId,
        infoHash: hash,
        title: button.dataset.sourceTitle || '',
        isBatch: true,
        startUnselected: true,
    });
}
