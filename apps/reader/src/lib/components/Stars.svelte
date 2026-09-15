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

	// Touch: ten stars in one row cannot each be 44pt wide on a phone, so the whole row
	// is one target, like a slider. The finger selects the star under it, a slide
	// corrects the choice with a live preview, and lifting the finger commits. A mouse
	// keeps hover + click.
	let row: HTMLDivElement;
	let sliding = false;
	// The click that follows a touch would rate a second time.
	let swallowClick = false;

	function starAt(x: number): number {
		const r = row.getBoundingClientRect();
		return Math.max(1, Math.min(max, Math.ceil(((x - r.left) / r.width) * max)));
	}
	function onDown(e: PointerEvent): void {
		if (e.pointerType === 'mouse') return;
		sliding = true;
		row.setPointerCapture(e.pointerId);
		hover = starAt(e.clientX);
	}
	function onMove(e: PointerEvent): void {
		if (sliding) hover = starAt(e.clientX);
	}
	function onUp(e: PointerEvent): void {
		if (!sliding) return;
		sliding = false;
		hover = 0;
		swallowClick = true;
		setTimeout(() => (swallowClick = false), 400);
		pick(starAt(e.clientX));
	}
	function onCancel(): void {
		sliding = false;
		hover = 0;
	}
	function onStarClick(n: number): void {
		if (swallowClick) swallowClick = false;
		else pick(n);
	}
</script>

<div
	class="stars"
	style="gap:{gap}px"
	role="group"
	aria-label="Rating"
	bind:this={row}
	onmouseleave={() => (hover = 0)}
	onpointerdown={onDown}
	onpointermove={onMove}
	onpointerup={onUp}
	onpointercancel={onCancel}
>
	{#each Array(max) as _, i (i)}
		{@const n = i + 1}
		<button
			type="button"
			class="star"
			aria-label={`Rate ${n} of ${max}`}
			aria-pressed={value === n}
			onpointerenter={(e) => {
				if (e.pointerType === 'mouse') hover = n;
			}}
			onclick={() => onStarClick(n)}
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
	@media (pointer: coarse) {
		/* A sideways slide belongs to the row; an up or down one still scrolls the page. */
		.stars {
			touch-action: pan-y;
		}
		.star {
			position: relative;
		}
		.star::after {
			content: '';
			position: absolute;
			left: 0;
			right: 0;
			top: 50%;
			height: 44px;
			transform: translateY(-50%);
		}
	}
</style>
