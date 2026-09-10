<script lang="ts">
	import { auth } from '$lib/auth.svelte';
	import { serverAddress } from '$lib/config';
	import Button from '$lib/components/Button.svelte';
	import Onboarding from '$lib/components/Onboarding.svelte';
	import { copyText, finishSetup, labels, stepNumber, wizard } from '../wizard.svelte';

	let copied = $state(false);
	// Internet mode: readers use the public domain, not the address this console uses.
	const domain = wizard.extra.access === 'internet' ? (wizard.extra.domain as string) : '';
	const address = domain ? `https://${domain}` : serverAddress();

	async function copy(): Promise<void> {
		if (await copyText(address)) {
			copied = true;
			setTimeout(() => (copied = false), 1600);
		}
	}
</script>

<Onboarding
	step={stepNumber('done')}
	steps={labels()}
	title="You are all set"
	lead="Your server is ready to use."
>
	<dl class="ob-rows">
		<div>
			<dt>Reader</dt>
			<dd>
				<span class="ob-truncate">{address}</span>
				<Button
					variant="ghost"
					size="sm"
					icon={copied ? 'check' : 'copy'}
					iconOnly
					aria-label="Copy reader address"
					onclick={copy}
				/>
			</dd>
		</div>
		<div>
			<dt>Owner</dt>
			<dd>{auth.user?.username ?? '—'}</dd>
		</div>
		<div>
			<dt>Sign-up</dt>
			<dd>{wizard.signup === 'open' ? 'Anyone can sign up' : 'Only people you add'}</dd>
		</div>
		{#if wizard.extra.mangadex !== undefined || wizard.extra.extensions}
			<div>
				<dt>Sources</dt>
				<dd>
					MangaDex {wizard.extra.mangadex ? 'on' : 'off'} · {Number(wizard.extra.extensions ?? 0)}
					{Number(wizard.extra.extensions ?? 0) === 1 ? 'extension' : 'extensions'}
				</dd>
			</div>
		{/if}
		<div>
			<dt>People added</dt>
			<dd class="k-num">{wizard.added.length}</dd>
		</div>
	</dl>
	{#each wizard.notices as notice (notice)}<p class="k-hint">{notice}</p>{/each}
	{#snippet footer()}
		<Button variant="ghost" onclick={() => finishSetup('/')}>Open the catalog</Button>
		<Button variant="primary" onclick={() => finishSetup('/sources/extensions')}>
			Add your first source
		</Button>
	{/snippet}
</Onboarding>
