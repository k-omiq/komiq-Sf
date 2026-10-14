// No-FOUC theme: set data-theme before first paint.
try {
	var p = localStorage.getItem('komika-theme');
	var dark =
		p === 'dark' || ((!p || p === 'system') && matchMedia('(prefers-color-scheme: dark)').matches);
	document.documentElement.dataset.theme = dark ? 'dark' : 'light';
} catch (e) {}

// The background, the accent and the pure-black background, from the same key
// $lib/theme.svelte.ts writes: `attrs` are data attributes, `vars` CSS variables.
// An older version saved one palette colour as `seed`; it tinted and was the accent.
try {
	var root = document.documentElement;
	var look = JSON.parse(localStorage.getItem('komiq-look') || '{}') || {};
	var attrs = look.attrs;
	var vars = look.vars;
	if (look.v !== 2 && /^#[0-9a-f]{6}$/i.test(look.seed)) {
		attrs = { tint: 'solid', accent: '' };
		vars = { '--k-tint-a': look.seed, '--k-tint-b': look.seed, '--k-tint-mid': look.seed };
		vars['--k-tint-angle'] = '160deg';
		vars['--k-tint'] = '0.45';
		vars['--k-accent-seed'] = look.seed;
	}
	for (var a in attrs || {}) if (a === 'tint' || a === 'accent') root.dataset[a] = String(attrs[a]);
	for (var v in vars || {}) {
		if (/^--k-(tint|accent-seed)/.test(v)) root.style.setProperty(v, String(vars[v]));
	}
	if (look.black === true) root.dataset.black = '';
	// MangaDex ratings turned off on this device (see $lib/components/Ratings.svelte).
	if (look.md === false) root.dataset.mdRatings = 'off';
} catch (e) {}

// Native app (Tauri): mark <html> before first paint so app.css can drop the
// browser-page behaviours (see "native app shell" there), and stop the WebView
// zooming the whole app like a web page. The chapter reader re-allows zooming in on
// its pages (see $lib/native). A browser keeps its zoom: this runs in the app only.
try {
	if ('__TAURI_INTERNALS__' in window) {
		document.documentElement.dataset.native = '';
		var vp = document.querySelector('meta[name="viewport"]');
		if (vp) vp.content += ', minimum-scale=1, maximum-scale=1, user-scalable=no';
	}
} catch (e) {}
