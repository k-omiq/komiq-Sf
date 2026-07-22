<script lang="ts" generics="T extends string">
	let {
		value,
		options,
		label,
		disabled = false,
		onchange,
	}: {
		value: T;
		options: { value: T; label: string; count?: number | null }[];
		/** Accessible name of the group. */
		label: string;
		disabled?: boolean;
		onchange: (value: T) => void;
	} = $props();
</script>

<div class="k-segmented" role="group" aria-label={label}>
	{#each options as opt (opt.value)}
		<button
			type="button"
			aria-pressed={value === opt.value}
			{disabled}
			onclick={() => onchange(opt.value)}
		>
			{opt.label}
			{#if opt.count != null}<span class="k-count">({opt.count})</span>{/if}
		</button>
	{/each}
</div>
