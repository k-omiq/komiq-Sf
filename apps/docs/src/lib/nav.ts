/**
 * The shape of the docs: five handbooks, each an ordered list of chapters. This is
 * the only place that decides what is in the menu and in what order. A chapter's
 * title and text live in `src/content/<handbook id>/<slug>.md`; a chapter listed
 * here with no file yet is left out of the built site (see `lib/server/content.ts`).
 */
import type { IconName } from './components/Icon.svelte';

export interface Handbook {
	/** URL segment and content folder. */
	id: string;
	title: string;
	/** Who it is for, one sentence. */
	description: string;
	icon: IconName;
	/** Chapter slugs, in reading order. Each chapter's title lives in its own file. */
	chapters: string[];
}

export const handbooks: Handbook[] = [
	{
		id: 'start',
		title: 'Get started',
		description: 'The two apps, how they fit together, and which setup suits you.',
		icon: 'compass',
		chapters: ['what-is-komiq', 'how-it-works', 'choose-your-setup'],
	},
	{
		id: 'self-host',
		title: 'Host a server',
		description:
			'The admin app is the server and its management portal. Run it on your computer, a home server or a VPS.',
		icon: 'server',
		chapters: [
			'desktop-app',
			'docker',
			'first-run',
			'sources',
			'other-devices',
			'internet-access',
			'configuration',
			'update-and-backup',
			'troubleshooting',
		],
	},
	{
		id: 'guide',
		title: 'Reader app',
		description:
			'Connect the reader to the global server or to your own, then read and keep a library.',
		icon: 'book',
		chapters: [
			'apps',
			'connect',
			'accounts',
			'browse',
			'reading',
			'library',
			'community',
			'troubleshooting',
			'faq',
			'privacy',
			'copyright',
		],
	},
	{
		id: 'admin',
		title: 'Admin console',
		description: 'The management portal of your server: the catalogue, the sources and the people.',
		icon: 'wrench',
		chapters: [
			'overview',
			'catalog',
			'duplicates',
			'covers',
			'takedowns',
			'sources',
			'engine',
			'users',
			'settings',
		],
	},
	{
		id: 'developers',
		title: 'Developers',
		description: 'How komiq works inside: fetching, the image proxy, the engine, each platform.',
		icon: 'code',
		chapters: [
			'architecture',
			'repository',
			'local-development',
			'fetching',
			'image-proxy',
			'edge-caching',
			'media-manifests',
			'media-routes',
			'server',
			'graphql',
			'data-model',
			'catalogue-sync',
			'canonical-works',
			'suwayomi',
			'native-engine',
			'ios',
			'ios-engine',
			'android',
			'desktop',
			'reader-app',
			'admin-app',
			'auth-security',
			'deployment',
			'testing',
			'contributing',
		],
	},
];
