// Whether the public catalog routes are server-rendered.
//
// The reader defaults to a pure client-side SPA (see routes/+layout.ts): required
// by Tauri and by any server-less static host. The hosted web build on Cloudflare
// opts the *public, unauthenticated* pages into edge SSR for fresh content and a
// fast first paint, while user-specific and interactive routes (library, profile,
// login, reader) stay client-only.
//
// `__KOMIKA_SSR__` is a build-time constant replaced by Vite (`define` in
// vite.config.ts): true for the web/Cloudflare build, false for the Tauri/static
// build — so the same source produces the right rendering mode per target.
export const SSR_PUBLIC: boolean = __KOMIKA_SSR__;
