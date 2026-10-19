<script lang="ts">
	/** "Don't ask again this session" box for a confirm dialog footer. */
	let { checked = $bindable(false), disabled = false }: { checked?: boolean; disabled?: boolean } =
		$props();
</script>

<label class="skip">
	<input type="checkbox" bind:checked {disabled} />
	<span>Don't ask again this session</span>
</label>

<style>
	.skip {
		display: inline-flex;
		align-items: center;
		gap: var(--k-space-2);
		min-height: 28px;
		margin-right: auto;
		color: var(--k-text-2);
		font-size: var(--k-text-sm);
		cursor: pointer;
	}
	@media (pointer: coarse) {
		.skip {
			min-height: 44px;
		}
	}
</style>
