---
title: Privacy and your data
description: Understand the global server’s privacy policy and the separate responsibilities of people who self-host.
---

This page summarizes the privacy policy for the global server operated by the komiq project, including the work-in-progress demo at `komiq.cc`. The authoritative legal document is `PRIVACY.md` in the root of the repository. If you self-host komiq, you run your own server and need your own privacy policies and practices.

## Scope of this policy

This summary covers only the global server’s services operated directly by the komiq project.

If you self-host komiq on your own computer, a home server, or a VPS, you control the infrastructure, data storage, and external connections. Self-hosters must establish their own policies and follow the laws applicable to their deployment.

## Information komiq processes

Depending on how you use the global server, komiq may process the following categories of information:

- **Account details:** Usernames, email addresses, internal account identifiers, administrative permissions, and password hashes. Passwords are hashed using argon2id and are never stored in plaintext.
- **Authentication records:** Opaque session tokens, account associations, and session timestamps.
- **Social contributions:** Ratings, reviews, comments, spoiler flags, and timestamps. Social contributions submitted to public features are visible to other users.
- **Reading activity and libraries:** The series you save in your library, your reading progress, chapter markers, and reading state.
- **Catalog and source state:** Source configurations, catalog metadata, scan records, and administrative overrides.
- **Device-local data:** Session tokens, preferences, and local reading data saved in your browser local storage or native app storage.
- **Operational and network logs:** IP addresses, browser user agents, request timestamps, security diagnostics, rate-limiting triggers, and server logs.
- **Direct communications:** Details provided in support requests, bug reports, or legal notices sent to the project.

Do not include sensitive personal details in public reviews, comments, or public repository issues.

## Why komiq uses information

Komiq uses the collected information for the following purposes:

- To provide reader features, library organization, and account management.
- To authenticate your account on the global server and keep you signed in.
- To display public community reviews and ratings.
- To manage catalog synchronization and source configurations.
- To protect service security, enforce rate limits, and investigate abuse.
- To diagnose server errors and maintain system reliability.
- To respond to legal inquiries and enforce project policies.

Komiq does not sell personal information. Komiq accepts no payments, charges no fees, and displays no advertising. The service operates no payment processing systems and collects no credit card or billing details.

## Third-party service providers

The global server relies on external infrastructure providers to deliver its features:

- **Cloudflare:** Cloudflare provides web delivery, security protection, rate limiting, and temporary edge caching for images. Cloudflare processes network and request metadata during these operations.
- **Hosting providers:** Server hosts process application data, network traffic, and database storage to run the service.
- **Backup storage:** When backups are enabled, encrypted transport and access controls protect copies of the account and social database stored with configured storage providers.
- **Suwayomi and third-party sources:** When you open a chapter in the web reader, the image proxy fetches images from third-party sources. In native applications, the app or local engine may contact source hosts directly, which exposes your IP address and standard request headers to that source.

Each third-party service provider operates under its own terms of service and privacy policies.

## Data retention

Komiq retains personal information as long as your account remains active or as needed to provide requested features. Some information may be retained longer to meet legal obligations, resolve disputes, prevent fraud, enforce copyright policies, or maintain backup integrity.

Temporary caches follow specific lifecycles:

- Device-local information stays on your device until you sign out or clear your browser or application cache.
- Cloudflare edge-cache entries are temporary and expire automatically.
- Komiq does not maintain a permanent central archive of manga or comic pages.

Production-specific retention schedules for active accounts and logs have not yet been published.

## Your choices and current limitations

You can read comics on the global server without creating an account. If you do not register, no account record is created.

You can delete local device data at any time by clearing your browser local storage or uninstalling the client application. Clearing local storage removes your saved session and any preferences stored exclusively on your device.

### Work in progress

Several privacy management controls are currently planned and under development:

- Automated account export and account deletion workflows are not yet fully implemented in the software.
- The project has not yet published a verified private channel for submitting data access, correction, or deletion requests.
- Do not submit personal identification documents or sensitive account information through public repository issues.
- A private contact channel and verified identity procedures must be published before the demo is operated as a production service.

## Security practices

Komiq implements technical security measures, including argon2id password hashing, opaque session tokens, strict source-host allowlists, request size limits, and rate limiting.

No security system is impenetrable. You should protect your account by choosing a unique password and keeping your session credentials confidential. Security vulnerabilities should not be reported through public channels; a private security reporting procedure is planned prior to public launch.

## Next steps

Learn more about policies and account management:

- [Copyright policy](/guide/copyright): Review the global server’s copyright and takedown policies.
- [User accounts](/guide/accounts): Learn how accounts and permissions work in komiq.
- [What is komiq](/start/what-is-komiq): Understand the purpose and structure of the project.
