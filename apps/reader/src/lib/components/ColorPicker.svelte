<script lang="ts">
	// A colour picker that looks and works the same on every platform (a native
	// <input type="color"> opens a different dialog on each): a saturation and
	// brightness field, a hue slider, a hex field and a row of quick picks.
	//
	// It keeps its own HSV so that the hue does not jump when the colour goes grey or
	// black, and it reports at most one change per frame while a thumb is dragged.
	import { untrack } from 'svelte';
	import { hexToHsv, hsvToHex, parseHex, type Hsv } from '$lib/color';
	import Icon from './Icon.svelte';

	let {
		value,
		label,
		swatches = [],
		onchange,
	}: {
		/** `#rrggbb` */
		value: string;
		/** What the colour is for, for screen readers ("Accent colour"). */
		label: string;
		swatches?: readonly { name: string; hex: string }[];
		onchange: (hex: string) => void;
	} = $props();

	let hsv = $state<Hsv>(untrack(() => hexToHsv(value)));
	let draft = $state(untrack(() => value));
	/** The last colour this picker reported, so its own echo does not reset `hsv`. */
	let sent = untrack(() => value);
	const hex = $derived(hsvToHex(hsv));

	$effect(() => {
		const v = value;
		untrack(() => {
			if (v === sent) return;
			sent = v;
			hsv = hexToHsv(v);
			draft = v;
		});
	});

	let frame = 0;
	// A change still waiting for its frame must not land after the picker is gone.
	$effect(() => () => cancelAnimationFrame(frame));
	function set(next: Hsv, now = false): void {
		hsv = next;
		const out = hsvToHex(next);
		draft = out;
		sent = out;
		cancelAnimationFrame(frame);
		if (now) onchange(out);
		else frame = requestAnimationFrame(() => onchange(out));
	}

	// ---- Saturation / brightness field ------------------------------------
	let field = $state<HTMLDivElement>();
	let dragging = $state(false);

	function fromPointer(e: PointerEvent): void {
		if (!field) return;
		const r = field.getBoundingClientRect();
		const s = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
		const v = Math.min(1, Math.max(0, 1 - (e.clientY - r.top) / r.height));
		set({ ...hsv, s, v });
	}
	function down(e: PointerEvent): void {
		if (e.button !== 0) return;
		field?.setPointerCapture(e.pointerId);
		field?.focus();
		dragging = true;
		fromPointer(e);
	}
	function move(e: PointerEvent): void {
		if (dragging) fromPointer(e);
	}
	function up(): void {
		dragging = false;
	}
	function key(e: KeyboardEvent): void {
		const step = e.shiftKey ? 0.1 : 0.01;
		const clamp = (x: number) => Math.min(1, Math.max(0, x));
		const moves: Record<string, Partial<Hsv>> = {
			ArrowLeft: { s: clamp(hsv.s - step) },
			ArrowRight: { s: clamp(hsv.s + step) },
			ArrowUp: { v: clamp(hsv.v + step) },
			ArrowDown: { v: clamp(hsv.v - step) },
			Home: { s: 0 },
			End: { s: 1 },
			PageUp: { v: 1 },
			PageDown: { v: 0 },
		};
		const m = moves[e.key];
		if (!m) return;
		e.preventDefault();
		set({ ...hsv, ...m });
	}

	// ---- Hex field ----------------------------------------------------------
	function typed(text: string): void {
		draft = text;
		const parsed = parseHex(text);
		if (parsed) set(hexToHsv(parsed), true);
	}
</script>

