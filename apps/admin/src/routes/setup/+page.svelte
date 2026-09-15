<script lang="ts">
	import type { Component } from 'svelte';
	import { goto } from '$app/navigation';
	import { href } from '$lib/paths';
	import { auth } from '$lib/auth.svelte';
	import { instance } from '$lib/instance.svelte';
	import { runsLocalServer, target } from '$lib/config';
	import Access from './steps/Access.svelte';
	import Direct from './steps/Direct.svelte';
	import Accounts from './steps/Accounts.svelte';
	import Done from './steps/Done.svelte';
	import Internet from './steps/Internet.svelte';
	import Lan from './steps/Lan.svelte';
	import Owner from './steps/Owner.svelte';
	import Sources from './steps/Sources.svelte';
	import Welcome from './steps/Welcome.svelte';
	import { persist, stepNumber, wizard } from './wizard.svelte';

	// First-run wizard for a server with no owner yet. Each step is its own component
	// in ./steps and reads/writes the shared `wizard` state. To add a step, put it in
	// this list where it belongs; `show` hides it on installs where it does not apply.
	const STEPS: { id: string; label: string; component: Component; show?: () => boolean }[] = [
		{ id: 'welcome', label: 'Welcome', component: Welcome },
		{ id: 'owner', label: 'Owner account', component: Owner },
		{ id: 'access', label: 'Access', component: Access },
		{
			id: 'lan',
			label: 'Home network',
			component: Lan,
			// Only the desktop app's own server can be opened to the home network. Offered
			// next to a domain too: devices at home then reach this computer without it. It
			// comes before the Internet step, so its restart of the server ends before the
			// tunnel starts.
			show: () => target === 'desktop' && runsLocalServer && wizard.extra.access !== 'direct',
		},
		{
			id: 'internet',
			label: 'Internet',
			component: Internet,
			show: () => wizard.extra.access === 'internet',
		},
		{
			id: 'direct',
			label: 'Address',
			component: Direct,
			show: () => wizard.extra.access === 'direct',
		},
		{ id: 'accounts', label: 'Accounts', component: Accounts },
		{ id: 'sources', label: 'Sources', component: Sources },
		{ id: 'done', label: 'Done', component: Done },
	];

	// Reactive: a choice on one step (e.g. access) adds or removes later steps, and the
	// step bar and Next/Back follow at once.
	const visibleSteps = () => STEPS.filter((s) => !s.show || s.show());
	const visible = $derived(visibleSteps());
	wizard.order = visibleSteps().map(({ id, label }) => ({ id, label }));
	$effect.pre(() => {
		wizard.order = visible.map(({ id, label }) => ({ id, label }));
	});
	if (!visibleSteps().some((s) => s.id === wizard.current)) wizard.current = 'welcome';
	const Current = $derived(visible.find((s) => s.id === wizard.current)?.component ?? Welcome);

	// Save progress (never a password) so a closed app resumes here.
	$effect(() => {
		void [wizard.current, wizard.signup, wizard.added.length, JSON.stringify(wizard.extra)];
		persist();
	});

	// A server with no owner has nothing saved past the owner step. Progress this browser
	// kept from an earlier install on the same address starts over, instead of opening a
	// later step where every save answers "Not authenticated". The choices go whatever
	// step is open: a saved step this install does not have already fell back to Welcome.
	$effect(() => {
		if (!instance.info?.setupRequired) return;
		if (stepNumber(wizard.current) > stepNumber('owner')) wizard.current = 'welcome';
		if (wizard.signup !== 'closed') wizard.signup = 'closed';
		if (wizard.added.length) wizard.added = [];
		if (Object.keys(wizard.extra).length) wizard.extra = {};
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
