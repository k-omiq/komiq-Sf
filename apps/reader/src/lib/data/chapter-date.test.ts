/**
 * Run: node --test --experimental-strip-types src/lib/data/chapter-date.test.ts
 * (from apps/reader; Node 22 strips the types natively).
 */
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { chapterDateLabel, chapterFoundLabel } from './chapter-date.ts';

const NOW = Date.parse('2026-09-05T12:00:00.000Z');

function before(milliseconds: number): string {
	return new Date(NOW - milliseconds).toISOString();
}

test('chapter dates use natural relative prose within 30 days', () => {
	assert.equal(chapterDateLabel(before(30_000), NOW), 'just now');
	assert.equal(chapterDateLabel(before(60_000), NOW), '1 minute ago');
	assert.equal(chapterDateLabel(before(3 * 3_600_000), NOW), '3 hours ago');
	assert.equal(chapterDateLabel(before(12 * 86_400_000), NOW), '12 days ago');
});

test('older chapter dates use an absolute month and year', () => {
	assert.equal(chapterDateLabel('2023-03-15T12:00:00.000Z', NOW, 'en-US'), 'March 2023');
});

test('Found is shown for the newest three even when older than 30 days', () => {
	assert.equal(chapterFoundLabel('2023-03-15T12:00:00.000Z', true, NOW, 'en-US'), 'March 2023');
});

test('Found is shown for any chapter discovered within the last 30 days', () => {
	assert.equal(chapterFoundLabel(before(29 * 86_400_000), false, NOW), '29 days ago');
	assert.notEqual(chapterFoundLabel(before(30 * 86_400_000), false, NOW), '');
});

test('Found is omitted outside the window and never invents a placeholder', () => {
	assert.equal(chapterFoundLabel(before(30 * 86_400_000 + 1), false, NOW), '');
	assert.equal(chapterFoundLabel(null, true, NOW), '');
	assert.equal(chapterFoundLabel('not-a-date', true, NOW), '');
});
