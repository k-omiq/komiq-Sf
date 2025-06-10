// Default rendering mode: pure client-side SPA.
//  - required by Tauri (desktop + mobile) which serves static assets, and
//  - user-specific / interactive routes (library, profile, login, reader) have
//    nothing to gain from SSR and touch browser-only state, so they stay here.
//
// The hosted web build (Cloudflare) opts the *public* catalog pages back into
// edge SSR by overriding this per route (`export const ssr = SSR_PUBLIC`): home,
// browse, series/[slug], updates, support. A child page's value overrides the
// layout's, so those render on the server on web while everything else — and the
// entire Tauri/static build — stays client-only.
export const ssr = false;
export const prerender = false;
