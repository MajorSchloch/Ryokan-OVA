// Shared navigation and display state. This module never starts acquisition.
(() => {
    window.__ryokanLibraryManagementCleanup?.();
    function safePath(value) {
        try {
            const u = new URL(value, location.origin);
            if (u.origin !== location.origin || !(/^\/(?:$|franchises\/|anime\/|library\/(?:table|activity))/).test(u.pathname)) return null;
            return u.pathname + u.search;
        } catch { return null; }
    }
    function read(key, fallback) { try { return JSON.parse(sessionStorage.getItem(key)) || fallback; } catch { return fallback; } }
    function write(key, value) { try { sessionStorage.setItem(key, JSON.stringify(value)); } catch { /* storage optional */ } }
    let cleanup = () => {}, mountedRoot = null;
    const lifecycle = {
        check: () => !!document.querySelector('[data-title-page], #library-grid, #library-table'),
        mount() {
            const root = document.querySelector('[data-title-page], #library-grid, #library-table');
            if (!root || mountedRoot === root) return;
            cleanup();
            mountedRoot = root;
            const titlePage = document.querySelector('[data-title-page]');
            const params = new URLSearchParams(location.search);
            const library = safePath(params.get('return_to')) || safePath(read('ryokan.libraryContext', {}).url) || '/?individual=0';
            const parent = safePath(params.get('franchise_return'));
            document.querySelectorAll('[data-library-back]').forEach(a => { a.href = library; });
            document.querySelectorAll('[data-franchise-back]').forEach(a => { if (parent?.startsWith('/franchises/')) a.href = parent; });
            let timer = null, controller = null;
            const saveScroll = () => {
                const saved = read('ryokan.libraryScroll', {});
                saved[location.pathname + location.search] = scrollY;
                write('ryokan.libraryScroll', Object.fromEntries(Object.entries(saved).slice(-80)));
            };
            const click = event => {
                const a = event.target.closest('a[href]');
                if (!a) return;
                const path = safePath(a.getAttribute('href'));
                if (!path) return;
                saveScroll();
                if (!titlePage) write('ryokan.libraryContext', {url: location.pathname + location.search});
                if (!a.hasAttribute('data-title-tab') && /^\/(franchises|anime)\//.test(path) && !a.hasAttribute('data-franchise-back')) {
                    const u = new URL(path, location.origin);
                    u.searchParams.set('return_to', titlePage ? library : location.pathname + location.search);
                    if (location.pathname.startsWith('/franchises/')) u.searchParams.set('franchise_return', location.pathname + location.search);
                    a.href = u.pathname + u.search;
                }
            };
            document.addEventListener('click', click, true);
            window.addEventListener('pagehide', saveScroll);
            const downloadBox = titlePage?.querySelector('[data-scoped-downloads]');
            const tabs = titlePage ? [...titlePage.querySelectorAll('[data-title-tab]')] : [];
            const active = tabs.some(a => a.dataset.titleTab === params.get('tab')) ? params.get('tab') : titlePage?.dataset.defaultTab;
            if (titlePage) {
                titlePage.querySelectorAll('[data-title-panel]').forEach(p => { p.hidden = p.dataset.titlePanel !== active; });
                tabs.forEach(a => {
                    const u = new URL(location.href); u.searchParams.set('tab', a.dataset.titleTab);
                    a.href = u.pathname + u.search;
                    if (a.dataset.titleTab === active) a.setAttribute('aria-current', 'page');
                });
            }
            async function refreshDownloads() {
                if (!downloadBox?.isConnected || active !== 'downloads') return;
                if (controller) return;
                const requestController = new AbortController(); controller = requestController;
                try {
                    const response = await fetch(downloadBox.dataset.scopedDownloads, {signal: requestController.signal});
                    if (!response.ok) throw new Error('queue');
                    const html = await response.text();
                    if (downloadBox.isConnected && controller === requestController && !requestController.signal.aborted) downloadBox.innerHTML = html;
                } catch (e) {
                    if (e.name !== 'AbortError' && downloadBox.isConnected) downloadBox.textContent = 'Download status is unavailable. Try again or open Downloads.';
                } finally { if (controller === requestController) controller = null; }
            }
            if (active === 'downloads') { refreshDownloads(); timer = setInterval(refreshDownloads, 15000); }
            const filters = titlePage?.querySelector('[data-franchise-filters]');
            function filterReleases(update) {
                if (!filters) return;
                const values = Object.fromEntries(new FormData(filters));
                const term = (values.f_q || '').trim().toLowerCase();
                let shown = 0;
                const rows = [...titlePage.querySelectorAll('[data-franchise-release]')];
                rows.forEach(row => {
                    const raw = row.dataset.format.toUpperCase();
                    const type = raw.startsWith('TV') || raw === 'ONA' ? 'tv' : raw === 'MOVIE' ? 'movie' : raw === 'OVA' ? 'ova' : 'other';
                    const matches = (!term || row.dataset.title.toLowerCase().includes(term)) && (!values.f_monitor || row.dataset.monitor === values.f_monitor) && (!values.f_type || type === values.f_type) && (!values.f_status || row.dataset.states.split(/\s+/).includes(values.f_status));
                    row.hidden = !matches; if (matches) shown++;
                });
                filters.querySelector('[data-franchise-count]').textContent = `${shown} of ${rows.length} releases`;
                titlePage.querySelector('[data-franchise-empty]').hidden = shown > 0;
                if (update) {
                    const u = new URL(location.href);
                    ['f_q', 'f_monitor', 'f_type', 'f_status'].forEach(name => { values[name] ? u.searchParams.set(name, values[name]) : u.searchParams.delete(name); });
                    history.replaceState(null, '', u.pathname + u.search);
                    tabs.forEach(a => { const destination = new URL(u); destination.searchParams.set('tab', a.dataset.titleTab); a.href = destination.pathname + destination.search; });
                }
            }
            if (filters) {
                ['f_q', 'f_monitor', 'f_type', 'f_status'].forEach(name => { if (params.has(name)) filters.elements[name].value = params.get(name); });
                filters.addEventListener('input', () => filterReleases(true));
                filters.addEventListener('submit', e => { e.preventDefault(); filterReleases(true); });
                filters.addEventListener('reset', () => queueMicrotask(() => filterReleases(true)));
                filterReleases(false);
            }
            function enhance() {
                let options = {}; try { options = JSON.parse(localStorage.getItem('ryokan.libraryDisplay.v1')) || {}; } catch { /* optional */ }
                document.querySelectorAll('[data-summary-part]').forEach(p => { p.hidden = options[p.dataset.summaryPart] === false; });
                document.querySelectorAll('[data-summary-toggle]').forEach(p => { p.checked = options[p.dataset.summaryToggle] !== false; });
                document.querySelectorAll('[data-alias-editor]').forEach(textarea => {
                    if (textarea.dataset.chipsBound) return;
                    textarea.dataset.chipsBound = '1';
                    const editor = document.createElement('div'); editor.className = 'alias-editor';
                    const chips = document.createElement('ul'); chips.className = 'title-chips';
                    const input = document.createElement('input'); input.type = 'text'; input.placeholder = 'Add another title'; input.setAttribute('aria-label', 'New alternate title'); input.maxLength = 512;
                    const add = document.createElement('button'); add.type = 'button'; add.className = 'btn btn-secondary btn-sm'; add.textContent = 'Add title';
                    function names() { return textarea.value.split(/\r?\n/).map(x => x.trim()).filter(Boolean); }
                    function render() {
                        chips.replaceChildren();
                        names().forEach((name, index) => {
                            const li = document.createElement('li'), remove = document.createElement('button');
                            li.append(document.createTextNode(name)); remove.type = 'button'; remove.textContent = '×'; remove.setAttribute('aria-label', 'Remove alternate title ' + name);
                            remove.addEventListener('click', () => { textarea.value = names().filter((_, i) => i !== index).join('\n'); render(); });
                            li.append(remove); chips.append(li);
                        });
                    }
                    function addName() { const name = input.value.trim(), list = names(); if (!name) return; if (!list.some(x => x.toLowerCase() === name.toLowerCase())) list.push(name); textarea.value = list.join('\n'); input.value = ''; render(); }
                    add.addEventListener('click', addName); input.addEventListener('keydown', e => { if (e.key === 'Enter') { e.preventDefault(); addName(); } });
                    editor.append(chips, input, add); textarea.after(editor); textarea.hidden = true; render();
                });
                document.querySelectorAll('#media-requirements-form select[name$="_mode"]').forEach(select => {
                    select.closest('.form-group').querySelectorAll('input, label:not(:first-child), select[name$="_match"]').forEach(el => { if (el !== select && el.getAttribute('for') !== select.id) el.hidden = select.value !== 'require'; });
                });
            }
            const submit = event => {
                const form=event.target;
                if(titlePage && form.querySelector('input[name="future_controls"]')) {
                    const field=form.querySelector('input[name="return_to"]'); if(field) field.value=location.pathname+location.search;
                }
            };
            document.addEventListener('submit',submit,true);
            const change = event => {
                const toggle = event.target.closest('[data-summary-toggle]');
                if (toggle) { let options = {}; try { options = JSON.parse(localStorage.getItem('ryokan.libraryDisplay.v1')) || {}; options[toggle.dataset.summaryToggle] = toggle.checked; localStorage.setItem('ryokan.libraryDisplay.v1', JSON.stringify(options)); } catch { /* optional */ } }
                enhance();
            };
            document.addEventListener('change', change); document.addEventListener('htmx:after:settle', enhance); enhance();
            const previous = read('ryokan.libraryScroll', {})[location.pathname + location.search];
            if (Number.isFinite(previous)) requestAnimationFrame(() => requestAnimationFrame(() => scrollTo(0, previous)));
            if (params.get('remove') === '1') requestAnimationFrame(() => document.querySelector('[onclick*="removeSeries"], #remove-series-btn')?.click());
            cleanup = () => {
                mountedRoot = null;
                controller?.abort(); if (timer !== null) clearInterval(timer);
                document.removeEventListener('click', click, true);
                document.removeEventListener('change', change);
                document.removeEventListener('submit',submit,true);
                document.removeEventListener('htmx:after:settle', enhance);
                window.removeEventListener('pagehide', saveScroll);
            };
        },
        unmount() { cleanup(); cleanup = () => {}; }
    };
    window.__ryokanLibraryManagementCleanup = () => cleanup();
    window.ryokanRegisterPageInit('library-management', lifecycle);
    // A boosted body can replace the registration while it is still active.
    // The DOM guard makes this safe after either immediate or deferred mount.
    if (lifecycle.check()) lifecycle.mount();
})();
