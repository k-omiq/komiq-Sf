<script lang="ts">
	import type { Snippet } from 'svelte';
	import type { HTMLAnchorAttributes, HTMLButtonAttributes } from 'svelte/elements';
	import Icon, { type IconName } from './Icon.svelte';

	type Variant = 'secondary' | 'primary' | 'ghost' | 'danger' | 'danger-solid';
	type Size = 'sm' | 'md' | 'lg';

	let {
		variant = 'secondary',
		size = 'md',
		icon,
		iconOnly = false,
		busy = false,
		href,
		disabled = false,
		class: klass = '',
		children,
		...rest
	}: {
		variant?: Variant;
		size?: Size;
		/** Leading icon. Replaced by a spinner while `busy`. */
		icon?: IconName;
		/** Square button: the children become the accessible name only via aria-label. */
		iconOnly?: boolean;
		/** In flight: disables the button and spins the icon. */
		busy?: boolean;
		/** Render a link styled as a button. */
		href?: string;
		disabled?: boolean;
		class?: string;
		children?: Snippet;
	} & Omit<HTMLButtonAttributes & HTMLAnchorAttributes, 'class' | 'disabled' | 'href'> = $props();

	const classes = $derived(
		[
			'k-btn',
			variant !== 'secondary' && `k-btn--${variant}`,
			size !== 'md' && `k-btn--${size}`,
			iconOnly && 'k-btn--icon',
			klass,
		]
			.filter(Boolean)
			.join(' '),
	);
	const iconSize = $derived(size === 'sm' ? 14 : 16);
</script>

{#snippet content()}
	{#if busy}
		<span class="k-spin"><Icon name="refresh" size={iconSize} /></span>
	{:else if icon}
		<Icon name={icon} size={iconSize} />
	{/if}
	{#if children && !iconOnly}{@render children()}{/if}
{/snippet}

{#if href && !disabled}
	<a {href} class={classes} {...rest}>{@render content()}</a>
{:else}
	<button
		type="button"
		class={classes}
		disabled={disabled || busy}
		aria-busy={busy || undefined}
		{...rest}>{@render content()}</button
	>
{/if}
