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

/** Desktop filter rails stay in normal tab order; mobile sheets behave as dialogs. */
export function mobileModalFocus(node: HTMLElement, open: boolean) {
	let active: ReturnType<typeof modalFocus> | undefined;
	let current = open;
	const mobile = matchMedia('(max-width: 640px)');
	const update = (next: boolean) => {
		current = next;
		active?.destroy();
		active = undefined;
		node.inert = mobile.matches && !next;
		if (next && mobile.matches) {
			node.setAttribute('role', 'dialog');
			node.setAttribute('aria-modal', 'true');
			node.setAttribute('aria-label', 'Browse filters');
			active = modalFocus(node);
		} else {
			node.removeAttribute('role');
			node.removeAttribute('aria-modal');
			node.removeAttribute('aria-label');
		}
	};
	const resize = () => update(current);
	mobile.addEventListener('change', resize);
	update(open);
	return {
		update,
		destroy() {
			mobile.removeEventListener('change', resize);
			active?.destroy();
			node.inert = false;
		},
	};
}
