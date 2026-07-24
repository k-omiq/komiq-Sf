/**
 * The server's public facts (`instance` query): whether it still needs its owner,
 * its name and version, and the sign-up switch. Loaded once at boot by the layout
 * on self-host builds; the hosted console never asks.
 */
import type { InstanceInfo } from '@komika/api';
import { backend } from './context';

export const instance = $state<{
	info: InstanceInfo | null;
	ready: boolean;
	error: string | null;
}>({ info: null, ready: false, error: null });

export async function loadInstance(): Promise<InstanceInfo | null> {
	instance.error = null;
	try {
		if (!backend.instance) throw new Error('This server does not support setup.');
		instance.info = await backend.instance();
	} catch (err) {
		instance.info = null;
		instance.error = err instanceof Error ? err.message : 'The server could not be reached.';
	}
	instance.ready = true;
	return instance.info;
}
