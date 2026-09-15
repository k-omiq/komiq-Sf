// No-FOUC theme: set data-theme before first paint.
try {
	var p = localStorage.getItem('komika-theme');
	var dark =
		p === 'dark' || ((!p || p === 'system') && matchMedia('(prefers-color-scheme: dark)').matches);
	document.documentElement.dataset.theme = dark ? 'dark' : 'light';
} catch (e) {}
