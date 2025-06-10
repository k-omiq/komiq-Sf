import { createBackend } from '@komika/api';
import { config } from './config';

/** The unified Komika backend the console reads catalog + writes overrides through. */
export const backend = createBackend({ endpoint: config.apiEndpoint });
