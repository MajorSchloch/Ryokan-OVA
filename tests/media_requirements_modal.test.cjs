// Run with: node --test tests/media_requirements_modal.test.cjs
// These tests exercise the shipped renderer without a browser dependency.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const context = vm.createContext({ Intl, window: {}, document: { body: { addEventListener() {} } } });
const root = path.resolve(__dirname, '..');
for (const name of ['series_helpers.js', 'series_episode_modal.js']) {
    vm.runInContext(fs.readFileSync(path.join(root, 'static/js', name), 'utf8'), context);
}
const status = (changes = {}) => ({
    enabled: true, minimum_met: true, target_met: false,
    resolution: '1080p', minimum_resolution: '576p', target_resolution: '2160p',
    selectable_subtitles_satisfied: false, minimum_subtitle_check: false,
    target_subtitle_check: true, reasons: [], ...changes,
});
const languages = { enabled: true, required_languages: ['de', 'fr'], match_mode: 'any',
    satisfied: false, reasons: [] };

test('disabled requirements preserve the ordinary file details', () => {
    assert.equal(context.renderMediaRequirements(status({ enabled: false }), languages, true), '');
});

test('minimum and target stay separate and use configured resolutions and languages', () => {
    const html = context.renderMediaRequirements(status(), languages, false);
    assert.match(html, /Minimum<\/dt><dd>Passed/);
    assert.match(html, /Good enough target<\/dt><dd>Below target/);
    assert.match(html, /Minimum resolution<\/dt><dd>576p/);
    assert.match(html, /Target resolution<\/dt><dd>2160p/);
    assert.match(html, /Any of: German, French/);
    assert.match(html, /Selectable subtitles are not required at minimum/);
    assert.match(html, /At target<\/dt><dd>Required/);
    assert.match(html, /That switch is off for this release/);
    assert.doesNotMatch(html, /upgrade eligible|English/);
});

test('unknown resolution does not claim minimum failure, target eligibility, or readiness', () => {
    for (const resolution of ['Unknown', 'unknown', '']) {
        const html = context.renderMediaRequirements(status({ resolution, minimum_met: false }), languages, true);
        assert.equal((html.match(/<dd>Unknown<\/dd>/g) || []).length, 2);
        assert.match(html, /does not authorize an automatic replacement/);
        assert.doesNotMatch(html, /<dd>Failed|Below target|upgrade eligible|meets the configured good enough target/);
    }
});

test('minimum subtitle rule still applies at target and policy reasons are escaped', () => {
    const html = context.renderMediaRequirements(status({ minimum_met: false, minimum_subtitle_check: true,
        target_subtitle_check: false, reasons: ['<img src=x onerror=alert(1)>'] }), languages, true);
    assert.match(html, /Minimum<\/dt><dd>Failed/);
    assert.match(html, /At target<\/dt><dd>Required/);
    assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt;/);
    assert.doesNotMatch(html, /<img src=x/);
});

test('file facts remain the source of track claims and burned-in captions stay unverified', () => {
    const section = {};
    context.renderEpisodeMediaInfo(section, {
        media: { container: 'matroska', duration_seconds: 1200,
            video: [{ codec: 'h264', width: 1280, height: 720 }],
            audio: [{ codec: 'aac', language: 'ja' }], subtitles: [], warnings: [] },
        subtitle_requirements: { ...languages, required_languages: ['de'] },
        media_requirements: status({ resolution: '720p' }), upgrades_allowed: true,
    });
    assert.match(section.innerHTML, /Preferred selectable subtitles not verified/);
    assert.match(section.innerHTML, /No selectable subtitle tracks or matching files found/);
    assert.match(section.innerHTML, /Japanese · aac · Embedded/);
    assert.match(section.innerHTML, /Burned-in captions: unverified/);
    assert.doesNotMatch(section.innerHTML, /German · .*Embedded|English/);
});

test('subtitle headline follows minimum, preferred target, and disabled checks', () => {
    const render = (requirements) => {
        const section = {};
        context.renderEpisodeMediaInfo(section, {
            media: { video: [], audio: [], subtitles: [], warnings: [] },
            subtitle_requirements: languages, media_requirements: requirements,
        });
        return section.innerHTML;
    };
    const preferred = render(status());
    assert.match(preferred, /ep-media-verdict-preferred">Preferred selectable subtitles not verified/);
    assert.doesNotMatch(preferred, /ep-media-verdict-missing|Required selectable subtitles missing|Requires any/);
    const strict = render(status({ minimum_subtitle_check: true }));
    assert.match(strict, /ep-media-verdict-missing">Required selectable subtitles missing/);
    const disabled = render(status({ minimum_subtitle_check: false, target_subtitle_check: false }));
    assert.match(disabled, /ep-media-verdict-neutral">No selectable subtitles found/);
    assert.match(disabled, /Not required at minimum or target/);
    assert.doesNotMatch(disabled, /ep-media-verdict-missing|ep-media-verdict-preferred|Required selectable|Preferred selectable|Requires any/);
    const legacy = render(status({ enabled: false }));
    assert.match(legacy, /ep-media-verdict-missing">Required selectable subtitles missing/);
});

test('met target reports completion without promising an upgrade', () => {
    const html = context.renderMediaRequirements(status({ target_met: true, selectable_subtitles_satisfied: true }), languages, true);
    assert.match(html, /Good enough target<\/dt><dd>Met/);
    assert.match(html, /meets the configured good enough target/);
    assert.doesNotMatch(html, /Below target|Automatic upgrades also depend/);
});

test('requested dub is separate from subtitles and uses the effective release languages', () => {
    const section = {};
    context.renderEpisodeMediaInfo(section, {
        media: { video: [{ codec: 'h264', width: 1920, height: 1080 }], audio: [{ codec: 'aac', language: null }], subtitles: [], warnings: [] },
        media_requirements: status({ minimum_met: false, target_met: false, audio_languages_satisfied: false }),
        subtitle_requirements: { enabled: false, satisfied: true, required_languages: [], reasons: [] },
        audio_requirements: { enabled: true, satisfied: false, required_languages: ['de'], match_mode: 'all', reasons: ['Audio language is unknown.'] },
        upgrades_allowed: false,
    });
    assert.match(section.innerHTML, /Required audio<\/dt><dd>All of: German/);
    assert.match(section.innerHTML, /Required dialogue audio not verified/);
    assert.match(section.innerHTML, /Unknown language · aac/);
    assert.doesNotMatch(section.innerHTML, /English|Required dialogue audio present/);
});
