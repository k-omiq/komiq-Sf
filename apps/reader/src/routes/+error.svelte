<script lang="ts">
	import { page } from '$app/state';

	// Root error boundary — covers routes outside the (app) group (e.g. the
	// reader). Standalone (no Header) so it renders even if layout data fails.
	const status = $derived(page.status);
	const is404 = $derived(status === 404);
	const heading = $derived(is404 ? 'Page not found' : 'Something went wrong');
	const message = $derived(
		is404
			? 'The page you’re looking for doesn’t exist or has moved.'
			: (page.error?.message ?? 'An unexpected error occurred.'),
	);
</script>

<div class="err">
	<div class="err-code">{status}</div>
	<h1>{heading}</h1>
	<p>{message}</p>
	<a class="err-primary" href="/">Go home</a>
</div>

<style>
	.err {
		min-height: 100vh;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 12px;
		padding: 40px 20px;
		text-align: center;
	}
	.err-code {
		font-family: var(--k-font-mono);
		font-size: 13px;
		letter-spacing: 0.2em;
		color: var(--k-text-faint);
	}
	h1 {
		font-size: 34px;
		margin: 0;
		color: var(--k-text-bright);
	}
	p {
		margin: 0;
		max-width: 380px;
		font-size: 14.5px;
		line-height: 1.6;
		color: var(--k-text-dim);
	}
	.err-primary {
		margin-top: 12px;
		height: 46px;
		padding: 0 24px;
		display: inline-flex;
		align-items: center;
		border-radius: 8px;
		background: var(--k-primary);
		color: var(--k-on-primary);
		font-weight: 700;
		font-size: 14px;
		text-decoration: none;
	}
</style>
