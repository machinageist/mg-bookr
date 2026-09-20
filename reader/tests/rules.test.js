// Run: node --test reader/tests/rules.test.js
// Pins where the reader thinks it is, and what it injects into a page
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const { test } = require('node:test');
const R = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, '..', 'Rules.js'), 'utf8'), R);

const plain = value => JSON.parse(JSON.stringify(value));
const theme = R.palette('{"name":"fog","base":"#1d2124","fg":"#dde2e5","accent":"#9bb3bf"}');

test('a stored location becomes a place in the book, however odd it is', () => {
    assert.deepEqual(plain(R.place('3:0.42', 10)), { index: 3, fraction: 0.42 });
    assert.deepEqual(plain(R.place('99:0.5', 10)), { index: 9, fraction: 0.5 }, 'no further than the book goes');
    assert.deepEqual(plain(R.place('', 10)), { index: 0, fraction: 0 });
    assert.deepEqual(plain(R.place('nowhere', 10)), { index: 0, fraction: 0 });
    assert.deepEqual(plain(R.place('2:9', 10)), { index: 2, fraction: 1 });
    assert.equal(R.page('7', 20), 7);
    assert.equal(R.page('0', 20), 1);
    assert.equal(R.page('900', 20), 20);
});

test('locations and percentages match what the store keeps', () => {
    assert.equal(R.location('epub', 3, 0.4212345), '3:0.4212');
    assert.equal(R.location('comic', 7, 0), '7');
    assert.equal(R.percent('epub', 1, 0.5, 4), 37.5);
    assert.equal(R.percent('comic', 10, 0, 20), 50);
    assert.equal(R.percent('epub', 0, 0, 0), 0, 'an empty book is not a division by zero');
});

test('the chapter is the last table-of-contents entry at or before the page', () => {
    const toc = [
        { title: 'Cover', spine: 0 },
        { title: 'One', spine: 2 },
        { title: 'Two', spine: 5 },
        { title: 'Notes', spine: null }
    ];
    assert.equal(R.chapterOf(toc, 0), 'Cover');
    assert.equal(R.chapterOf(toc, 4), 'One');
    assert.equal(R.chapterOf(toc, 9), 'Two');
    assert.equal(R.chapterOf([], 3), '');
});

test('the page is dressed in the desktop palette, and columns only when paging', () => {
    const paged = R.css(theme, 'pages', 18);
    assert.ok(paged.includes('#1d2124') && paged.includes('#dde2e5'));
    assert.ok(paged.includes('column-width'), 'paging is columns the width of the window');
    assert.ok(paged.includes('34em'), 'a line never runs past a comfortable measure');
    assert.ok(paged.includes('column-gap: calc(2 * max('), 'column plus gap is one screenful, so a page turn lands square');
    assert.ok(paged.includes('font-size: 18px'));
    assert.ok(!R.css(theme, 'scroll', 18).includes('column-width'));
    assert.ok(R.css(theme, 'scroll', 99).includes('font-size: 32px'), 'the size has a ceiling');
    assert.equal(R.clampFont(2), 10);
    assert.equal(R.clampFont(18.4), 18);
});

test('the injected script answers the window and keeps to its own name', () => {
    const script = R.pageScript(theme, 'pages', 18);
    for (const call of ['progress', 'goto', 'step', 'advance', 'selection']) {
        assert.ok(script.includes(call + ':'), call);
    }
    assert.ok(script.startsWith('(function () {') && script.trim().endsWith('})()'), 'one expression');
    assert.ok(script.includes('window.__mgReader'), 'one name on the page');
    assert.ok(script.includes('mg-reader-style'), 'its stylesheet is replaced, not piled up');
});

test('answers that are refusals or rubbish come back as nothing', () => {
    assert.equal(R.answer('{"ok":false,"error":"no book 9"}'), null);
    assert.equal(R.answer('not json'), null);
    assert.equal(R.answer(''), null);
    assert.equal(R.answer('{"ok":true,"mode":"scroll"}').mode, 'scroll');
});

test('the palette is filled in wherever the shell was silent', () => {
    assert.equal(theme.base, '#1d2124');
    assert.equal(theme.surface, '#21222c', 'a colour the shell did not send');
    assert.ok(theme.bookFont.includes('serif'), 'a book is read in a serif face');
    const none = R.palette('');
    assert.equal(none.name, 'dracula');
    assert.equal(none.dark, true);
    assert.equal(R.palette('{"base":5}').base, '#282a36', 'a number is not a colour');
});
