// All table controls work on the current snapshot. Only the title links navigate.
(() => {
    const root = document.getElementById('library-table');
    if (!root || root.dataset.bound) return;
    root.dataset.bound = 'true';

    const key = 'ryokan.libraryTable.v1';
    let preferences = {};
    try { preferences = JSON.parse(localStorage.getItem(key)) || {}; } catch { /* storage unavailable */ }
    if (typeof preferences !== 'object' || Array.isArray(preferences)) preferences = {};
    function store() {
        try { localStorage.setItem(key, JSON.stringify(preferences)); return true; }
        catch { return false; }
    }

    const headers = [...root.querySelectorAll('th[data-key]')];
    const keys = headers.map(th => th.dataset.key);
    const tbody = root.querySelector('tbody');
    const rows = [...tbody.rows].map(tr => {
        const values = Object.fromEntries(keys.map((key, i) => {
            const text = tr.cells[i].dataset.value ?? tr.cells[i].textContent;
            return [key, text === 'Unknown' || text === '—' ? '' : text];
        }));
        return { tr, values, search: (values.title + ' ' + tr.dataset.aliases).toLowerCase() };
    });
    const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });
    const filters = [...root.querySelectorAll('[data-filter]')];
    const preset = root.querySelector('#table-preset');
    const savedGroup = root.querySelector('#table-saved');
    const name = root.querySelector('#table-name');
    const notice = root.querySelector('#table-save-status');
    const del = root.querySelector('#table-delete');
    const saved = Array.isArray(preferences.saved) ? preferences.saved.filter(s => s && typeof s.name === 'string' && s.filters) : [];
    const quick = { missing: { mode: 'monitored', files: 'missing' }, follows: { follow: 'yes', files: 'empty' } };

    function listPresets(selected) {
        savedGroup.replaceChildren(...saved.map(s => new Option(s.name, 'user:' + s.name)));
        preset.value = selected || '';
        del.disabled = !preset.value.startsWith('user:');
    }
    function currentFilters() {
        return Object.fromEntries(filters.map(el => [el.dataset.filter, el.value]));
    }
    function setFilters(values) {
        for (const el of filters) el.value = values?.[el.dataset.filter] || '';
    }
    function filterRows() {
        const { search, mode, follow, files } = currentFilters();
        const term = search.toLowerCase().trim();
        let count = 0;
        for (const { tr, values, search } of rows) {
            const monitored = tr.dataset.mode !== '' && tr.dataset.mode !== 'none';
            const match = search.includes(term)
                && (!mode || (mode === 'monitored' ? monitored : tr.dataset.mode === mode))
                && (!follow || tr.dataset.follow === follow)
                && (!files || (files === 'missing' && Number(values.missing) > 0)
                    || (files === 'empty' && values.disk === '0')
                    || (files === 'unknown' && values.disk === ''));
            tr.hidden = !match;
            if (match) count++;
        }
        const label = count === rows.length ? count : count + ' of ' + rows.length;
        root.querySelector('#table-count').textContent = label + (rows.length === 1 ? ' entry' : ' entries');
        root.querySelector('#table-empty').hidden = count > 0;
    }
    function rememberFilters() {
        preferences.filters = currentFilters();
        preferences.preset = preset.value;
        store();
        filterRows();
    }
    for (const el of filters) el.addEventListener('input', () => {
        preset.value = '';
        del.disabled = true;
        notice.textContent = '';
        rememberFilters();
    });
    preset.addEventListener('change', () => {
        const selected = saved.find(s => 'user:' + s.name === preset.value);
        setFilters(selected?.filters || quick[preset.value]);
        name.value = selected?.name || '';
        notice.textContent = '';
        del.disabled = !selected;
        rememberFilters();
    });
    root.querySelector('#table-clear').addEventListener('click', () => {
        preset.value = '';
        preset.dispatchEvent(new Event('change'));
    });
    root.querySelector('#table-save').addEventListener('click', () => {
        const title = name.value.trim();
        if (!title) {
            notice.textContent = 'Enter a filter set name.';
            name.focus();
            return;
        }
        const entry = { name: title, filters: currentFilters() };
        const index = saved.findIndex(s => s.name === title);
        if (index < 0) saved.push(entry);
        else saved[index] = entry;
        listPresets('user:' + title);
        preferences.saved = saved;
        preferences.preset = preset.value;
        preferences.filters = entry.filters;
        notice.textContent = store() ? 'Filter set saved.' : 'Browser storage is unavailable.';
    });
    del.addEventListener('click', () => {
        const index = saved.findIndex(s => 'user:' + s.name === preset.value);
        if (index < 0) return;
        saved.splice(index, 1);
        listPresets('');
        name.value = '';
        preferences.saved = saved;
        preferences.preset = '';
        notice.textContent = store() ? 'Filter set deleted.' : 'Browser storage is unavailable.';
    });

    let sortKey = keys.includes(preferences.sortKey) ? preferences.sortKey : 'title';
    let descending = preferences.descending === true;
    function sortRows() {
        for (const th of headers) {
            if (th.dataset.key === sortKey) th.setAttribute('aria-sort', descending ? 'descending' : 'ascending');
            else th.removeAttribute('aria-sort');
        }
        rows.sort((a, b) => {
            const av = a.values[sortKey], bv = b.values[sortKey];
            if (Boolean(av) !== Boolean(bv)) return av ? -1 : 1; // Unknown values stay last.
            const cmp = collator.compare(av, bv);
            return (descending ? -cmp : cmp) || collator.compare(a.values.title, b.values.title);
        });
        const fragment = document.createDocumentFragment();
        for (const row of rows) fragment.append(row.tr);
        tbody.append(fragment);
    }
    const visible = new Set(Array.isArray(preferences.columns) ? preferences.columns.filter(k => keys.includes(k)) : keys);
    const openIndex = keys.length;
    function showColumn(i, show) {
        root.querySelector('thead tr').cells[i].hidden = !show;
        for (const { tr } of rows) tr.cells[i].hidden = !show;
    }
    for (const [i, th] of headers.entries()) {
        const column = keys[i];
        th.querySelector('button').addEventListener('click', () => {
            descending = sortKey === column && !descending;
            sortKey = column;
            Object.assign(preferences, { sortKey, descending });
            store();
            sortRows();
        });
        const label = document.createElement('label');
        const checkbox = document.createElement('input');
        checkbox.type = 'checkbox';
        checkbox.checked = visible.has(column);
        label.append(checkbox, ' ' + th.textContent.trim());
        root.querySelector('#table-columns').append(label);
        showColumn(i, checkbox.checked);
        checkbox.addEventListener('change', () => {
            if (checkbox.checked) visible.add(column);
            else visible.delete(column);
            showColumn(i, checkbox.checked);
            showColumn(openIndex, !visible.has('title'));
            preferences.columns = [...visible];
            store();
        });
    }
    showColumn(openIndex, !visible.has('title'));
    listPresets(preferences.preset);
    setFilters(preferences.filters);
    if (preset.value.startsWith('user:')) name.value = preset.selectedOptions[0].text;
    sortRows();
    filterRows();
})();
