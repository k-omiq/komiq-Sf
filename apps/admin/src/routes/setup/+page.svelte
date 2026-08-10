<script lang="ts">
	import type { Component } from 'svelte';
	import { goto } from '$app/navigation';
	import { href } from '$lib/paths';
	import { auth } from '$lib/auth.svelte';
	import { instance } from '$lib/instance.svelte';
	import { runsLocalServer, target } from '$lib/config';
	import Accounts from './steps/Accounts.svelte';
	import Done from './steps/Done.svelte';
	import Lan from './steps/Lan.svelte';
	import Owner from './steps/Owner.svelte';
	import Welcome from './steps/Welcome.svelte';
	import { persist, stepNumber, wizard } from './wizard.svelte';

	// First-run wizard for a server with no owner yet. Each step is its own component
	// in ./steps and reads/writes the shared `wizard` state. To add a step, put it in
	// this list where it belongs; `show` hides it on installs where it does not apply.
	const STEPS: { id: string; label: string; component: Component; show?: () => boolean }[] = [
		{ id: 'welcome', label: 'Welcome', component: Welcome },
		{ id: 'owner', label: 'Owner account', component: Owner },
		{
			id: 'lan',
			label: 'Home network',
			component: Lan,
			// Only the desktop app's own server can be opened to the home network.
			show: () => target === 'desktop' && runsLocalServer && wizard.extra.access !== 'internet',
		},
		{ id: 'accounts', label: 'Accounts', component: Accounts },
		{ id: 'done', label: 'Done', component: Done },
	];

	const visible = STEPS.filter((s) => !s.show || s.show());
	wizard.order = visible.map(({ id, label }) => ({ id, label }));
	if (!visible.some((s) => s.id === wizard.current)) wizard.current = 'welcome';
	const Current = $derived(visible.find((s) => s.id === wizard.current)?.component ?? Welcome);

	// Save progress (never a password) so a closed app resumes here.
	$effect(() => {
		void [wizard.current, wizard.signup, wizard.added.length, JSON.stringify(wizard.extra)];
		persist();
	});

	// A finished server opens the catalog instead, unless this browser is mid-wizard
	// as the new owner (the steps after the account still belong to setup).
	$effect(() => {
		const pastOwner = stepNumber(wizard.current) > stepNumber('owner');
		if (instance.info && !instance.info.setupRequired && !(auth.user && pastOwner)) {
			void goto(href('/'), { replaceState: true });
		}
	});
</script>

<svelte:head><title>Set up - komiq</title></svelte:head>

<Current />
