# Komiq Privacy Policy

**Effective:** August 9, 2026  
**Applies to:** Komiq-operated services, including the work-in-progress demo at
`komiq.cc`

This policy describes the categories of information the current repository is
designed to process. Production configuration must be verified before public launch;
unfinished privacy controls are identified below. A self-hosted Komiq deployment has
its own operator, configuration, and privacy responsibilities and is not covered by
this policy.

## Information Komiq processes

Depending on the features and deployment configuration you use, Komiq may process:

- **Account information:** username, email address, account identifier, administrative
  status, and password hash. Komiq is designed to hash passwords rather than store
  plaintext passwords.
- **Authentication information:** opaque session tokens, user association, and session
  creation data.
- **Social content:** ratings, reviews, comments, spoiler flags, and timestamps. Content
  you submit to public community features may be visible to other users.
- **Reader and library activity:** source, series, chapter, library, progress, and
  reading-state information processed through Komiq and Suwayomi. The exact persistence
  and account separation depend on the selected backend.
- **Community-source and catalog state:** source configuration, source submitter or
  privilege information when implemented, catalog metadata, administrative overrides,
  scan state, and update history.
- **Device-local information:** authentication state, ratings, comments, preferences,
  and similar values may be stored in browser local storage or app data on your device,
  depending on the backend and build.
- **Network and operational information:** IP address and request metadata, timestamps,
  user agent, rate-limit events, security events, diagnostics, and server or proxy logs
  that Komiq, Cloudflare, or an infrastructure provider ordinarily processes to deliver
  and protect the service.
- **Communications:** information included in support, privacy, copyright, security, or
  other messages sent to the project after the relevant contact channels are published.

Please do not include sensitive personal information in public reviews, comments,
repository issues, or other public project channels.

## Why Komiq uses information

Komiq may use the information above to:

- deliver requested reader, source, library, account, and social features;
- authenticate users and maintain sessions;
- operate community source privileges and catalog updates;
- display public ratings, reviews, and comments;
- secure the service, enforce rate limits, prevent abuse, and investigate incidents;
- debug failures, maintain reliability, and understand aggregate service operation;
- respond to support, privacy, copyright, and legal requests; and
- comply with law and enforce project policies.

Komiq does not sell personal information. Donations support software development and
are not tied to reader activity, source access, or content access. A third-party
donation provider, if used, processes payment and account information under its own
privacy terms. The current repository links to an external donation page rather than
implementing payment-card processing inside Komiq.

## Third-party sources and service providers

Komiq relies on other systems to operate:

- **Cloudflare:** the web path may use Cloudflare for delivery, security, rate limiting,
  Workers proxying, and temporary edge caching. Cloudflare processes request and
  network information as part of those services.
- **Hosting and infrastructure providers:** configured hosts may process application,
  database, network, and security information to run the service.
- **Backup/storage providers:** when backup features are enabled, encrypted transport
  and access controls should protect copies of Komiq's account and social database, but
  the configured provider stores those backup objects.
- **Suwayomi, extensions, and third-party sources:** Komiq sends source, catalog, and
  media requests through these components. A native direct request may expose the
  user's IP address and ordinary request metadata to the source. A proxied web request
  generally exposes the proxy's request information to the source while Cloudflare and
  Komiq infrastructure process the user's request.

These third parties have their own terms and privacy practices. Komiq may also disclose
information when reasonably necessary to comply with law, protect rights or safety,
investigate abuse, complete a change in project ownership or operation, or act with
the user's direction or consent.

## Retention

Komiq retains information while an account is active or while it is reasonably needed
for the feature or purpose for which it was collected. Some information may be kept
longer when reasonably necessary for security, fraud prevention, dispute resolution,
copyright enforcement, legal compliance, or backup integrity.

Device-local information remains until the app, browser, or user removes it. Edge-cache
entries are temporary and expire or may be evicted under the configured cache policy;
Suwayomi, browser, app, and device caches or user-enabled downloads can use different
retention periods. Komiq is not intended to operate a permanent central media archive.
Deleting an active database record may not immediately remove copies from rolling
backups or security records. Komiq has not yet published production-specific retention
periods.

## Your choices and requests

You can avoid public social features, clear browser local storage through your browser,
or remove local app data through your device. Clearing local data may sign you out or
remove device-only preferences and contributions that were never synchronized.

### Privacy contact — WIP and public-launch blocker

Komiq has not yet published a private channel for access, correction, export, or
deletion requests. Do not place identity documents, account credentials, or other
sensitive request information in a public repository issue. A verified private contact
channel and request procedure must be published here before the demo is treated as a
public production service.

Account export and deletion are not yet evidenced as complete end-to-end features in
the repository. Komiq will describe available rights and verification steps after the
production operator, data flows, and applicable state laws have been reviewed.

## Security

Komiq uses measures represented in the repository such as password hashing, opaque
sessions, source-host allowlists, response size limits, rate limiting, and restricted
service exposure. No transmission, storage system, or security control is perfectly
secure. Users should use a unique password and should not share session tokens or
credentials.

Security issues should not be reported through the copyright process. A dedicated
private security-reporting channel is a separate pre-launch requirement.

## Privacy engineering roadmap (WIP)

The items below are planned work, **not current features or completed compliance
requirements**:

- [ ] Implement verified account access, export, correction, and deletion workflows.
- [ ] Publish production retention periods for accounts, sessions, social content,
      operational logs, enforcement records, edge caching, and backups.
- [ ] Define how account deletion propagates to backups and when retained copies expire.
- [ ] Publish the production operator identity, private privacy contact, and complete
      processor/subprocessor inventory.
- [ ] Create authenticated privacy-request tracking and identity-verification procedures.
- [ ] Verify production data flows for Cloudflare, Suwayomi, source extensions, hosting,
      backups, donations, logging, and analytics before launch.
- [ ] Review children's privacy, state privacy laws, and breach-response obligations
      with qualified U.S. privacy counsel before public production use.
- [ ] Add a private security reporting channel and incident-response procedure.

## Policy changes

Komiq may update this policy as the software, providers, and operated service change.
Material changes will be reflected in this file's effective date. Continued use after
an update is subject to the notice and consent requirements of applicable law.
