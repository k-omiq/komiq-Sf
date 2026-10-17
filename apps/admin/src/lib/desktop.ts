/**
 * Bridge to the desktop app's native side (the Tauri shell, plan phase 4). In a
 * browser there is no bridge and `desktop.available` is false, so the "Run komiq
 * on this computer" path is offered only inside the desktop app.
 *
 * Contract the shell implements (Tauri commands):
 * - `start_local_server` → starts the database, the source engine, then the server, or
 *   tries a failed step again. Returns immediately; progress is read with the next
 *   command.
 * - `local_server_status` → `LocalServerStatus`.
 * - `local_server_log` → the last lines of the database, engine and server logs
 *   (string), with every warning of this start listed again on top. Kept in memory
 *   only; the setup code and the engine's credential are masked.
 * - `local_network` → `LocalNetwork`: whether "Allow other devices" is on and the
 *   reader addresses other devices can open.
 * - `set_local_network { enabled }` → `LocalNetwork`. Saves the choice and restarts
 *   the server on all interfaces (on) or loopback only (off), on the same port when
 *   it is free. Resolves once the server answers again; rejects with the reason.
 * - `firewall_info` → `FirewallInfo`: this computer's system, and whether the shell can
 *   open the system firewall for komiq by itself.
 * - `open_firewall` → lets komiq's server through the system firewall. Resolves with a
 *   sentence saying what was done; rejects with a plain reason.
 *
 * A shell that predates a command rejects the call. For the two firewall commands that
 * means "no automatic help": `firewallInfo()` answers null.
 *
 * The shell is `apps/admin/src-tauri` (README: "Desktop app").
 */

export type LocalStage = 'database' | 'engine' | 'server' | 'ready';

export interface LocalServerStatus {
	/** The step running now, or `ready` once the server answers `/health`. */
	stage: LocalStage;
	/** Set when the current step failed; the stage says which one. */
	error?: string | null;
	/** Programs of an earlier run that were left running. Plain sentences. */
	warnings?: string[] | null;
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

export interface FirewallInfo {
	os: 'macos' | 'windows' | 'linux';
	/** The shell can open the firewall for komiq (`open_firewall`). */
	automatic: boolean;
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
	/** Null when the shell cannot say (it predates the command): no automatic help. */
	async firewallInfo(): Promise<FirewallInfo | null> {
		try {
			const info = await invoke!<FirewallInfo>('firewall_info');
			return info && typeof info.os === 'string' ? info : null;
		} catch {
			return null;
		}
	},
	/** Resolves with a sentence saying what was done; rejects with a plain reason. */
	openFirewall(): Promise<string> {
		return invoke!<string>('open_firewall');
	},
};

/** A rejected command's reason: Tauri rejects with the command's error string. */
export function bridgeError(err: unknown, fallback: string): string {
	if (typeof err === 'string' && err) return err;
	return err instanceof Error && err.message ? err.message : fallback;
}

/**
 * The setup code handed from `/start` to the wizard. Session-only, never persisted. It
 * stays until the owner exists, so going Back or reloading the owner step still has it:
 * the app hands it out once, and its user has no log to read it from.
 */
const CODE_KEY = 'komiq-setup-code';

export function handOffSetupCode(code: string | null | undefined): void {
	try {
		if (code) sessionStorage.setItem(CODE_KEY, code);
	} catch {
		/* ignore */
	}
}

export function handedSetupCode(): string {
	try {
		return sessionStorage.getItem(CODE_KEY) ?? '';
	} catch {
		return '';
	}
}

/** The owner exists: the code is spent. */
export function forgetSetupCode(): void {
	try {
		sessionStorage.removeItem(CODE_KEY);
	} catch {
		/* ignore */
	}
}
