# Security policy

Kiln runs AI agents with access to your code and credentials, so security
reports are very welcome.

## Reporting a vulnerability

Please **don't open a public issue**. Use GitHub's private reporting instead:
<https://github.com/chhhee10/kiln/security/advisories/new>.

Include what you found, how to reproduce it, and the impact you expect. You'll
get an acknowledgement within a few days.

## Scope

In scope: the daemon (`kilnd`), runner, sandbox hardening, credential vault,
authentication/pairing, memory visibility rules, and the clients.
Kiln is designed to listen on a private network (Tailscale) only; exposing it
to the public internet is unsupported.
