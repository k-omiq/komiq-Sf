// No-FOUC theme: set data-theme before first paint.
try {
	var p = localStorage.getItem('komika-theme');
	var dark =
		p === 'dark' || ((!p || p === 'system') && matchMedia('(prefers-color-scheme: dark)').matches);
	document.documentElement.dataset.theme = dark ? 'dark' : 'light';
} catch (e) {}

// The colour palette and the pure-black background, from the same key
// $lib/theme.svelte.ts writes. `seed` is absent for the default palette.
try {
	var look = JSON.parse(localStorage.getItem('komiq-look') || '{}') || {};
	if (/^#[0-9a-f]{6}$/i.test(look.seed)) {
		document.documentElement.dataset.palette = look.palette || '';
		document.documentElement.style.setProperty('--k-seed', look.seed);
	}
	if (look.black === true) document.documentElement.dataset.black = '';
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
