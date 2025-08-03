/** Focus containment, background isolation and focus restoration for console overlays. */
export function modal(node: HTMLElement, onclose: () => void) {
	const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
	const overflow = document.body.style.overflow;
	const isolated: { element: HTMLElement; inert: boolean }[] = [];
	let branch: HTMLElement = node;
	while (branch.parentElement) {
		for (const sibling of branch.parentElement.children) {
			if (
				sibling !== branch &&
				sibling instanceof HTMLElement &&
				!sibling.hasAttribute('data-modal-backdrop')
			) {
				isolated.push({ element: sibling, inert: sibling.inert });
				sibling.inert = true;
			}
		}
		branch = branch.parentElement;
		if (branch === document.body) break;
	}
	document.body.style.overflow = 'hidden';
	node.tabIndex = -1;
	function focusable(): HTMLElement[] {
		return [
			...node.querySelectorAll<HTMLElement>('a[href], button, input, select, textarea, [tabindex]'),
		].filter(
			(el) =>
				el.tabIndex >= 0 &&
				!el.matches(':disabled, [inert], [hidden]') &&
				el.getClientRects().length > 0,
		);
	}
	queueMicrotask(() => {
		if (node.isConnected) (focusable()[0] ?? node).focus();
	});
	function keydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			event.preventDefault();
			event.stopPropagation();
			onclose();
		} else if (event.key === 'Tab') {
			const items = focusable();
			const first = items[0];
			const last = items.at(-1);
			if (!first) {
				event.preventDefault();
				node.focus();
			} else if (
				event.shiftKey &&
				(document.activeElement === first || document.activeElement === node)
			) {
				event.preventDefault();
				last?.focus();
			} else if (!event.shiftKey && document.activeElement === last) {
				event.preventDefault();
				first.focus();
			}
		}
	}
	node.addEventListener('keydown', keydown);
	return {
		update(close: () => void) {
			onclose = close;
		},
		destroy() {
			node.removeEventListener('keydown', keydown);
			for (const { element, inert } of isolated) element.inert = inert;
			document.body.style.overflow = overflow;
			if (previous?.isConnected) previous.focus();
		},
	};
}
