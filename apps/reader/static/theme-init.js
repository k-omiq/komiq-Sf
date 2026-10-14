// No-FOUC theme: before first paint, show the saved look of the theme in use, from
// the `paint` entries $lib/theme.svelte.ts saves (`data` attributes, CSS `vars`).
// Nothing saved yet, or an older format: the tokens' own light or dark theme, until
// the app starts and converts what was saved.
try {
	var root = document.documentElement;
	var p = localStorage.getItem('komika-theme');
	var mode =
		p === 'dark' || ((!p || p === 'system') && matchMedia('(prefers-color-scheme: dark)').matches)
			? 'dark'
			: 'light';
	root.dataset.theme = mode;
	var look = JSON.parse(localStorage.getItem('komiq-look') || '{}') || {};
	var paint = look.v === 3 && look.paint && look.paint[mode];
	if (paint) {
		for (var d in paint.data || {}) {
			if (d === 'theme' || d === 'tint' || d === 'accent') root.dataset[d] = String(paint.data[d]);
		}
		for (var v in paint.vars || {}) {
			if (/^--k-[a-z0-9-]+$/.test(v)) root.style.setProperty(v, String(paint.vars[v]));
		}
	}
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
