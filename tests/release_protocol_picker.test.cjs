// Run with: node --test tests/release_protocol_picker.test.cjs
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');

function environment(file) {
    const picks = [], requests = [];
    const context = vm.createContext({
        window: { GRAB_PREVIEW_MODE: 'batches_only', openGrabPicker: (...args) => picks.push(args), addEventListener() {} },
        document: { getElementById() { return null; }, addEventListener() {}, body: { addEventListener() {} } },
        SD: { id: 100, dbId: 10 },
        fetch: (url, options) => { requests.push({ url, options }); return new Promise(() => {}); },
    });
    let source = fs.readFileSync(path.join(root, 'static/js', file), 'utf8');
    if (file === 'wanted.js') {
        // Expose the real closure function only in this VM, preserving its
        // actual picker/direct-grab branches and page-local state.
        source = source.replace("    document.addEventListener('click',", "    window.testGrabWantedResult = grabWantedResult;\n    document.addEventListener('click',");
    }
    vm.runInContext(source, context);
    return { context, picks, requests };
}

for (const flow of ['episode', 'batch', 'wanted episode', 'wanted batch']) {
    for (const protocol of ['torrent', 'usenet', 'unknown']) {
        test(`${flow}: ${protocol} uses its actual protocol, not its hash or peer placeholders`, () => {
            const wanted = flow.startsWith('wanted');
            const { context, picks, requests } = environment(wanted ? 'wanted.js' : 'series_interactive_search.js');
            const result = { title: 'Example', is_batch: true, protocol,
                torrent: 'https://example.test/download', info_hash: '0123456789abcdef0123456789abcdef01234567',
                seeders: protocol === 'torrent' ? 42 : null, indexer_id: 7 };
            const button = { dataset: { result: JSON.stringify(result) } };
            if (wanted) context.window.testGrabWantedResult(button, flow.endsWith('batch') ? null : 3);
            else if (flow === 'batch') context.grabInteractiveBatchResult(button);
            else context.grabInteractiveResult(3, button);
            if (protocol === 'torrent') {
                assert.equal(picks.length, 1);
                assert.equal(requests.length, 0);
                assert.equal(picks[0][1].seeders, 42);
            } else {
                assert.equal(picks.length, 0, 'Usenet and unknown sources must not show a torrent picker or its seeder hint');
                assert.equal(requests.length, 1);
                assert.equal(JSON.parse(requests[0].options.body).indexer_id, 7);
            }
        });
    }
}
