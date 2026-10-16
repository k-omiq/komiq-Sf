import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { runInNewContext } from 'node:vm';

const template = readFileSync(new URL('./app.html', import.meta.url), 'utf8');
const [, buildVersion, script] = template.match(
	/<script id="komiq-preload-recovery" data-version="([^"]+)">([\s\S]*?)<\/script>/,
);

function browser(storage = new Map(), storageBlocked = false, version = 'build-1') {
	let reloads = 0;
	let prevented = 0;
	const listeners = new Map();
	const elements = [];
	function element(tag) {
		const result = {
			tag,
			id: '',
			style: {},
			children: [],
			attributes: {},
			listeners: {},
			focused: false,
			setAttribute(name, value) {
				this.attributes[name] = value;
			},
			append(...children) {
				this.children.push(...children);
			},
			addEventListener(name, callback) {
				this.listeners[name] = callback;
			},
			focus() {
				this.focused = true;
			},
		};
		elements.push(result);
		return result;
	}
	const body = element('body');
	const window = {
		addEventListener: (name, callback, options) => listeners.set(name, { callback, options }),
		removeEventListener: (name, callback) => {
			if (listeners.get(name)?.callback === callback) listeners.delete(name);
		},
		location: { reload: () => reloads++ },
	};
	runInNewContext(script, {
		window,
		sessionStorage: {
			getItem(key) {
				if (storageBlocked) throw new Error('Blocked');
				return storage.get(key);
			},
			setItem(key, value) {
				if (storageBlocked) throw new Error('Blocked');
				storage.set(key, value);
			},
			removeItem(key) {
				if (storageBlocked) throw new Error('Blocked');
				storage.delete(key);
			},
		},
		document: {
			currentScript: { dataset: { version } },
			body,
			createElement: element,
			getElementById: (id) => elements.find((el) => el.id === id),
		},
	});
	function fire(name) {
		const listener = listeners.get(name);
		if (!listener) return;
		if (listener.options?.once) listeners.delete(name);
		listener.callback({ preventDefault: () => prevented++ });
	}
	return {
		fire: () => fire('vite:preloadError'),
		rejectBootstrap: () => fire('unhandledrejection'),
		started: () => fire('komiq:started'),
		body,
		get reloads() {
			return reloads;
		},
		get prevented() {
			return prevented;
		},
	};
}

test('preload failure reloads once across documents, then offers a focused reload button', () => {
	const storage = new Map();
	const first = browser(storage);
	first.fire();
	first.fire();
	assert.equal(first.reloads, 1);
	assert.equal(first.prevented, 2);
	assert.equal(first.body.children.length, 0);

	const next = browser(storage);
	next.fire();
	next.fire();
	assert.equal(next.reloads, 0);
	assert.equal(next.body.children.length, 1);
	const panel = next.body.children[0];
	assert.equal(panel.attributes.role, 'alert');
	assert.match(panel.children[0].textContent, /Couldn’t load/);
	const button = panel.children[2];
	assert.equal(button.textContent, 'Reload');
	assert.equal(button.focused, true);
	button.listeners.click();
	assert.equal(next.reloads, 1);
});

test('unavailable sessionStorage shows recovery without risking a reload loop', () => {
	const page = browser(new Map(), true);
	page.fire();
	assert.equal(page.reloads, 0);
	assert.equal(page.body.children.length, 1);
});

test('bare bootstrap import rejections share the Vite reload guard and error panel', () => {
	const storage = new Map();
	const first = browser(storage);
	first.rejectBootstrap();
	first.fire();
	assert.equal(first.reloads, 1);
	assert.equal(first.prevented, 2);

	const retry = browser(storage);
	retry.rejectBootstrap();
	retry.fire();
	assert.equal(retry.reloads, 0);
	assert.equal(retry.body.children.length, 1);
	assert.equal(retry.body.children[0].attributes.role, 'alert');
});

test('bootstrap rejection with blocked storage offers a manual reload', () => {
	const page = browser(new Map(), true);
	page.rejectBootstrap();
	assert.equal(page.reloads, 0);
	assert.equal(page.prevented, 1);
	assert.equal(page.body.children[0].children[2].textContent, 'Reload');
});

test('deferred import failures after root mount reload only once for the same build', () => {
	const storage = new Map();
	const first = browser(storage);
	first.started();
	first.fire();
	assert.equal(first.reloads, 1);

	for (let attempt = 0; attempt < 3; attempt++) {
		const retry = browser(storage);
		retry.started();
		retry.fire();
		assert.equal(retry.reloads, 0);
		assert.equal(retry.body.children.length, 1);
		assert.equal(storage.get('komiq-preload-reloaded:build-1'), '1');
	}
});

test('a new SvelteKit build gets its own reload guard without clearing older guards', () => {
	assert.equal(buildVersion, '%sveltekit.version%');
	const storage = new Map([['komiq-preload-reloaded:build-1', '1']]);
	const nextBuild = browser(storage, false, 'build-2');
	nextBuild.started();
	nextBuild.fire();
	assert.equal(nextBuild.reloads, 1);
	assert.equal(storage.get('komiq-preload-reloaded:build-1'), '1');
	assert.equal(storage.get('komiq-preload-reloaded:build-2'), '1');
	const retry = browser(storage, false, 'build-2');
	retry.started();
	retry.fire();
	assert.equal(retry.reloads, 0);
	assert.equal(retry.body.children.length, 1);
});

test('a root mount after a startup failure does not clear the reload-loop guard', () => {
	const storage = new Map([['komiq-preload-reloaded:build-1', '1']]);
	const page = browser(storage);
	page.fire();
	page.started();
	assert.equal(storage.get('komiq-preload-reloaded:build-1'), '1');
	page.fire();
	assert.equal(page.reloads, 0);
	assert.equal(page.body.children.length, 1);
});

test('unrelated promise rejections after startup do not trigger app recovery', () => {
	const page = browser();
	page.started();
	page.rejectBootstrap();
	assert.equal(page.prevented, 0);
	assert.equal(page.reloads, 0);
	assert.equal(page.body.children.length, 0);
});
