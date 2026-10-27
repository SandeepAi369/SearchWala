# Security Policy

## Supported versions

Only the latest release on `main` receives security fixes.

| Version | Supported |
|---|---|
| 8.1.x | ✅ |
| < 8.1 | ❌ |

## Reporting a vulnerability

**Please don't open a public issue for security problems.**

Report privately through GitHub:
[**Report a vulnerability**](https://github.com/SandeepAi369/SearchWala/security/advisories/new)

Include what you found, how to reproduce it, and the impact you expect. You'll
get an acknowledgement within 72 hours and a fix or a plan within 14 days for
confirmed issues. Credit is given in the advisory unless you'd rather stay
anonymous.

## In scope

SearchWala relays your LLM API key and fetches URLs taken from search results,
so these matter most:

- SSRF: reaching private, loopback, link-local or cloud-metadata addresses
  through page fetches, redirects, DNS rebinding or the LLM `base_url`
- API keys leaking into logs, error messages, responses or another provider
- Injection through scraped page content into the UI (XSS) or the LLM prompt
  that escapes the answer view
- Memory-safety or denial-of-service bugs reachable from remote pages or
  search engine responses

Running the server on `0.0.0.0` without an authenticating reverse proxy is
unsupported (see the README); reports that depend on it are out of scope.

## How dependencies are kept safe

- `cargo audit` runs against the RustSec database on every dependency change
  and daily.
- Dependabot opens update PRs for Cargo crates and GitHub Actions weekly.
- CodeQL scans the Rust code and the workflows; OpenSSF Scorecard grades the
  repository's practices.
- Every GitHub Action is pinned to a full commit SHA.
