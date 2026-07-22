/** Focus containment, background isolation and focus restoration for console overlays. */

// Overlays stack (a confirmation over a drawer, quick search over either) and do not
// always close in the order they opened: confirming "discard changes" closes the
// drawer underneath in the same tick. So the page state they share is counted here,
// once, instead of being snapshotted and restored by each overlay on its own. With
// per-overlay snapshots the last one to close put back `overflow: hidden` and
// `inert`, and the page stayed locked.
const stack: HTMLElement[] = [];
const isolation = new Map<HTMLElement, { count: number; inert: boolean }>();
let bodyOverflow = '';

function isolate(element: HTMLElement): void {
	const entry = isolation.get(element);
	if (entry) entry.count += 1;
	else isolation.set(element, { count: 1, inert: element.inert });
	element.inert = true;
}

function release(element: HTMLElement): void {
	const entry = isolation.get(element);
	if (!entry) return;
	entry.count -= 1;
	if (entry.count > 0) return;
	isolation.delete(element);
	element.inert = entry.inert;
}

export function modal(node: HTMLElement, onclose: () => void) {
	const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
	const isolated: HTMLElement[] = [];
	let branch: HTMLElement = node;
	while (branch.parentElement) {
		for (const sibling of branch.parentElement.children) {
			if (
				sibling !== branch &&
				sibling instanceof HTMLElement &&
				!sibling.hasAttribute('data-modal-backdrop')
			) {
				isolated.push(sibling);
				isolate(sibling);
			}
		}
		branch = branch.parentElement;
		if (branch === document.body) break;
	}
	if (stack.length === 0) bodyOverflow = document.body.style.overflow;
	stack.push(node);
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
	// On the document, not the overlay: focus can sit outside it (a toast's dismiss
	// button, or the body after a scrim click) and Escape must still close it. Only the
	// topmost overlay answers.
	function keydown(event: KeyboardEvent) {
		if (stack.at(-1) !== node) return;
		if (event.key === 'Escape') {
			event.preventDefault();
			event.stopPropagation();
			onclose();
		} else if (event.key === 'Tab') {
			const items = focusable();
			const first = items[0];
			const last = items.at(-1);
			const active = document.activeElement;
			if (!first) {
				event.preventDefault();
				node.focus();
			} else if (!(active instanceof Node) || !node.contains(active)) {
				event.preventDefault();
				first.focus();
			} else if (event.shiftKey && (active === first || active === node)) {
				event.preventDefault();
				last?.focus();
			} else if (!event.shiftKey && active === last) {
				event.preventDefault();
				first.focus();
			}
		}
	}
	document.addEventListener('keydown', keydown);
	return {
		update(close: () => void) {
			onclose = close;
		},
		destroy() {
			document.removeEventListener('keydown', keydown);
			for (const element of isolated) release(element);
			const at = stack.lastIndexOf(node);
			if (at !== -1) stack.splice(at, 1);
			if (stack.length === 0) document.body.style.overflow = bodyOverflow;
			if (previous?.isConnected) previous.focus();
		},
	};
}
