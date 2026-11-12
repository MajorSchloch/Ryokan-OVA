// Run with: node --test tests/interactive_search_errors.test.cjs
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname, '..');
const readScript = name => fs.readFileSync(path.join(root, 'static/js', name), 'utf8');

class Element extends EventTarget {
    constructor(tag = 'div') {
        super();
        this.tagName = tag.toUpperCase();
        this.children = [];
        this.parentNode = null;
        this.attributes = new Map();
        this.style = {};
        this.className = '';
        this.textContent = '';
        this.connected = false;
    }
    get isConnected() { return this.connected || !!this.parentNode?.isConnected; }
    setAttribute(key, value) { this.attributes.set(key, value); }
    getAttribute(key) { return this.attributes.get(key) ?? null; }
    removeAttribute(key) { this.attributes.delete(key); }
    append(...children) { for (const child of children) { this.children.push(child); child.parentNode = this; } }
    replaceChildren(...children) {
        for (const child of this.children) child.parentNode = null;
        this.children = [];
        this.append(...children);
    }
}

function descend(element, predicate) {
    return predicate(element) ? element : element.children.map(child => descend(child, predicate)).find(Boolean);
}

function environment() {
    const document = new EventTarget();
    document.createElement = tag => new Element(tag);
    document.body = new Element('body');
    document.body.connected = true;
    const elements = new Map();
    document.getElementById = id => elements.get(id) || null;
    document.querySelectorAll = () => [];
    document.querySelector = () => null;
    const calls = [];
    function emit(name, ctx, extra = {}) {
        const event = new Event(name, { cancelable: true });
        event.detail = { ctx, ...extra };
        document.dispatchEvent(event);
        return !event.defaultPrevented;
    }
    const window = { addEventListener() {}, htmx: { ajax(method, url, options) {
        const call = { method, url, options, aborts: 0 };
        call.ctx = { sourceElement: options.source, target: options.target,
            request: { abort() { call.aborts += 1; } } };
        call.promise = new Promise(resolve => { call.resolve = resolve; });
        call.event = (name, extra) => emit(name, call.ctx, extra);
        call.finish = () => { call.event('htmx:finally:request'); call.resolve(); };
        call.fail = () => { call.event('htmx:error', { error: new TypeError('Failed to fetch') }); call.finish(); };
        call.swap = label => {
            if (!call.event('htmx:before:swap')) return false;
            const result = new Element('table');
            result.textContent = label;
            options.target.replaceChildren(result);
            // HTMX repoints a disconnected source after a successful swap.
            call.ctx.sourceElement = options.target;
            return true;
        };
        calls.push(call);
        call.event('htmx:before:request');
        return call.promise;
    } } };
    const context = vm.createContext({ window, document, Promise, WeakMap, AbortController, SD: { id: 74 }, fetch() { return new Promise(() => {}); } });
    vm.runInContext(readScript('interactive_search.js'), context);
    function target(id = 'isearch-body') {
        const element = new Element();
        element.id = id;
        elements.set(id, element);
        document.body.append(element);
        return element;
    }
    return { window, document, context, calls, emit, target, elements, helper: window.ryokanInteractiveSearch };
}

function retry(target) { return descend(target, element => element.tagName === 'BUTTON'); }
function hasSpinner(target) { return !!descend(target, element => element.className === 'isearch-loading-spinner'); }
function alert(target) { return descend(target, element => element.getAttribute('role') === 'alert'); }

test('HTMX error events end the spinner even when ajax resolves; Try again keeps the search', async () => {
    const env = environment(), target = env.target();
    const done = env.helper.load(target, '/episodes/6', 'Searching for episode 6');
    env.calls[0].fail();
    await done;
    assert.ok(alert(target));
    assert.equal(hasSpinner(target), false);
    assert.equal(target.getAttribute('aria-busy'), null);
    assert.equal(retry(target).textContent, 'Try again');
    retry(target).dispatchEvent(new Event('click'));
    assert.equal(env.calls[1].url, '/episodes/6');
    assert.equal(env.calls[1].options.source.children[1].textContent, 'Searching for episode 6');
    assert.notEqual(env.calls[0].options.source, env.calls[1].options.source);
    assert.equal(target.getAttribute('aria-busy'), 'true');
    env.calls[1].swap('Results');
    env.calls[1].finish();
    await env.calls[1].promise;
    assert.equal(target.children[0].textContent, 'Results');
    assert.equal(target.getAttribute('aria-busy'), null);
});

