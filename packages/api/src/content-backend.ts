import type { Chapter, Id, Page, Series } from '@komika/types';

/** Identifies a specific series within a specific source, resolved from `workSources`. */
export interface SourceRef {
	/** Suwayomi source id (or "mangadex"). */
	sourceId: string;
	/** Manga id/slug within that source — corresponds to `MangaType.url`. */
	sourceKey: string;
	/**
	 * Extension package id, used for on-device provisioning. Sourced from
	 * `workSources.extension.pkgName`. When present, the local backend installs the
	 * extension (once) before resolving/fetching; when absent, provisioning is skipped.
	 */
	pkgName?: string;
	/**
	 * Extension store index url, used to register the extension repo before install.
	 * Sourced from `workSources.extension.repoUrl`. Optional — omitted for extensions
	 * whose store is already configured.
	 */
	repoUrl?: string;
	/**
	 * Human-readable title hint for the general search fallback when resolving
	 * `sourceKey` → internal engine id fails via the fast/by-id paths.
	 */
	title?: string;
}

/**
 * The content-only slice of a backend: live series/chapters/pages fetched from a
 * specific source mapping. Implemented on-device by the embedded engine
 * (LocalSuwayomiBackend). Deliberately excludes auth/library/progress/social —
 * those always route to the hosted server (see the §1 routing table).
 */
export interface ContentBackend {
	/** Whether the local engine is up and can serve content right now. */
	isReady(): Promise<boolean>;
	/** Live series metadata for one source mapping. */
	series(ref: SourceRef): Promise<Series>;
	/** Live chapter list for one source mapping. */
	chapters(ref: SourceRef): Promise<Chapter[]>;
	/**
	 * Page image URLs for a chapter. `chapterId` is the engine's internal integer
	 * chapter id (passed as a string `Id`) — the same value returned as `Chapter.id`
	 * from {@link chapters}, not a source-local slug.
	 */
	pages(chapterId: Id): Promise<Page[]>;
}
