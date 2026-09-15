<script lang="ts">
	import { apiOrigin } from '$lib/config';
	import { answerExtensionInstall, extensionConsent } from '$lib/extension-consent.svelte';
	import { modalFocus } from '$lib/modal-focus';

	// The server named an extension for this device to install. Shown over everything,
	// one question at a time. See `extension-consent.svelte.ts`.
	const first = $derived(extensionConsent.pending[0]);
	const server = $derived(apiOrigin.replace(/^https?:\/\//, ''));
</script>

{#if first}
	{#key first.key}
		<div
			class="scrim-wrap"
			use:modalFocus
			role="alertdialog"
			aria-modal="true"
			aria-labelledby="ext-consent-title"
			aria-describedby="ext-consent-body"
		>
			<div class="scrim"></div>
			<div class="panel">
				<h2 id="ext-consent-title">Install an extension?</h2>
				<div id="ext-consent-body" class="body">
					<p>
						The server <strong>{server}</strong> asks this app to install an extension. Then this device
						gets pages from the source itself.
					</p>
					<dl>
						<dt>Extension</dt>
						<dd>{first.ask.pkgName}</dd>
						<dt>From</dt>
						<dd>{first.ask.repoUrl ?? 'A repository this app already uses'}</dd>
					</dl>
					<p>
						An extension is code that runs on this device. Allow it only if you trust the person who
						runs this server.
					</p>
					{#if first.ask.repoUrl}
						<p>Your answer also applies to other extensions from this repository.</p>
					{/if}
					<p class="dim">If you do not allow it, the server gets the pages for you.</p>
				</div>
				<footer>
					<button class="ghost k-hit" onclick={() => answerExtensionInstall(false)}>Not now</button>
					<button class="primary" onclick={() => answerExtensionInstall(true)}>Allow</button>
				</footer>
			</div>
		</div>
	{/key}
{/if}

<style>
	.scrim-wrap {
		position: fixed;
		inset: 0;
		z-index: 400;
		display: flex;
		align-items: center;
		justify-content: center;
		padding: 24px;
	}
	.scrim {
		position: fixed;
		inset: 0;
		background: rgba(6, 6, 7, 0.82);
		backdrop-filter: blur(6px);
	}
	.panel {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: 14px;
		width: min(460px, 100%);
		max-height: 100%;
		overflow-y: auto;
		overscroll-behavior: contain;
		padding: 22px;
		background: var(--k-surface);
		border-radius: 16px;
	}
	h2 {
		margin: 0;
		font-family: var(--k-font-display);
		font-size: 1.05rem;
		color: var(--k-text-bright);
	}
	.body {
		display: flex;
		flex-direction: column;
		gap: 12px;
		font-size: 0.88rem;
		color: var(--k-text-1);
	}
	p {
		margin: 0;
	}
	.dim {
		color: var(--k-text-dim);
	}
	dl {
		display: grid;
		grid-template-columns: auto 1fr;
		gap: 6px 12px;
		margin: 0;
		padding: 12px;
		border-radius: 10px;
		background: var(--k-surface-2);
		font-size: 0.82rem;
	}
	dt {
		color: var(--k-text-dim);
	}
	dd {
		margin: 0;
		min-width: 0;
		overflow-wrap: anywhere;
		font-family: var(--k-font-mono, ui-monospace, monospace);
	}
	footer {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
	}
	.ghost,
	.primary {
		padding: 8px 14px;
		border: none;
		border-radius: 9px;
		background: var(--k-surface-2);
		color: var(--k-text-1);
		font: inherit;
		font-weight: 600;
		cursor: pointer;
	}
	.primary {
		background: var(--k-primary);
		color: var(--k-on-primary);
	}
</style>
