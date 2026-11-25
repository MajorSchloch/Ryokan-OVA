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
                saveScroll();
                if (a.hasAttribute('data-franchise-review')) {
                    const destination = new URL(a.href);
                    destination.searchParams.set('return_to', location.pathname + location.search);
                    a.href = destination.pathname + destination.search;
                    return;
                }
                const path = safePath(a.getAttribute('href'));
                if (!path) return;
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
            const filterNames = ['f_q', 'f_monitor', 'f_type', 'f_status', 'f_order', 'f_hide_unfollowed'];
            const rows = [...(titlePage?.querySelectorAll('[data-franchise-list] > [data-franchise-release]') ?? [])];
            const sections = [...(titlePage?.querySelectorAll('[data-franchise-section]') ?? [])];
            const releaseOrder = filters?.elements.f_order.value;
            function filterReleases(update) {
                const values = Object.fromEntries(new FormData(filters));
                values.f_q = values.f_q.trim();
                const url = new URL(location.href);
                if (update) {
                    filterNames.forEach(name => { values[name] ? url.searchParams.set(name, values[name]) : url.searchParams.delete(name); });
                    // Order changes can regroup the rows. Let the server render
                    // the chosen order while carrying the current filters along.
                    if (values.f_order !== releaseOrder) {
                        url.hash = 'franchise-releases-title';
                        location.assign(url);
                        return;
                    }
                }
                const term = values.f_q.toLowerCase();
                let shown = 0;
                rows.forEach(row => {
                    const matches = (!term || row.dataset.title.toLowerCase().includes(term))
                        && (!values.f_monitor || row.dataset.monitor === values.f_monitor)
                        && (!values.f_type || row.dataset.format === values.f_type)
                        && (!values.f_status || row.dataset.states.split(/\s+/).includes(values.f_status))
                        && (!values.f_hide_unfollowed || row.dataset.followed === 'true');
                    row.hidden = !matches;
                    if (matches) shown++;
                });
                sections.forEach(section => {
                    section.hidden = !section.querySelector('[data-franchise-release]:not([hidden])');
                    if (!section.hidden && term) {
                        const details = section.matches('.library-franchise-extras')
                            ? section : section.querySelector('.library-storyline-releases');
                        if (details) details.open = true;
                    }
                });
                filters.querySelector('[data-franchise-count]').textContent = `${shown} of ${rows.length} releases`;
                titlePage.querySelector('[data-franchise-empty]').hidden = shown > 0;
                if (update) {
                    history.replaceState(history.state, '', url.pathname + url.search);
                    tabs.forEach(a => { const destination = new URL(url); destination.searchParams.set('tab', a.dataset.titleTab); a.href = destination.pathname + destination.search; });
                }
            }
            if (filters && titlePage.querySelector('[data-franchise-list]')) {
                filters.addEventListener('input', () => filterReleases(true));
                filters.addEventListener('submit', e => { e.preventDefault(); filterReleases(true); });
                filters.querySelector('[data-franchise-reset]').addEventListener('click', event => {
                    event.preventDefault();
                    filterNames.forEach(name => {
                        const control = filters.elements[name];
                        if (control.type === 'checkbox') control.checked = false;
                        else control.value = '';
                    });
                    filterReleases(true);
                });
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
