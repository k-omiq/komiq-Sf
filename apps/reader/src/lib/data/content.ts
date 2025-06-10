/**
 * Static site copy for the Support page. Editorial content, not catalog data —
 * served through `source.ts` so screens keep a single data seam.
 */

export const supportCategories = [
	{
		icon: 'user',
		title: 'Account',
		desc: 'Sign-in, profile, and privacy settings.',
		iconBg: 'rgba(127,211,154,0.14)',
		iconColor: 'var(--k-ongoing)',
	},
	{
		icon: 'book',
		title: 'Reading',
		desc: 'Reader modes and display settings.',
		iconBg: 'rgba(198,156,240,0.14)',
		iconColor: 'var(--k-accent-purple)',
	},
	{
		icon: 'card',
		title: 'Donations',
		desc: 'How supporting komiq works.',
		iconBg: 'rgba(224,179,84,0.14)',
		iconColor: 'var(--k-hiatus)',
	},
	{
		icon: 'flag',
		title: 'Report an issue',
		desc: 'Broken pages, wrong info, or abuse.',
		iconBg: 'rgba(224,131,105,0.14)',
		iconColor: 'var(--k-accent)',
	},
];

export const faqs = [
	{
		q: 'Is komiq free to use?',
		a: 'Yes. komiq is free for everyone and ad-free, and always will be. Donations are voluntary — they help cover running costs, not unlock anything, so every series is fully readable without paying.',
	},
	{
		q: "What's the difference between manga, manhwa, and manhua?",
		a: 'They refer to comics from different regions: manga (Japan, read right-to-left), manhwa (Korea, usually full-color vertical scroll), and manhua (China). You can filter by any of these formats on the Browse page.',
	},
	{
		q: 'Can I change the reading direction and page size?',
		a: 'Absolutely. Open any chapter and tap the settings icon to switch between long-strip and single-page modes and adjust page width. Your preferences are remembered per device.',
	},
	{
		q: "A chapter is missing pages or won't load. What do I do?",
		a: 'First try refreshing. If it persists, use the Report an issue option on the chapter page so our team can re-process the file, or email us below.',
	},
];
