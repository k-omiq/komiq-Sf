export async function installAdminQueueFixtures(page, state = {}) {
	const at = '2026-10-03T00:00:00Z';
	await page.addInitScript(() => localStorage.setItem('komika-admin-token', 'local-queue-fixture'));
	await page.route(
		(url) => url.pathname === '/graphql',
		async (route) => {
			const req = route.request(),
				body =
					req.method() === 'POST'
						? req.postDataJSON()
						: Object.fromEntries(new URL(req.url()).searchParams),
				q = body.query || '',
				v = typeof body.variables === 'string' ? JSON.parse(body.variables) : body.variables || {},
				p = v.page || 1;
			if (/\bmutation\b/.test(q))
				return route.fulfill({
					headers: { 'access-control-allow-origin': '*' },
					json: { errors: [{ message: 'Local fixture rejects mutations' }] },
				});
			if (state.delayField && q.includes(state.delayField))
				await new Promise((r) => setTimeout(r, 1200));
			if (state.errorField && q.includes(state.errorField))
				return route.fulfill({
					headers: { 'access-control-allow-origin': '*' },
					json: { errors: [{ message: 'Fixture queue unavailable' }] },
				});
			const envelope = (items) => ({ items, page: p, hasNextPage: p < 3, total: 150 }),
				long = 'VeryLongSourceAndReportContext'.repeat(12);
			const user = {
				id: 'admin-fixture',
				username: 'fixture-admin',
				email: 'fixture@example.invalid',
				isAdmin: true,
				isBanned: false,
				showNsfw: true,
				createdAt: at,
				avatarUrl: null,
			};
			const reports = Array.from({ length: 50 }, (_, i) => ({
				id: `report-${p}-${i}`,
				kind: v.kind || 'OTHER',
				status: v.status || 'OPEN',
				reporter: 'fixture-reader',
				subjectId: 'w_fixture',
				subjectIdSecondary: null,
				subjectTitle: `Page ${p} Report ${i} ${long}`,
				commentId: null,
				reportedUsername: null,
				commentExcerpt: null,
				sourceUrl: null,
				detail: long,
				adminNote: null,
				createdAt: at,
				resolvedAt: null,
			}));
			const merges = Array.from({ length: 50 }, (_, i) => ({
				id: `merge-${p}-${i}`,
				sourceSeriesId: `ss-${i}`,
				sourceWorkId: `w_source_${i}`,
				candidateWorkId: `w_target_${i}`,
				sourceTitle: `Page ${p} Source ${i}`,
				candidateTitle: `Candidate ${i}`,
				sourceCoverUrl: '',
				candidateCoverUrl: '',
				score: 0.8,
				method: 'title_corroborated',
				status: 'pending',
				createdAt: at,
			}));
			const health = Array.from({ length: 40 }, (_, i) => ({
				id: `fixture-${i}`,
				name: i === 0 ? long : `Fixture source ${i}`,
				pkgName: 'fixture',
				installed: true,
				subscribed: true,
				subscriptionDisabledAt: null,
				series: 150,
				failing: 40,
				confirmedFailing: 40,
				cachedFallback: 35,
				fetchError: 5,
				zeroChapterSeries: 10,
				exclusiveWorks: 4,
				worstStreak: 5,
				lastFailureAt: at,
				lastScannedAt: at,
				outageDetectedAt: at,
				outageLastAlertAt: at,
				outageKind: 'fetch_error',
				outageParkedUntil: at,
			}));
			const extension = (pkgName, lang, isInstalled) => ({
				pkgName,
				name: lang === 'en' ? 'Fixture English' : 'Fixture Japanese',
				versionName: '1.0',
				versionCode: 1,
				lang,
				isNsfw: false,
				isInstalled,
				hasUpdate: false,
				isObsolete: false,
				iconUrl: null,
				sources: [],
				subscribed: false,
			});
			const data = {
				session: { token: 'local-queue-fixture', user, expiresAt: '2099-01-01T00:00:00Z' },
				users: envelope(
					Array.from({ length: 20 }, (_, i) => ({
						...user,
						id: `user-${p}-${i}`,
						username: `Page ${p} User ${i}`,
						isAdmin: false,
					})),
				),
				canonicalUpdates: Array.from({ length: 20 }, (_, i) => ({
					workId: `w_update_${p}_${i}`,
					mangadexId: `md${i}`,
					title: `Page ${p} Update ${i}`,
					isNsfw: false,
					coverUrl: null,
					latestChapter: '12',
					latestChapterTitle: null,
					latestAt: at,
				})),
				coverIssues: envelope(
					Array.from({ length: 20 }, (_, i) => ({
						workId: `w_cover_${p}_${i}`,
						title: `Page ${p} Cover ${i}`,
						coverUrl: '',
						reason: 'empty',
						detail: long,
						attempts: 2,
						firstSeen: at,
						lastSeen: at,
					})),
				),
				mergeQueue: envelope(merges),
				reports: { ...envelope(reports), openCount: 150 },
				sourceScanHealth: health,
				sources: [
					{
						id: 'fixture-0',
						name: 'Fixture source',
						lang: 'en',
						isNsfw: false,
						iconUrl: null,
						pkgName: 'one',
					},
				],
				extensions: [extension('one', 'en', true), extension('two', 'ja', false)],
				extensionRepos: [],
				sourceIngestJobs: [
					{
						id: 'ingest-fixture',
						sourceId: 'fixture-0',
						state: 'running',
						pagesDone: 50,
						itemsSeen: 1000,
						succeeded: 990,
						failed: 10,
						newWorks: 950,
						autoMerged: 30,
						queuedForReview: 10,
						alreadyExisting: 0,
						error: null,
						startedAt: at,
						finishedAt: null,
					},
				],
				sourceBrowse: {
					items: [
						{
							suwayomiMangaId: '123',
							title: 'Fixture ingest title',
							thumbnailUrl: null,
							inLibrary: false,
						},
					],
					page: p,
					hasNextPage: false,
				},
				scanStatus: { librarySize: 150, overdueCount: 4, lastTickAt: at, nextDueAt: at },
			};
			await route.fulfill({ headers: { 'access-control-allow-origin': '*' }, json: { data } });
		},
	);
}
