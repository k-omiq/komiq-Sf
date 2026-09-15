---
title: Auth and security
description: Learn how your komiq server handles accounts, sessions, password hashing, security policies and first-run setup.
---

Komiq is self-hostable, and each server handles its own accounts and sessions. The server enforces strict authentication, stores credentials safely at rest, restricts network capabilities through Content Security Policy (CSP), and generates a one-time code during [first-run setup](/self-host/first-run).

This page explains how accounts and sessions work, how clients store bearer tokens, how passwords are validated and hashed, what security headers are sent, and how the setup code protects fresh installations.

## Accounts and sessions

User identity is defined by the `User` struct in `apps/server/src/auth.rs`. Each account record contains an identifier, username, email, password hash, avatar reference, and flags for admin status, ban state, and password change requirements.

Usernames are validated during registration. They may contain letters, digits, underscores, hyphens, and periods. A database trigger and case-insensitive uniqueness check (`COLLATE NOCASE`) ensure that two users cannot register variations of the same name with different casing.

Admin status is granted through environment configuration (`KOMIKA_ADMIN_USERS`) or through the initial setup code. Regular user registration cannot self-elevate to admin status.

When an account is banned, its active sessions become invalid immediately. Token lookup queries check `u.is_banned = 0` on every request.

Temporary passwords can be assigned when an admin creates a user or resets an account password. The server marks the user with `must_change_password = 1`. Until the user submits a new password through the `changePassword` mutation, the server treats their session token as anonymous for all other API actions.

### Session tokens

When a user signs in, the server generates an opaque 256-bit session token using operating system randomness (`OsRng`) in `apps/server/src/auth.rs`:

```rust
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}
```

The server returns the plaintext hex token to the client. The database never stores the plaintext token. Instead, the server hashes the token using SHA-256 (`hash_token`) and stores only the hex digest in the `sessions` table.

Because only hashes are stored, a leaked database backup cannot be replayed as a bearer token. When authenticating an API request, the server hashes the incoming token from the `Authorization: Bearer <token>` header and searches for matching unexpired session rows.

Sessions carry an absolute expiration timestamp (`expires_at`) stored in ISO-8601 format (`YYYY-MM-DDTHH:MM:SSZ`). By default, sessions remain valid for 30 days (`SESSION_TTL_SECS`).

## Client token storage

Both web clients receive a plaintext bearer token upon successful login.

In the [reader app](/developers/reader-app), authentication state is managed in `apps/reader/src/lib/auth.svelte.ts`. The token is stored in `localStorage` under the key `komika-token`.

To prevent visual layout shifts on initial load, `apps/reader/src/lib/context.ts` restores the persisted token synchronously at module evaluation before SvelteKit runs the first route `load` function. Components then validate the token and populate user information via `initAuth`.

In the [admin app](/developers/admin-app), authentication state is managed in `apps/admin/src/lib/auth.svelte.ts`. The token is stored in `localStorage` under `komika-admin-token`. On load, `initAuth` validates the session against the server and verifies that `user.isAdmin` is true. If the account lacks admin privileges, the token is discarded.

Storing tokens in `localStorage` means scripts executing on that origin can read them. To defend against cross-site scripting (XSS), komiq avoids dangerous patterns:

- Neither app uses `eval`, `document.write`, or `innerHTML`.
- Svelte automatically escapes expressions rendered in templates.
- Content Security Policy headers restrict script execution to hashed scripts and trusted origins.
- External URLs rendered in link `href` attributes pass through strict protocol validation to reject unsafe schemes like `javascript:`.

## Password rules and hashing

Password validation and hashing logic is located in `apps/server/src/auth.rs` and `apps/server/src/graphql/mod.rs`.

New passwords must satisfy length requirements:

- Minimum length: 8 characters (`validate_new_password`).
- Maximum length: 1024 characters (`MAX_PASSWORD_LEN`).

When changing an existing password, the new password must differ from the current password.

On login and registration, passwords exceeding 1024 characters are rejected immediately. This check occurs before running any cryptographic functions, preventing attackers from consuming server resources with oversized inputs.

### Argon2id configuration

Passwords are hashed using Argon2id with a unique cryptographic salt generated by `OsRng`. Hashes are stored as Password Hashing Competition (PHC) formatted strings.

The hashing parameters are:

- Memory: 64 MiB (65536 KiB)
- Iterations: 3 passes (`t = 3`)
- Parallelism: 1 thread (`p = 1`)

Existing hashes verify correctly even if parameters change because `verify_password` uses the parameter values recorded in each stored PHC string.

### Defense against timing attacks and exhaustion

The server implements several safeguards around password processing:

- Constant-time verification: When an unknown username attempts to sign in, the server verifies the submitted password against a static dummy hash. This ensures that failed login attempts take the same amount of time whether the account exists or not, preventing username enumeration.
- Bounded concurrency: A Tokio semaphore (`password_work`) limits the number of concurrent 64 MiB Argon2 jobs that can run across all client requests.
- Worker threads: Cryptographic verification runs inside `tokio::task::spawn_blocking` so expensive hashing loops do not block the asynchronous event loop.
- Rate limiting: An in-memory sliding-window limiter (`auth_limiter`) throttles login and registration requests by client IP address.

## Security headers and CSP

Security headers are enforced at the server layer and at the web application edges.

### Server HTTP headers

In `apps/server/src/main.rs`, Axum middleware attaches security headers to every HTTP response:

