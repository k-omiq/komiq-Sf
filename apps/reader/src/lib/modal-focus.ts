/** Contain keyboard focus and suspend background interaction while an overlay is open. */
export function modalFocus(node: HTMLElement) {
	const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
	const suspended: { node: HTMLElement; inert: boolean }[] = [];
	let branch: HTMLElement = node;
	while (branch.parentElement) {
		for (const sibling of branch.parentElement.children) {
			if (
				sibling !== branch &&
				sibling instanceof HTMLElement &&
				!sibling.hasAttribute('data-modal-backdrop')
			) {
				suspended.push({ node: sibling, inert: sibling.inert });
				sibling.inert = true;
			}
		}
		branch = branch.parentElement;
	}
	const candidates = () =>
		Array.from(
			node.querySelectorAll<HTMLElement>(
				'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
			),
		).filter((el) => !el.closest('[inert]') && el.getClientRects().length > 0);
	node.tabIndex = -1;
	queueMicrotask(() => (candidates()[0] ?? node).focus());
	const keydown = (event: KeyboardEvent) => {
		if (event.key !== 'Tab') return;
		const items = candidates();
		const first = items[0] ?? node;
		const last = items.at(-1) ?? node;
		if (event.shiftKey && (document.activeElement === first || document.activeElement === node)) {
			event.preventDefault();
			last.focus();
		} else if (
			!event.shiftKey &&
			(document.activeElement === last || document.activeElement === node)
		) {
			event.preventDefault();
			first.focus();
		}
	};
	node.addEventListener('keydown', keydown);
	return {
		destroy() {
			node.removeEventListener('keydown', keydown);
			for (const entry of suspended) entry.node.inert = entry.inert;
			if (previous?.isConnected) previous.focus();
		},
	};
}

/**
 * Desktop filter rails stay in normal tab order; below `query` the same element is a
 * sheet and behaves as a modal dialog (focus contained, background inert).
 *
 * Takes either a bare `open` flag (the 640px phone breakpoint) or an options object, so
 * a screen whose rail collapses earlier — Browse turns it into a sheet on tablets too —
 * can say where. The dialog role is applied ONLY while it is open below the breakpoint:
 * on desktop the rail is ordinary page content and must not announce itself as a dialog.
 */
export function mobileModalFocus(
	node: HTMLElement,
	param: boolean | { open: boolean; query?: string; label?: string },
) {
	const opts = typeof param === 'boolean' ? { open: param } : param;
	const label = opts.label ?? 'Browse filters';
	let active: ReturnType<typeof modalFocus> | undefined;
	let current = opts.open;
	const mobile = matchMedia(opts.query ?? '(max-width: 640px)');
	const update = (next: boolean) => {
		current = next;
		active?.destroy();
		active = undefined;
		node.inert = mobile.matches && !next;
		if (next && mobile.matches) {
			node.setAttribute('role', 'dialog');
			node.setAttribute('aria-modal', 'true');
			node.setAttribute('aria-label', label);
			active = modalFocus(node);
		} else {
			node.removeAttribute('role');
			node.removeAttribute('aria-modal');
			node.removeAttribute('aria-label');
		}
	};
	const resize = () => update(current);
	mobile.addEventListener('change', resize);
	update(current);
	return {
		update(p: boolean | { open: boolean }) {
			update(typeof p === 'boolean' ? p : p.open);
		},
		destroy() {
			mobile.removeEventListener('change', resize);
			active?.destroy();
			node.inert = false;
		},
	};
}