test('HTTP errors show a retry and cannot swap their response body', async () => {
    const env = environment(), target = env.target();
    const done = env.helper.load(target, '/batch', 'Searching batch releases');
    env.calls[0].ctx.response = { status: 502 };
    env.calls[0].ctx.text = '<script>private server error</script>';
    env.calls[0].event('htmx:response:error');
    assert.equal(env.calls[0].swap('private server error'), false);
    env.calls[0].finish();
    await done;
    assert.ok(alert(target));
    assert.ok(retry(target));
    assert.equal(hasSpinner(target), false);
});

test('unrelated HTMX errors do not affect the search or its successful result', async () => {
    const env = environment(), target = env.target();
    const done = env.helper.load(target, '/episode', 'Searching');
    env.emit('htmx:error', { target, sourceElement: new Element() });
    assert.equal(hasSpinner(target), true);
    env.calls[0].swap('Results with working grab controls');
    env.calls[0].finish();
    await done;
    env.emit('htmx:error', env.calls[0].ctx);
    assert.equal(target.children[0].textContent, 'Results with working grab controls');
});

test('an older failure or completion cannot clear the latest busy state or overwrite success', async () => {
    const env = environment(), target = env.target();
    const old = env.helper.load(target, '/old', 'Old search');
    const latest = env.helper.load(target, '/new', 'New search');
    assert.equal(env.calls[0].aborts, 1);
    env.calls[0].fail();
    await old;
    assert.equal(target.getAttribute('aria-busy'), 'true');
    assert.equal(hasSpinner(target), true);
    env.calls[1].swap('New results');
    env.calls[1].finish();
    await latest;
    assert.equal(target.children[0].textContent, 'New results');
});

test('a superseded response is blocked both before reading and after delayed response text', async () => {
    const env = environment(), target = env.target();
    const old = env.helper.load(target, '/old', 'Old search');
    assert.equal(env.calls[0].event('htmx:before:response'), true);
    const latest = env.helper.load(target, '/new', 'New search');
    env.calls[1].swap('New results');
    env.calls[1].finish();
    await latest;
    assert.equal(env.calls[0].event('htmx:before:response'), false);
    assert.equal(env.calls[0].event('htmx:after:request'), false);
    assert.equal(env.calls[0].swap('Stale results'), false);
    env.calls[0].finish();
    await old;
    assert.equal(target.children[0].textContent, 'New results');
});

test('closing a modal cancels its request without showing an error or accepting late results', async () => {
    const env = environment(), target = env.target();
    const done = env.helper.load(target, '/batch', 'Searching');
    env.helper.cancel(target);
    assert.equal(env.calls[0].aborts, 1);
    assert.equal(target.getAttribute('aria-busy'), null);
    assert.equal(env.calls[0].swap('Late results'), false);
    env.calls[0].fail();
    await done;
    assert.equal(alert(target), undefined);
});

test('navigation to a new target with the same id cannot receive an older result', async () => {
    const env = environment(), oldTarget = env.target();
    const old = env.helper.load(oldTarget, '/old', 'Old search');
    oldTarget.parentNode = null;
    const target = env.target();
    const latest = env.helper.load(target, '/new', 'New search');
    assert.equal(env.calls[0].swap('Old results'), false);
    env.calls[0].fail();
    await old;
    assert.equal(hasSpinner(target), true);
    env.calls[1].swap('New page results');
    env.calls[1].finish();
    await latest;
    assert.equal(target.children[0].textContent, 'New page results');
});

test('series and Wanted targets have independent requests and error states', async () => {
    const env = environment(), series = env.target(), wanted = env.target('wanted-isearch-body');
    const first = env.helper.load(series, '/episode', 'Searching episode');
    const second = env.helper.load(wanted, '/wanted', 'Searching wanted episodes');
    env.calls[0].fail();
    await first;
    assert.ok(alert(series));
    assert.equal(hasSpinner(wanted), true);
    env.calls[1].swap('Wanted results');
    env.calls[1].finish();
    await second;
    assert.equal(wanted.children[0].textContent, 'Wanted results');
});

