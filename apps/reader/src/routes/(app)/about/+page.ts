import { SSR_PUBLIC } from '$lib/ssr';

// Public, entirely static copy — server-rendered on the web build (it is the page rights
// holders and search engines land on). See $lib/ssr.
// The hosted Worker runs only on /series/* (apps/reader/wrangler.toml), so the hosted
// site serves this route as the static shell: no edge cache time is set here.
export const ssr = SSR_PUBLIC;
