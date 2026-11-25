(() => {
    const dialogId = 'discover-preview-dialog';
    const posterSelector = 'a[data-discover-preview]';
    let active = null;

    function posterCover(poster) {
        const cover = poster.querySelector('img')?.cloneNode();
        if (cover) {
            cover.className = 'discover-preview-cover';
            cover.removeAttribute('loading');
        }
        return cover;
    }

    function posterDetails(poster) {
        const snapshot = document.createElement('div');
        snapshot.className = 'discover-preview-details';
        const cover = posterCover(poster);
        if (cover) snapshot.append(cover);
        const copy = poster.querySelector('.discover-poster-copy').cloneNode(true);
        copy.className = 'discover-preview-copy';
        copy.querySelector('strong').remove();
        const rating = poster.querySelector('.provider-rating');
        if (rating) copy.prepend(rating.cloneNode(true));
        snapshot.append(copy);
        return snapshot;
    }

    function isCurrent(preview, request) {
        return active === preview && preview.request === request && preview.dialog.isConnected && preview.dialog.open;
    }

    function fillMissingFacts(details, poster) {
        if (!details.querySelector('.discover-preview-cover')) {
            const cover = posterCover(poster);
            if (cover) details.prepend(cover);
        }
        const copy = details.querySelector('.discover-preview-copy');
        const rating = poster.querySelector('.provider-rating');
        if (rating && !copy.querySelector('.provider-rating')) {
            let facts = copy.querySelector('.discover-preview-facts');
            if (!facts) {
                facts = document.createElement('div');
                facts.className = 'discover-preview-facts';
                copy.prepend(facts);
            }
            facts.prepend(rating.cloneNode(true));
        }
        const genres = poster.querySelectorAll('.discover-poster-genres > span');
        if (genres.length && !copy.querySelector('.discover-preview-genres')) {
            const list = document.createElement('ul');
            list.className = 'discover-preview-genres';
            list.setAttribute('aria-label', 'Genres');
            for (const genre of genres) {
                const item = document.createElement('li');
                item.textContent = genre.textContent;
                list.append(item);
            }
            copy.insertBefore(list, copy.querySelector('.discover-preview-synopsis'));
        }
    }

    async function loadPreview(preview) {
        preview.request?.abort();
        const request = new AbortController();
        preview.request = request;
        preview.body.setAttribute('aria-busy', 'true');
        preview.status.textContent = 'Loading preview…';
        preview.status.hidden = false;
        preview.retry.hidden = true;

        try {
            const response = await fetch(preview.poster.dataset.discoverPreview, {
                signal: request.signal,
                mode: 'same-origin',
                redirect: 'error',
            });
            if (!response.ok) throw new Error('Preview request failed');
            const markup = await response.text();
            if (!isCurrent(preview, request)) return;

            const template = document.createElement('template');
            template.innerHTML = markup;
            const details = template.content.firstElementChild;
            if (!details?.matches('[data-discover-preview-details][data-preview-title]')) {
                throw new Error('Preview response was not a preview');
            }
            fillMissingFacts(details, preview.poster);
            if (details.dataset.previewTitle.trim()) preview.title.textContent = details.dataset.previewTitle;
            preview.body.replaceChildren(details);
            preview.status.hidden = true;
        } catch {
            if (!isCurrent(preview, request)) return;
            preview.status.textContent = 'Preview unavailable. Try again or open the franchise.';
            preview.retry.hidden = false;
        } finally {
            if (isCurrent(preview, request)) preview.body.removeAttribute('aria-busy');
        }
    }

    function clearPreview(restoreFocus) {
        if (!active) return;
        const { dialog, poster, request } = active;
        active = null;
        request?.abort();
        if (dialog.open) dialog.close();
        if (restoreFocus && poster.isConnected) poster.focus({ preventScroll: true });
    }

    function onClick(event) {
        if (active && event.target === active.dialog) {
            active.dialog.close();
            return;
        }
        if (active && event.target === active.retry) {
            active.dialog.querySelector('[autofocus]').focus({ preventScroll: true });
            void loadPreview(active);
            return;
        }
        if (event.defaultPrevented || event.button !== 0 || event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
        const poster = event.target.closest?.(posterSelector);
        if (!poster?.dataset.discoverPreview) return;
        const dialog = document.getElementById(dialogId);
        if (typeof dialog?.showModal !== 'function' || dialog.open) return;

        clearPreview(false);
        const title = dialog.querySelector('#discover-preview-title');
        const body = dialog.querySelector('[data-discover-preview-body]');
        title.textContent = poster.querySelector('.discover-poster-copy strong').textContent;
        body.replaceChildren(posterDetails(poster));
        dialog.querySelector('[data-discover-preview-open]').href = poster.href;
        dialog.querySelector('.discover-preview-content').scrollTop = 0;
        dialog.showModal();
        event.preventDefault();
        active = {
            poster, dialog, title, body, request: null,
            status: dialog.querySelector('[data-discover-preview-status]'),
            retry: dialog.querySelector('[data-discover-preview-retry]'),
        };
        void loadPreview(active);
    }

    function onClose(event) {
        if (active && event.target === active.dialog && !active.dialog.open) clearPreview(true);
    }

    function onPageHide() {
        clearPreview(false);
    }

    window.ryokanRegisterPageInit('discover-preview', {
        check: () => typeof document.getElementById(dialogId)?.showModal === 'function',
        mount() {
            document.addEventListener('click', onClick);
            document.addEventListener('close', onClose, true);
            window.addEventListener('pagehide', onPageHide);
        },
        unmount() {
            clearPreview(false);
            document.removeEventListener('click', onClick);
            document.removeEventListener('close', onClose, true);
            window.removeEventListener('pagehide', onPageHide);
        },
    });
})();
