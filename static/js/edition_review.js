// Explicit playback downloads one bounded preview. Native controls seek
// the local blob, so a browser does not request another server encode.
ryokanRegisterPageInit('episode-edition-review', (() => {
    let root;
    let page;

    async function load(event) {
        const button = event.target.closest('button[data-edition-sample]');
        if (!button) return;
        const player = button.closest('.edition-player');
        const video = player.querySelector('video');
        const status = player.querySelector('[role=status]');
        const buttons = player.querySelectorAll('button');
        if (video.dataset.sample === button.dataset.editionSample) {
            video.play().catch(() => video.focus());
            return;
        }
        buttons.forEach(item => item.disabled = true);
        status.hidden = false;
        status.textContent = 'Preparing preview…';
        const signal = page.signal;
        try {
            const response = await fetch(button.dataset.editionSample, {signal});
            if (!response.ok || response.headers.get('Content-Type') !== 'video/mp4') {
                throw new Error('Preview unavailable');
            }
            const clip = await response.blob();
            if (signal.aborted) return;
            if (video.src) URL.revokeObjectURL(video.src);
            video.src = URL.createObjectURL(clip);
            video.dataset.sample = button.dataset.editionSample;
            video.hidden = false;
            status.textContent = 'Preview ready.';
            video.play().catch(() => video.focus());
        } catch (error) {
            if (!signal.aborted) {
                status.textContent = 'Preview unavailable. The file details are still available.';
            }
        } finally {
            buttons.forEach(item => item.disabled = false);
        }
    }

    return {
        check: () => !!document.querySelector('.edition-comparison'),
        mount() {
            root = document.querySelector('.edition-comparison');
            page = new AbortController();
            root.addEventListener('click', load);
        },
        unmount() {
            page.abort();
            root.removeEventListener('click', load);
            root.querySelectorAll('video[src]').forEach(video => {
                video.pause();
                URL.revokeObjectURL(video.src);
            });
        },
    };
})());
