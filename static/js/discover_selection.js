/* Keep the review in sync with the actual form controls, including inputs outside the form. */
(() => {
    const formId = 'add-anime-selected';

    function refresh() {
        const form = document.getElementById(formId);
        const review = document.getElementById('discover-selection-review');
        if (!form || !review) return;

        const picks = [...form.elements].filter(input => input.name === 'release' && input.checked);
        const items = picks.map(input => {
            const row = input.closest('.anilist-result');
            const item = document.createElement('li');
            const title = document.createElement('a');
            title.href = `#${row.id}`;
            title.textContent = row.querySelector('.anilist-title-link').textContent.trim();
            const facts = document.createElement('small');
            facts.textContent = row.dataset.reviewFacts;
            item.append(title, facts);
            return item;
        });
        const count = picks.length;
        review.querySelector('[data-discover-selection-count]').textContent = `${count} release${count === 1 ? '' : 's'} selected`;
        const list = review.querySelector('ul');
        list.replaceChildren(...items);
        list.hidden = count === 0;
        review.querySelector('.form-hint').hidden = count !== 0;
        review.hidden = false;
        document.querySelector('[data-discover-review-link]').hidden = false;
    }

    function onChange(event) {
        if (event.target.name === 'release' && event.target.form?.id === formId) refresh();
    }

    function revealRelease(hash) {
        if (hash.startsWith('#discover-release-')) {
            const target = document.getElementById(hash.slice(1));
            const details = target?.closest('.discover-storyline-releases');
            if (details) details.open = true;
        }
    }

    function onLink(event) {
        const link = event.target.closest('a[href^="#discover-release-"]');
        if (link) revealRelease(link.hash);
    }

    window.ryokanRegisterPageInit('discover-selection', {
        check: () => document.getElementById(formId) || document.querySelector('.discover-storyline-section'),
        mount() {
            document.addEventListener('change', onChange);
            document.addEventListener('click', onLink);
            window.addEventListener('pageshow', refresh);
            revealRelease(location.hash);
            refresh();
        },
        unmount() {
            document.removeEventListener('change', onChange);
            document.removeEventListener('click', onLink);
            window.removeEventListener('pageshow', refresh);
        },
    });
})();
