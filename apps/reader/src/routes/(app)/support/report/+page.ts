// The report form is interactive and per-visitor (it reads the session to label the
// submission), so it stays CLIENT-ONLY — unlike the static /support page above it. There
// is nothing to server-render but an empty form.
//
// No `load` at all, and deliberately no `setHeaders`: with `ssr = false` the load would
// only ever run in the browser, where `setHeaders` is a documented no-op — writing one
// here would look like cache control and do nothing.
export const ssr = false;