<div class="picker" style:--hue={hsv.h} style:--pick={hex}>
	<div
		bind:this={field}
		class="field"
		class:dragging
		role="slider"
		tabindex="0"
		aria-label="{label}: saturation and brightness"
		aria-valuemin={0}
		aria-valuemax={100}
		aria-valuenow={Math.round(hsv.s * 100)}
		aria-valuetext="Saturation {Math.round(hsv.s * 100)}%, brightness {Math.round(hsv.v * 100)}%"
		onpointerdown={down}
		onpointermove={move}
		onpointerup={up}
		onpointercancel={up}
		onkeydown={key}
	>
		<span class="thumb" style:left="{hsv.s * 100}%" style:top="{(1 - hsv.v) * 100}%"></span>
	</div>

	<input
		class="hue"
		type="range"
		min="0"
		max="359"
		step="1"
		aria-label="{label}: hue"
		value={Math.round(hsv.h)}
		oninput={(e) => set({ ...hsv, h: e.currentTarget.valueAsNumber })}
	/>

	<div class="foot">
		<label class="hex">
			<span class="chip" aria-hidden="true"></span>
			<span class="hash" aria-hidden="true">#</span>
			<input
				type="text"
				inputmode="text"
				autocomplete="off"
				autocapitalize="off"
				spellcheck="false"
				maxlength="7"
				aria-label="{label}: hex code"
				value={draft.replace(/^#/, '')}
				oninput={(e) => typed(e.currentTarget.value)}
				onblur={() => (draft = hex)}
			/>
		</label>
		{#if swatches.length}
			<div class="quick" role="group" aria-label="{label}: quick picks">
				{#each swatches as s (s.hex)}
					<button
						type="button"
						class="dot k-hit"
						style:background={s.hex}
						aria-label={s.name}
						aria-pressed={hex === s.hex}
						title={s.name}
						onclick={() => set(hexToHsv(s.hex), true)}
					>
						{#if hex === s.hex}<Icon name="check" size={12} strokeWidth={3} />{/if}
					</button>
				{/each}
			</div>
		{/if}
	</div>
</div>

<style>
	.picker {
		display: flex;
		flex-direction: column;
		gap: 14px;
	}

	/* Hue at full colour, washed to white across, to black downwards. */
	.field {
		position: relative;
		height: 168px;
		border-radius: 12px;
		background:
			linear-gradient(to top, #000, transparent),
			linear-gradient(to right, #fff, hsl(var(--hue) 100% 50%));
		cursor: crosshair;
		touch-action: none;
		-webkit-user-select: none;
		user-select: none;
	}
	.field:focus-visible {
		outline: 2px solid var(--k-accent);
		outline-offset: 3px;
	}
	.thumb {
		position: absolute;
		width: 22px;
		height: 22px;
		border-radius: 50%;
		background: var(--pick);
		transform: translate(-50%, -50%);
		box-shadow:
			0 0 0 3px #fff,
			0 2px 8px rgb(0 0 0 / 0.45);
		pointer-events: none;
		transition: transform 160ms cubic-bezier(0.16, 1, 0.3, 1);
	}
	.field.dragging .thumb {
		transform: translate(-50%, -50%) scale(1.15);
	}

	/* ---- Hue slider -------------------------------------------------------- */
	.hue {
		-webkit-appearance: none;
		appearance: none;
		width: 100%;
		height: 28px;
		margin: 0;
		background: transparent;
		cursor: pointer;
		touch-action: pan-y;
	}
	.hue::-webkit-slider-runnable-track {
		height: 14px;
		border-radius: 999px;
		background: linear-gradient(
			to right,
			hsl(0 100% 50%),
			hsl(60 100% 50%),
			hsl(120 100% 50%),
			hsl(180 100% 50%),
			hsl(240 100% 50%),
			hsl(300 100% 50%),
			hsl(360 100% 50%)
		);
	}
	.hue::-moz-range-track {
		height: 14px;
		border-radius: 999px;
		background: linear-gradient(
			to right,
			hsl(0 100% 50%),
			hsl(60 100% 50%),
			hsl(120 100% 50%),
			hsl(180 100% 50%),
			hsl(240 100% 50%),
			hsl(300 100% 50%),
			hsl(360 100% 50%)
		);
	}
	.hue::-webkit-slider-thumb {
		-webkit-appearance: none;
		width: 22px;
		height: 22px;
		margin-top: -4px;
		border: none;
		border-radius: 50%;
		background: hsl(var(--hue) 100% 50%);
		box-shadow:
			0 0 0 3px #fff,
			0 2px 8px rgb(0 0 0 / 0.45);
	}
	.hue::-moz-range-thumb {
		width: 22px;
		height: 22px;
		border: none;
		border-radius: 50%;
		background: hsl(var(--hue) 100% 50%);
		box-shadow:
			0 0 0 3px #fff,
			0 2px 8px rgb(0 0 0 / 0.45);
	}
	.hue:focus-visible {
		outline: none;
	}
	.hue:focus-visible::-webkit-slider-thumb {
		box-shadow:
			0 0 0 3px #fff,
			0 0 0 5px var(--k-accent);
	}
	.hue:focus-visible::-moz-range-thumb {
		box-shadow:
			0 0 0 3px #fff,
			0 0 0 5px var(--k-accent);
	}

	/* ---- Hex field and quick picks ------------------------------------------ */
	.foot {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 12px 16px;
	}
	.hex {
		flex: 0 0 auto;
		display: flex;
		align-items: center;
		gap: 8px;
		height: 40px;
		padding: 0 12px 0 8px;
		border-radius: 10px;
		background: var(--k-surface-5);
		cursor: text;
	}
	.hex:focus-within {
		outline: 2px solid var(--k-accent);
		outline-offset: 2px;
	}
	.chip {
		width: 24px;
		height: 24px;
		border-radius: 7px;
		background: var(--pick);
	}
	.hash {
		color: var(--k-text-faint);
		font-size: 14px;
		font-weight: 600;
	}
	.hex input {
		width: 7ch;
		margin-left: -6px;
		padding: 0;
		border: none;
		background: transparent;
		color: var(--k-text);
		font-family: var(--k-font-mono);
		font-size: 14px;
		text-transform: uppercase;
		outline: none;
	}
	.quick {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
	}
	.dot {
		width: 26px;
		height: 26px;
		padding: 0;
		border: none;
		border-radius: 50%;
		display: grid;
		place-items: center;
		color: #fff;
		cursor: pointer;
		transition: transform var(--k-dur) var(--k-ease);
	}
	.dot:hover {
		transform: scale(1.12);
	}
	.dot :global(svg) {
		filter: drop-shadow(0 1px 1px rgb(0 0 0 / 0.4));
	}

	@media (max-width: 640px) {
		.field {
			height: 150px;
		}
		/* iOS zooms into a text field under 16px. */
		.hex input {
			font-size: 16px;
		}
		.dot {
			width: 30px;
			height: 30px;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.thumb,
		.dot {
			transition: none;
		}
	}
</style>
