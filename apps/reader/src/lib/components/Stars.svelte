<script lang="ts">
	import Icon from './Icon.svelte';

	let {
		value = $bindable(0),
		max = 10,
		size = 24,
		gap = 3,
		onchange,
	}: {
		value?: number;
		max?: number;
		size?: number;
		gap?: number;
		/** Fired only on an explicit user click (not on programmatic value changes). */
		onchange?: (value: number) => void;
	} = $props();

	let hover = $state(0);
	const display = $derived(hover || value);

	function pick(n: number): void {
		value = n;
		onchange?.(n);
	}
</script>

<div
	class="stars"
	style="gap:{gap}px"
	role="group"
	aria-label="Rating"
	onmouseleave={() => (hover = 0)}
>
	{#each Array(max) as _, i (i)}
		{@const n = i + 1}
		<button
			type="button"
			class="star"
			aria-label={`Rate ${n} of ${max}`}
			aria-pressed={value === n}
			onmouseenter={() => (hover = n)}
			onclick={() => pick(n)}
		>
			{#if n <= display}
				<Icon name="star" {size} fill="var(--k-star)" />
			{:else}
				<svg
					width={size}
					height={size}
					viewBox="0 0 24 24"
					fill="#37363116"
					stroke="#4a4842"
					stroke-width="1.4"
					aria-hidden="true"
				>
					<path d="M12 2l2.9 6.3 6.9.7-5.1 4.6 1.4 6.8L12 17.6 5.9 20.4l1.4-6.8L2.2 9l6.9-.7z" />
				</svg>
			{/if}
		</button>
	{/each}
</div>

<style>
	.stars {
		display: flex;
	}
	.star {
		background: none;
		border: none;
		padding: 3px;
		line-height: 0;
		cursor: pointer;
		color: inherit;
		border-radius: 6px;
	}
	.star:focus-visible {
		outline: 2px solid var(--k-star);
		outline-offset: 2px;
	}
</style>
