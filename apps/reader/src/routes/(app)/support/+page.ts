import { SSR_PUBLIC } from '$lib/ssr';

// Public support page — server-rendered on the web build. There is no data load: the
// copy is static module content (`$lib/data/content`), and the one interactive thing
// here — the report form — lives on /support/report. See $lib/ssr.
// The hosted Worker runs only on /series/* (apps/reader/wrangler.toml), so the hosted
// site serves this route as the static shell: no edge cache time is set here.
export const ssr = SSR_PUBLIC;
