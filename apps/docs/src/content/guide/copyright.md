---
title: Content and copyright
description: Understand the global server’s copyright policy and the separate responsibilities of people who self-host.
---

This page summarizes copyright policies for the global server operated by the komiq project, including the work-in-progress demo at `komiq.cc`. The authoritative legal document is `COPYRIGHT.md` in the root of the repository. If you self-host komiq, you run your own server and need your own copyright policies, compliance processes, and notice procedures.

## The role of komiq

The komiq project develops open-source reader software and operates the global server as community-configured intermediary infrastructure.

The software indexes manga and comic metadata and provides an interface for reading material hosted on third-party websites. When a user opens a series or chapter, komiq retrieves covers and page images on demand from external sources. In the web application, images pass through an image proxy and may be stored temporarily in Cloudflare edge cache.

The global server is not intended to operate a permanent central archive of comic image files. It does not provide file upload tools or bulk download archives.

Komiq does not claim ownership of any third-party works, characters, titles, or trademarks. Komiq does not grant licenses to any third-party content. The availability of a source in the catalog does not mean that komiq endorses that source or has verified that its content is authorized.

## Rules for users and sources

You may only add and access sources and material that you are legally authorized to use. You must comply with applicable copyright laws, licensing restrictions, and source terms of service.

You may not use komiq to infringe copyright, circumvent technical access controls, misrepresent source permissions, or interfere with source servers.

Community privileges to add or manage sources are conditional. Komiq reserves the right to restrict privileges, suspend accounts, block specific URLs, or remove sources entirely to address copyright infringement, security risks, or legal requirements.

## How a rights holder reports infringement

Only a copyright owner or an agent authorized to act on behalf of the owner should submit a copyright notice.

To submit an actionable notice, include the following six pieces of information:

1. A physical or electronic signature of the copyright owner or their authorized representative.
2. Clear identification of the copyrighted work claimed to be infringed, or a representative list if multiple works are covered.
3. Specific identification of the material claimed to be infringing, with sufficient detail for komiq to locate it. Include specific komiq URLs, source names, series titles, chapter numbers, and original upstream URLs.
4. Your contact details, including your full legal name, organization, mailing address, telephone number, and email address.
5. A statement that you have a good-faith belief that the disputed use is not authorized by the copyright owner, its agent, or the law.
6. A statement that the information in the notice is accurate and, under penalty of perjury, that you are the copyright owner or authorized to act on behalf of the owner.

### Notice contact status

The komiq project has not yet published a dedicated copyright notice submission channel or registered a designated DMCA agent with the United States Copyright Office.

Do not post formal copyright notices in public GitHub repository issues, pull requests, community discussions, or personal developer social media accounts. Registering a designated DMCA agent and publishing formal contact details are required before the demo service is operated as a public production service.

## How komiq handles reports

When komiq receives a specific, good-faith copyright notice through an approved channel, its policy is to review the notice promptly and take appropriate technical action.

Depending on the report and technical capabilities, actions may include:

- Disabling access to specific komiq URLs.
- Removing or suspending an external source from the catalog.
- Revoking trusted source-submission privileges from a user.
- Suspending or restricting user accounts involved in the violation.

Komiq may notify the affected user or source submitter and may forward the notice to them. Notices and related correspondence are retained to maintain legal records, handle counter-notices, and enforce repeat-infringer policies.

Submitting a notice does not guarantee removal. Notices that are incomplete, lack locating details, or appear fraudulent may be rejected.

## Counter-notices

If a user believes that material was removed or disabled by mistake or misidentification, they may submit a counter-notice once the formal contact channel is published.

A valid counter-notice must include:

1. The user's physical or electronic signature.
2. Identification of the material that was disabled and its location before removal.
3. A statement under penalty of perjury that the user has a good-faith belief that the material was restricted due to mistake or misidentification.
4. The user's name, address, telephone number, and consent to federal court jurisdiction as specified under 17 U.S.C. § 512(g)(3).

Komiq may forward the counter-notice to the original claimant. Material is restored only when appropriate under applicable law and service policies.

## Repeat-infringer policy

Komiq will terminate accounts and revoke source-adding privileges of users who repeatedly infringe copyright or repeatedly submit infringing sources.

Komiq evaluates the number, validity, and severity of notices, counter-notices, and deliberate abuse patterns. Users who have their privileges revoked may not evade enforcement by creating new accounts. If narrower measures fail to stop repeated infringement, an entire source may be disabled across the global server.

## Self-hosted deployments

This policy applies solely to the global server’s services operated by the komiq project.

People who self-host run their own servers independently. The komiq project cannot remove content or modify settings on servers run by third parties. Rights holders who identify issues on a self-hosted instance must contact the administrator of that specific server.

## Next steps

Review related policies and documentation:

- [Privacy policy](/guide/privacy): Learn how the global server handles user data.
- [What is komiq](/start/what-is-komiq): Understand the role and scope of the software.
- [Choose your setup](/start/choose-your-setup): Learn about self-hosting your own server.
