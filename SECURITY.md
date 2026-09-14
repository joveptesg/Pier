# Security Policy

## Reporting a Vulnerability

**Please do not open a public issue for security reports.**

Two ways to reach us, either is fine:

1. **GitHub private vulnerability reporting** — use the **Report a vulnerability**
   button on the [Security tab](https://github.com/joveptesg/Pier/security/advisories).
   Preferred: it keeps the report, the discussion, and the eventual advisory in
   one place, and it lets us add you as a collaborator on the draft.
2. **Email** — [info@devcom.app](mailto:info@devcom.app).

You do not need to ask permission first. Send the report.

## What to include

The more of this you can supply, the faster we can act:

- The affected file and function, or the endpoint and HTTP method.
- The commit or release you looked at.
- What an attacker gains, concretely — not just the bug class.
- A proof of concept, if you have one. If you could not build or run Pier and
  are reporting from source review alone, say so plainly. That is a perfectly
  good report; we would just rather know which parts you verified and which
  you inferred.

## What to expect

- **Acknowledgement within 3 working days.**
- **An initial assessment within 10 working days** — whether we can reproduce
  it, our severity read, and a rough fix timeline.
- We will tell you when a fix ships and give you the commit and release.

If you disagree with our severity assessment, say so and show your reasoning.
We would rather argue it out with you before publication than after. When we
publish an advisory we state our own score and the reasoning behind it, so a
disagreement will be visible either way — better that it is a resolved one.

## Disclosure

We aim to ship a fix and publish a GitHub Security Advisory within **90 days**
of a valid report, usually much sooner. If you intend to disclose publicly, or
you have already shared the report with a third party such as a CERT, bug
bounty platform, or another vendor, please tell us in the first message. That
is your right and it does not change how we handle the report — but knowing
about it changes our timeline, and we would rather not learn it afterwards.

If we go quiet for more than 30 days, treat that as a failure on our side and
disclose as you see fit.

## Scope

**In scope** — the Pier core panel and API, `pier-agent`, `pier-net-helper`,
the installer and enrollment scripts, and the default configuration they
produce.

**Out of scope:**

- Vulnerabilities in Docker, Traefik, or other third-party software Pier
  deploys or depends on. Report those upstream.
- Findings that require an operator to have already given the attacker root,
  panel Admin, or the agent token.
- Missing hardening headers, TLS cipher preferences, and similar scanner
  output, unless you can show concrete impact.
- Denial of service through raw traffic volume.
- Self-hosted instances that a third party has misconfigured. Tell the
  operator, not us — unless Pier's own defaults caused the misconfiguration,
  in which case we very much want to know.

## Credit

We credit reporters by name in the advisory and the release notes, unless you
ask us not to. Tell us how you would like to be named.

We do not run a paid bug bounty.
