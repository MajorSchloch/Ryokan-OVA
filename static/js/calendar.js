// Calendar controls, local episode times, and the iCal subscription dialog.

(function () {
    if (!document.getElementById('calendar-list')) return;

    function fmtTime(unixTs) {
        try {
            var d = new Date(unixTs * 1000);
            return d.toLocaleTimeString(undefined, {
                hour: 'numeric',
                minute: '2-digit',
            });
        } catch (_) {
            return '';
        }
    }

    if (window.htmx && !window.ryokanCalendarFilterControls) {
        window.ryokanCalendarFilterControls = true;
        document.addEventListener('change', function (event) {
            var select = event.target;
            if (!select.matches('[data-calendar-filter-select]')) return;
            var fields = new URLSearchParams(new FormData(select.form));
            var current = new URLSearchParams(window.location.search);
            var title = fields.get('search').trim();
            if (title) fields.set('search', title);
            else fields.delete('search');
            if (fields.get('status') === 'all') fields.delete('status');
            var monitoring = fields.get('monitoring');
            if (monitoring === 'all') fields.delete('monitoring');
            else if (monitoring === 'monitored' && current.get('monitored') === 'true') {
                fields.delete('monitoring');
                fields.set('monitored', 'true');
            }
            window.htmx.ajax('GET', '/calendar?' + fields, {
                target: '#calendar-list', swap: 'innerHTML', push: 'true',
            });
        });
    }

    // ── iCal subscribe modal ────────────────────────────────────

    var icalBtn = document.getElementById('calendar-ical-btn');
    var modal = document.getElementById('calendar-subscribe-modal');
    var keyPicker = document.getElementById('calendar-subscribe-key');
    var urlInput = document.getElementById('calendar-subscribe-url');
    var copyBtn = document.getElementById('calendar-subscribe-copy-btn');
    var copyConfirm = document.getElementById('calendar-subscribe-copy-confirm');

    function refreshSubscribeUrl() {
        if (!keyPicker || !urlInput) return;
        var keyId = keyPicker.value;
        if (!keyId) {
            urlInput.value = '';
            return;
        }
        urlInput.value = 'Loading...';
        fetch('/api/api-keys/' + keyId + '/reveal', { credentials: 'same-origin' })
            .then(function (r) {
                if (!r.ok) throw new Error('reveal failed');
                return r.json();
            })
            .then(function (data) {
                if (!data || !data.plaintext) {
                    urlInput.value = '(failed to load key)';
                    return;
                }
                urlInput.value = window.location.origin
                    + '/api/calendar.ics?apikey='
                    + encodeURIComponent(data.plaintext);
            })
            .catch(function () {
                urlInput.value = '(failed to load key)';
            });
    }

    if (icalBtn && modal) {
        icalBtn.addEventListener('click', function () {
            modal.style.display = 'flex';
            refreshSubscribeUrl();
        });
    }
    if (keyPicker) keyPicker.addEventListener('change', refreshSubscribeUrl);

    document.querySelectorAll('[data-modal-close="calendar-subscribe-modal"]').forEach(function (el) {
        el.addEventListener('click', function () {
            if (modal) modal.style.display = 'none';
        });
    });
    if (modal) {
        modal.addEventListener('click', function (ev) {
            if (ev.target === modal) modal.style.display = 'none';
        });
    }

    if (copyBtn && urlInput) {
        copyBtn.addEventListener('click', function () {
            if (!urlInput.value || urlInput.value.startsWith('(')) return;
            navigator.clipboard.writeText(urlInput.value).then(function () {
                if (copyConfirm) {
                    copyConfirm.hidden = false;
                    setTimeout(function () { copyConfirm.hidden = true; }, 2000);
                }
            }, function () {
                urlInput.select();
                urlInput.setSelectionRange(0, 99999);
            });
        });
    }

    // ── Per-cell time hydration ─────────────────────────────────
    // Server renders `·` as a placeholder for both the list view
    // (`.calendar-episode-time`) and the grid view's pills
    // (`.calendar-month-pill-time`). We hydrate both shapes to
    // the user's local-time format on first paint and after every
    // HTMX swap.
    function hydrateTimes(root) {
        var sel = '.calendar-episode-time[data-airing-at], .calendar-month-pill-time[data-airing-at]';
        (root || document).querySelectorAll(sel).forEach(function (el) {
            var ts = parseInt(el.dataset.airingAt, 10);
            if (ts > 0) el.textContent = fmtTime(ts);
        });
    }

    // ── Today highlight + scroll-to-today ───────────────────────
    // Both views key by `data-day-key` (UTC midnight). List view
    // gets `.calendar-day-today`; grid view gets
    // `.calendar-month-cell-today`. Server-side render already
    // sets `is_today` on the right cell, so this is mainly the
    // post-swap re-apply.
    function applyTodayHighlight(root) {
        var now = Math.floor(Date.now() / 1000);
        var todayUtcMidnight = now - (((now % 86400) + 86400) % 86400);
        var todayEl = null;
        (root || document).querySelectorAll('.calendar-day, .calendar-month-cell').forEach(function (el) {
            var dk = parseInt(el.dataset.dayKey, 10);
            var isToday = dk === todayUtcMidnight;
            if (el.classList.contains('calendar-day')) {
                el.classList.toggle('calendar-day-today', isToday);
            } else {
                el.classList.toggle('calendar-month-cell-today', isToday);
            }
            if (isToday && !todayEl) todayEl = el;
        });
        return todayEl;
    }

    function hydrateCalendar() {
        var root = document.getElementById('calendar-list');
        if (!root) return;
        hydrateTimes(root);
        applyTodayHighlight(root);
        var period = root.querySelector('.calendar-period');
        if (period) root.dataset.viewMode = period.dataset.range === 'month' ? 'grid' : 'list';
    }

    hydrateCalendar();
    if (window.htmx && !window.ryokanCalendarHydration) {
        window.ryokanCalendarHydration = true;
        window.htmx.onLoad(hydrateCalendar);
    }
})();
