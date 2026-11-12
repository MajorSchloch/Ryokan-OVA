// Run with: node --test tests/numbering_review_picker.test.cjs
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const source = file => fs.readFileSync(path.join(root, 'static/js', file), 'utf8');
const hash = '91a564c6d0f605344b67cef81eeab3199d61872b';
const tick = () => new Promise(resolve => setImmediate(resolve));

test('saved review opens only the tracked owner and canonical hash with explicit selection', () => {
    const calls = [];
    const context = vm.createContext({ window: { openGrabPicker: (...args) => calls.push(args) } });
    vm.runInContext(source('numbering_review.js'), context);
    context.chooseReviewedSourceFiles({ dataset: { seriesId: '23', infoHash: hash,
        sourceTitle: 'Legion <source> & "title"', sourceUrl: 'file:///untrusted' } });
    assert.equal(calls.length, 1);
    assert.equal(calls[0][0], 'magnet:?xt=urn:btih:' + hash);
    assert.deepEqual(JSON.parse(JSON.stringify(calls[0][1])), {
        seriesId: 23, infoHash: hash, title: 'Legion <source> & "title"', isBatch: true, startUnselected: true,
    });
    for (const dataset of [{ seriesId: '0', infoHash: hash }, { seriesId: '1519x', infoHash: hash },
        { seriesId: '23', infoHash: 'all' }, { seriesId: '23', infoHash: hash + '|other' }]) {
        context.chooseReviewedSourceFiles({ dataset });
    }
    assert.equal(calls.length, 1);
});

function pickerEnvironment() {
    const elements = new Map();
    for (const id of ['modal', 'body', 'confirm', 'title', 'subtitle', 'total', 'footer', 'toolbar']) {
        elements.set('grab-picker-' + id, { style: {}, dataset: {}, innerHTML: '', textContent: '',
            classList: { toggle() {} }, addEventListener() {}, querySelectorAll() { return []; } });
    }
    const requests = [], intervals = [];
    let resolvePreview;
    const pending = new Promise(resolve => { resolvePreview = resolve; });
    const document = { readyState: 'complete', getElementById: id => elements.get(id) || null,
        querySelectorAll: () => [], querySelector: () => null, addEventListener() {} };
    const window = { addEventListener() {}, ryokanToast() {} };
    const context = vm.createContext({ window, document, navigator: {}, Blob, console,
        setInterval(fn) { intervals.push(fn); return intervals.length; }, clearInterval() {},
        setTimeout() { return 1; }, clearTimeout() {},
        fetch(url, options = {}) {
            requests.push({ url, options });
            if (url === '/api/grab/preview') return pending;
            if (url === '/api/grab/preview/own-preview') return Promise.resolve({ ok: true, status: 200,
                json: async () => ({ status: 'ready', file_list: [
                    { name: '[Legion] Black Lagoon - 15.mkv', size: 100, episodes: [3] },
                    { name: '[Legion] Black Lagoon - 16.mkv', size: 100, episodes: [4] },
                ] }) });
            return Promise.resolve({ ok: true, status: 200, json: async () => ({}), text: async () => '{}' });
        },
    });
    const code = source('grab_picker.js').replace('    // ─── Global handlers',
        '    window.testPicker = { close: closeModal, unmount: unmountGrabPicker, flush: flushSelectionSync, confirm: confirmGrab, getSession: () => session };\n    // ─── Global handlers');
    vm.runInContext(code, context);
    const ready = () => resolvePreview({ ok: true, json: async () => ({ preview_id: 'own-preview' }) });
    return { window, context, elements, requests, intervals, ready };
}

test('new review intent reaches the server before metadata and starts with no files selected', async () => {
    const env = pickerEnvironment();
    env.window.openGrabPicker('magnet:?xt=urn:btih:' + hash, { infoHash: hash, seriesId: 23, startUnselected: true });
    const first = JSON.parse(env.requests[0].options.body);
    assert.equal(first.start_unselected, true);
    assert.equal(first.series_id, 23);
    env.ready();
    await tick();
    assert.equal(env.window.testPicker.getSession().wanted.size, 0);
    assert.equal(env.elements.get('grab-picker-confirm').disabled, true);
    const initialSelection = env.requests.find(request => request.url === '/api/grab/selection/own-preview');
    assert.deepEqual(JSON.parse(initialSelection.options.body), { wanted_indices: [] });
    // Exercise the real confirmation payload after a deliberate single-file pick.
    env.window.testPicker.getSession().wanted.add(0);
    env.window.testPicker.flush(false);
    env.window.testPicker.confirm();
    await tick();
    const confirm = env.requests.find(request => request.url === '/api/grab/confirm');
    assert.deepEqual(JSON.parse(confirm.options.body).wanted_indices, [0]);
});

for (const leave of ['close', 'unmount']) {
    test(`a late initial preview response after ${leave} cannot start a hidden heartbeat`, async () => {
        const env = pickerEnvironment();
        env.window.openGrabPicker('magnet:?xt=urn:btih:' + hash, { infoHash: hash, seriesId: 23, startUnselected: true });
        env.window.testPicker[leave]();
        env.ready();
        await tick();
        assert.equal(env.window.testPicker.getSession(), null);
        assert.equal(env.intervals.length, 0);
        assert.equal(env.requests.length, 1, 'closed attempts must not poll, post selection, or keep themselves alive');
    });
}

test('a replacement page with another picker cannot receive the old initial response or heartbeat', async () => {
    const env = pickerEnvironment();
    env.window.openGrabPicker('magnet:?xt=urn:btih:' + hash, { infoHash: hash, seriesId: 23, startUnselected: true });
    env.elements.set('grab-picker-modal', { style: {}, dataset: {} });
    env.ready();
    await tick();
    assert.equal(env.window.testPicker.getSession(), null);
    assert.equal(env.intervals.length, 0);
    assert.equal(env.requests.length, 1);
});

test('ordinary callers retain default selection and close still posts the last selection for walkaway', async () => {
    const env = pickerEnvironment();
    env.window.openGrabPicker('magnet:?xt=urn:btih:' + hash, { infoHash: hash });
    assert.equal(JSON.parse(env.requests[0].options.body).start_unselected, false);
    env.ready();
    await tick();
    assert.equal(env.window.testPicker.getSession().wanted.size, 2);
    env.window.testPicker.getSession().wanted.delete(1);
    env.window.testPicker.close();
    const selections = env.requests.filter(request => request.url === '/api/grab/selection/own-preview');
    assert.deepEqual(JSON.parse(selections.at(-1).options.body), { wanted_indices: [0] });
    assert.equal(env.window.testPicker.getSession(), null);
});
