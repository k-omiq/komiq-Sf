import {
	constants,
	copyFileSync,
	mkdirSync,
	readFileSync,
	readdirSync,
	realpathSync,
} from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

/** @param {string} previous @param {string} current */
export function retainImmutableAssets(previous, current) {
	const source = realpathSync(join(previous, '_app/immutable'));
	const destination = realpathSync(join(current, '_app/immutable'));
	if (source === destination) throw new Error('Previous and current builds must be different');
	let copied = 0;
	/** @param {string} from @param {string} to */
	function copyDirectory(from, to) {
		mkdirSync(to, { recursive: true });
		for (const entry of readdirSync(from, { withFileTypes: true })) {
			const sourceFile = join(from, entry.name);
			const destinationFile = join(to, entry.name);
			if (entry.isDirectory()) {
				copyDirectory(sourceFile, destinationFile);
			} else if (entry.isFile()) {
				try {
					copyFileSync(sourceFile, destinationFile, constants.COPYFILE_EXCL);
					copied++;
				} catch (error) {
					if (
						!error ||
						typeof error !== 'object' ||
						!('code' in error) ||
						error.code !== 'EEXIST'
					) {
						throw error;
					}
					if (!readFileSync(sourceFile).equals(readFileSync(destinationFile))) {
						throw new Error(`Immutable asset collision: ${destinationFile}`);
					}
				}
			} else {
				throw new Error(`Expected a regular immutable asset: ${sourceFile}`);
			}
		}
	}
	copyDirectory(source, destination);
	return copied;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
	const [, , previous, current] = process.argv;
	if (!previous || !current)
		throw new Error('Usage: retain-assets.mjs PREVIOUS_BUILD CURRENT_BUILD');
	console.log(`Retained ${retainImmutableAssets(previous, current)} immutable files`);
}
