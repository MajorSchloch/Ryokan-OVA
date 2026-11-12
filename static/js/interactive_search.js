// HTMX 4 reports fetch failures through events, then resolves ajax().
// Keep the error and retry state here so every interactive-search entry
// point handles those failures, HTTP errors and superseded requests alike.
(function () {
    if (window.ryokanInteractiveSearch) return;

    var requests = new WeakMap();

    function cancel(target) {
        var request = target && requests.get(target);
        if (!request) return;
        request.cancelled = true;
        requests.delete(target);
        target.removeAttribute('aria-busy');
        if (request.abort) { try { request.abort(); } catch (_) {} }
    }

    function load(target, url, message) {
        if (!target) return Promise.resolve();
        cancel(target);

        // Each request has its own connected source. Reusing the modal
        // body as the source would put rapid menu changes in HTMX's
        // default "queue first", which can drop the latest choice.
        var source = document.createElement('div');
        source.className = 'isearch-loading';
        source.setAttribute('role', 'status');
        var spinner = document.createElement('span');
        spinner.className = 'isearch-loading-spinner';
        spinner.setAttribute('aria-hidden', 'true');
        var label = document.createElement('span');
        label.textContent = message;
        source.append(spinner, label);
        target.replaceChildren(source);
        target.setAttribute('aria-busy', 'true');

        var request = { source: source, ctx: null, abort: null, cancelled: false, finished: false, failed: false };
        requests.set(target, request);

        function current() {
            return !request.cancelled && target.isConnected && requests.get(target) === request;
        }

        function ownContext(event) {
            var ctx = event.detail && event.detail.ctx;
            if (!ctx) return null;
            if (!request.ctx && ctx.sourceElement === source && ctx.target === target) request.ctx = ctx;
            return request.ctx === ctx ? ctx : null;
        }

        function showError() {
            if (!current() || request.finished || request.failed) return;
            request.failed = true;
            target.removeAttribute('aria-busy');
            var error = document.createElement('div');
            error.className = 'isearch-empty';
            error.setAttribute('role', 'alert');
            var title = document.createElement('p');
            title.textContent = 'The search could not finish.';
            var hint = document.createElement('p');
            hint.className = 'isearch-empty-hint';
            hint.textContent = 'Check your connection or try again in a moment.';
            var retry = document.createElement('button');
            retry.type = 'button';
            retry.className = 'btn btn-secondary isearch-retry';
            retry.textContent = 'Try again';
            retry.addEventListener('click', function () { load(target, url, message); });
            error.append(title, hint, retry);
            target.replaceChildren(error);
        }

        function beforeRequest(event) {
            var ctx = ownContext(event);
            if (!ctx) return;
            request.abort = ctx.request && ctx.request.abort;
            if (!current()) event.preventDefault();
        }

        function beforeResponseOrSwap(event) {
            if (ownContext(event) && (!current() || request.failed)) event.preventDefault();
        }

        function error(event) {
            if (ownContext(event)) showError();
        }

        function finish() {
            if (request.finished) return;
            // Also cover a cancelled HTMX pipeline or an empty no-swap
            // response. A completed request must never retain its spinner.
            if (current() && source.parentNode === target) showError();
            request.finished = true;
            if (current()) target.removeAttribute('aria-busy');
            listeners.forEach(function (entry) { document.removeEventListener(entry[0], entry[1]); });
        }

        function finallyRequest(event) {
            if (ownContext(event)) finish();
        }

        var listeners = [
            ['htmx:before:request', beforeRequest],
            ['htmx:before:response', beforeResponseOrSwap],
            ['htmx:after:request', beforeResponseOrSwap],
            ['htmx:before:swap', beforeResponseOrSwap],
            ['htmx:error', error],
            ['htmx:response:error', error],
            ['htmx:finally:request', finallyRequest],
        ];
        listeners.forEach(function (entry) { document.addEventListener(entry[0], entry[1]); });

        try {
            return Promise.resolve(window.htmx.ajax('GET', url, {
                source: source,
                target: target,
                swap: 'innerHTML',
            })).then(finish, function () { showError(); finish(); });
        } catch (_) {
            showError();
            finish();
            return Promise.resolve();
        }
    }

    window.ryokanInteractiveSearch = { load: load, cancel: cancel };
})();