- `X-Content-Type-Options: nosniff`: Prevents MIME-type sniffing by browsers.
- `X-Frame-Options: DENY`: Blocks clickjacking by preventing the API from being embedded in frames.
- `Referrer-Policy: no-referrer`: Omits referrer information in outbound requests.
- `Permissions-Policy`: Disables sensitive browser features including geolocation, camera, microphone, payment, and USB APIs.

Cross-origin resource sharing (CORS) permits designated reader origins through `CORS_ORIGINS`. For your own server, configure the origins of your reader deployments. The native reader apps send requests from `tauri://localhost`, `http://tauri.localhost` or `https://tauri.localhost`; a server that does not list them cannot be chosen on the app's connect screen. The self-host Docker image (`deploy/selfhost/Dockerfile`) and the admin desktop app list them. For now, the global server lists the `komiq.cc` site only, on purpose: it is a shared demo, and an API open to other apps is open to abuse. Opening it to the apps later is a change to that list. Anonymous `GET /graphql` responses also receive `Access-Control-Allow-Origin: *`.

### Reader Content Security Policy

The web reader configures its CSP in `apps/reader/vite.config.ts` using hash mode. SvelteKit calculates cryptographic hashes of inline bootstrap scripts and embeds them in the policy.

The reader CSP defines:

- `default-src 'self'`
- `script-src 'self'`
- `style-src 'self' 'unsafe-inline'`
- `img-src 'self' https: http: data: blob:`
- `connect-src`: Dynamically generated by `apps/reader/csp.ts` to allow connections to the configured GraphQL endpoint and image proxy Worker.
- `font-src 'self' data:`
- `object-src 'none'`
- `base-uri 'self'`
- `form-action 'self'`

The native reader apps use a separate policy in `apps/reader/src-tauri/tauri.conf.json`. Its `img-src` is `'self' blob: data: engine-image: http: https:`. The `engine-image:` entry lets the web view load engine page images from the app's own address scheme, as [The on-device engine](/developers/native-engine) describes. On Android, the loopback image listener's `http://127.0.0.1` address passes through the `http:` entry.

### Admin Content Security Policy

The global server serves its console at `admin.komiq.cc`. Its static headers in `apps/admin/static/_headers` pin `connect-src` to the global API at `https://api.komiq.cc`. These names describe the global server configuration.

The console also has a generated policy in `apps/admin/vite.config.ts`. Together, these policies define:

- `default-src 'self'`
- `script-src 'self'`
- `style-src 'self' 'unsafe-inline'`
- `img-src 'self' https: http: data:`
- `connect-src`: The global server's static header allows `'self'` and `https://api.komiq.cc`. The Vite policy allows `'self'`, `https:` and `http:`. Desktop builds also allow `ipc:` and `http://ipc.localhost` for Tauri inter-process communication.
- `frame-ancestors 'none'`
- `base-uri 'none'`
- `object-src 'none'`

For self-hosting, the embedded console calls `/graphql` on its own origin. The desktop console uses the server selected at runtime. If you serve a separate-origin console through Cloudflare Workers Assets, adapt the static header to your own API origin, such as `https://komiq.example.com`. The header's global API address does not change with `PUBLIC_KOMIKA_API`.

The reader's web CSP derives connection origins from its public build settings. Your configured API and optional Worker origins take the place of the global server's addresses. Same-origin API paths need no extra origin permission.

## First-run setup code

When a new komiq server starts, it checks whether any administrative account exists in the database using `admin_exists(pool)` in `apps/server/src/instance.rs`.

If no admin account exists, the server issues a one-time setup code via `SetupCode`:

1. Code generation: The server picks 12 symbols at random using `OsRng` from the Crockford base32 alphabet (`0123456789ABCDEFGHJKMNPQRSTVWXYZ`). The alphabet excludes the letters I, L, O, and U to prevent visual confusion. 12 symbols provide 60 bits of entropy.
2. Logging: The server formats the code into three groups of four (`XXXX-XXXX-XXXX`) and prints it once to standard output on startup.
3. In-memory protection: The server never writes the plaintext code to disk and never stores it in the database. Only its SHA-256 hash is kept in an in-memory `Mutex<Option<String>>`. No API query can read the code.
4. Redemption: The administrator enters the code into the initial setup screen, which calls the `createOwner` GraphQL mutation. The mutation normalizes the code by removing spaces and dashes, converts characters to uppercase, and compares the SHA-256 digest against the in-memory value.
5. Code retirement: As soon as the owner account is created, `clear()` removes the code from memory. Restarting an unconfigured server generates a fresh code.

Attempts to redeem the code through `createOwner` are rate-limited to prevent brute-force attacks.

## Where to look

- `apps/server/src/auth.rs`: User data structures, Argon2id hashing, token generation, and token hash lookups.
- `apps/server/src/instance.rs`: Setup code generation, verification, and retirement logic.
- `apps/server/src/graphql/mod.rs`: Password validation, dummy-hash timing attack defenses, and authentication rate limiters.
- `apps/reader/src/lib/auth.svelte.ts`: Reader client token persistence and authentication store.
- `apps/reader/src/lib/context.ts`: Synchronous token restoration before route execution.
- `apps/admin/src/lib/auth.svelte.ts`: Admin console token management and admin-role verification.
- `apps/reader/csp.ts`: Dynamic CSP `connect-src` calculation for the reader web build.
- `apps/admin/static/_headers`: Security headers and CSP definitions for the global server admin console.
- `apps/server/src/main.rs`: Server-level security headers and CORS middleware configuration.