test('an early resolved or rejected ajax pipeline cannot leave a spinner', async () => {
    for (const ajax of [() => Promise.resolve(), () => Promise.reject(new Error('request setup')), () => { throw new Error('missing HTMX'); }]) {
        const env = environment(), target = env.target();
        env.window.htmx.ajax = ajax;
        await env.helper.load(target, '/episode', 'Searching');
        assert.ok(alert(target));
        assert.equal(hasSpinner(target), false);
        assert.equal(target.getAttribute('aria-busy'), null);
    }
});

test('rapid search changes use distinct sources so HTMX cannot queue or drop the latest choice', async () => {
    const env = environment(), target = env.target();
    const requests = [1, 2, 3].map(number => env.helper.load(target, '/episode/' + number, 'Searching ' + number));
    assert.equal(new Set(env.calls.map(call => call.options.source)).size, 3);
    assert.equal(env.calls[2].event('htmx:before:response'), true);
    env.calls[2].swap('Episode 3');
    env.calls[2].finish();
    for (const call of env.calls.slice(0, 2)) { assert.equal(call.swap('Stale'), false); call.finish(); }
    await Promise.all(requests);
    assert.equal(target.children[0].textContent, 'Episode 3');
});

test('a failed abort hook cannot prevent the next search from starting', async () => {
    const env = environment(), target = env.target();
    const first = env.helper.load(target, '/old', 'Old search');
    env.calls[0].ctx.request.abort = () => { throw new Error('extension abort failure'); };
    env.calls[0].event('htmx:before:request');
    const latest = env.helper.load(target, '/new', 'New search');
    assert.equal(env.calls.length, 2);
    env.calls[1].swap('New results');
    env.calls[1].finish();
    env.calls[0].fail();
    await Promise.all([first, latest]);
    assert.equal(target.children[0].textContent, 'New results');
});

test('series episode and batch entry points share error handling and cancel on close', () => {
    const env = environment(), target = env.target();
    const modal = env.target('isearch-modal'), title = env.target('isearch-title');
    const loads = [], cancels = [];
    env.window.ryokanInteractiveSearch = { load: (...args) => loads.push(args), cancel: element => cancels.push(element) };
    vm.runInContext(readScript('series_interactive_search.js'), env.context);
    env.context.openInteractiveSearch(6);
    env.context.openInteractiveBatchSearch();
    assert.equal(loads[0][0], target);
    assert.equal(loads[0][1], '/api/series/74/interactive-search/6');
    assert.equal(loads[1][1], '/api/series/74/interactive-search-batch');
    assert.equal(modal.style.display, 'flex');
    assert.equal(title.textContent, 'Interactive Batch Search');
    env.context.closeInteractiveSearch({ target: title });
    assert.equal(cancels.length, 0, 'a click inside the modal must not close it');
    env.context.closeInteractiveSearch();
    assert.equal(cancels[0], target);
    assert.equal(modal.style.display, 'none');
});

test('Wanted single, episode-set and batch choices share the same error handling and cancellation', () => {
    const env = environment(), target = env.target('wanted-isearch-body');
    const modal = env.target('wanted-isearch-modal');
    const loads = [], cancels = [];
    env.window.ryokanInteractiveSearch = { load: (...args) => loads.push(args), cancel: element => cancels.push(element) };
    const source = readScript('wanted.js').replace("    document.addEventListener('click',",
        "    window.testWanted = { pick: pickWantedChoice, close: closeWantedInteractive, state: isearch };\n    document.addEventListener('click',");
    vm.runInContext(source, env.context);
    env.window.testWanted.state.seriesId = 74;
    env.window.testWanted.state.episodes = [1, 2];
    for (const kind of ['episode', 'episodes', 'batch']) {
        env.window.testWanted.pick({ closest() { return null; }, textContent: kind,
            getAttribute(name) { return { 'data-isearch-choice': kind, 'data-episode': '2', 'data-episodes': '1,2', 'data-label': 'Episodes 1 and 2' }[name] || null; } });
    }
    assert.equal(loads.length, 3);
    assert.ok(loads.every(args => args[0] === target));
    assert.equal(loads[0][1], '/api/series/74/interactive-search/2?from=wanted');
    assert.equal(loads[1][1], '/api/series/74/interactive-search-episodes?from=wanted&episodes=1%2C2');
    assert.equal(loads[2][1], '/api/series/74/interactive-search-batch?from=wanted');
    env.window.testWanted.close();
    assert.equal(cancels[0], target);
    assert.equal(modal.style.display, 'none');
});
