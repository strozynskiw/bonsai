# Security policy

## Supported versions

Until 1.0, only the newest published Bonsai prerelease receives security fixes. After
1.0, the latest 1.x release line will receive security fixes; older alpha, prerelease,
and minor versions may be required to upgrade.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub's private
[security advisory form](https://github.com/strozynskiw/bonsai/security/advisories/new)
and include the affected version, platform, impact, reproduction steps, and any known
workaround. Do not include real credentials, private source code, or user transcripts.

You should receive an acknowledgement within five business days. We will validate the
report, coordinate a fix and disclosure timeline, and credit reporters who want to be
named. Please allow time for a patched release before publishing details.

## Scope

Credential exposure, sandbox or permission bypass, unintended external side effects,
untrusted-content instruction injection, cross-workspace data access, release-signature
bypass, and session/database data loss are security-sensitive. General model quality,
unsupported platforms, and provider outages belong in the normal issue tracker unless
they create one of those impacts.
