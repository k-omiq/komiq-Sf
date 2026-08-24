/**
 * Bridge to the desktop app's native side (the Tauri shell, plan phase 4). In a
 * browser there is no bridge and `desktop.available` is false, so the "Run komiq
 * on this computer" path is offered only inside the desktop app.
 *
 * Contract the shell implements (Tauri commands):
 * - `start_local_server` → starts the source engine, then the server. Returns
 *   immediately; progress is read with the next command.
 * - `local_server_status` → `LocalServerStatus`.
 * - `local_server_log` → the last lines of the engine and server logs (string).
 *   Kept in memory only; the setup code and the engine's credential are masked.
 * - `local_network` → `LocalNetwork`: whether "Allow other devices" is on and the
 *   reader addresses other devices can open.
 * - `set_local_network { enabled }` → `LocalNetwork`. Saves the choice and restarts
 *   the server on all interfaces (on) or loopback only (off), on the same port when
 *   it is free. Resolves once the server answers again; rejects with the reason.
 *
 * The shell is `apps/admin/src-tauri` (README: "Desktop app").
 */

export type LocalStage = 'engine' | 'server' | 'ready';

export interface LocalServerStatus {
	/** The step running now, or `ready` once the server answers `/health`. */
	stage: LocalStage;
	/** Set when the current step failed; the stage says which one. */
	error?: string | null;
	/** Server origin once ready, e.g. `http://127.0.0.1:8080`. */
	address?: string | null;
	/** One-time setup code read from the server's own output, when it printed one. */
	setupCode?: string | null;
}

export interface LocalNetwork {
	/** "Allow other devices" is on: the server listens on every interface. */
	enabled: boolean;
	/** The server's port while it runs. */
	port: number | null;
	/** Reader addresses for other devices, e.g. `http://192.168.1.20:7580`. Empty when off. */
	addresses: string[];
}

type Invoke = <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;

function tauriInvoke(): Invoke | null {
	if (typeof window === 'undefined') return null;
	const internals = (window as unknown as { __TAURI_INTERNALS__?: { invoke?: Invoke } })
		.__TAURI_INTERNALS__;
	return internals?.invoke ?? null;
}

const invoke = tauriInvoke();

export const desktop = {
	available: invoke !== null,
	start(): Promise<void> {
		return invoke!<void>('start_local_server');
	},
	status(): Promise<LocalServerStatus> {
		return invoke!<LocalServerStatus>('local_server_status');
	},
	log(): Promise<string> {
		return invoke!<string>('local_server_log');
	},
	network(): Promise<LocalNetwork> {
		return invoke!<LocalNetwork>('local_network');
	},
	setNetwork(enabled: boolean): Promise<LocalNetwork> {
		return invoke!<LocalNetwork>('set_local_network', { enabled });
	},
};

/** A rejected command's reason: Tauri rejects with the command's error string. */
export function bridgeError(err: unknown, fallback: string): string {
	if (typeof err === 'string' && err) return err;
	return err instanceof Error && err.message ? err.message : fallback;
}

/** The setup code handed from `/start` to the wizard. Session-only, never persisted. */
const CODE_KEY = 'komiq-setup-code';

export function handOffSetupCode(code: string | null | undefined): void {
	try {
		if (code) sessionStorage.setItem(CODE_KEY, code);
	} catch {
		/* ignore */
	}
}

export function takeSetupCode(): string {
	try {
		const code = sessionStorage.getItem(CODE_KEY) ?? '';
		sessionStorage.removeItem(CODE_KEY);
		return code;
	} catch {
		return '';
	}
}
