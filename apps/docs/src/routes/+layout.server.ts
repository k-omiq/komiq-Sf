import { getHandbooks } from '$lib/server/content';

export function load() {
	return { handbooks: getHandbooks() };
}
