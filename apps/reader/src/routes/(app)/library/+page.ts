import type { PageLoad } from './$types';
import { getLibrary } from '$lib/data/source';

// The library is user-specific and never server-rendered (the app defaults to
// `ssr = false`, see routes/+layout.ts), so awaiting here bought nothing — it just
// blocked the navigation. Clicking "Library" left the PREVIOUS page on screen with
// no spinner, no skeleton and no indication anything was happening, for as long as
// the round trip took. Hand the page the promise instead and let it render its own
// loading state. `getLibrary()` never rejects — it resolves to an empty library on
// error.
export const load: PageLoad = () => ({ library: getLibrary() });
