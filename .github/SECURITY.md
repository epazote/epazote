# Security Policy

## Supported Versions

Security updates are provided for the latest release line only. Users on older
releases are encouraged to upgrade before reporting an issue.

| Version | Supported          |
| ------- | ------------------ |
| 4.0.x   | :white_check_mark: |
| < 4.0   | :x:                |

## Reporting a Vulnerability

Please do not open a public issue, pull request, or discussion for security
vulnerabilities.

Report privately through GitHub's
[Report a vulnerability](https://github.com/epazote/epazote/security/advisories/new)
form, which is enabled for this repository. If you cannot use GitHub, email
[nbari@tequila.io](mailto:nbari@tequila.io) instead.

Include, when possible:

- A description of the vulnerability and its potential impact
- Steps to reproduce the issue or a proof of concept
- The affected version (`epazote --version`) and platform
- Any suggested mitigation or fix

You can expect an initial response within 48 hours and a status update within
seven days. The time required for a fix depends on the vulnerability's severity
and complexity.

Please allow time for a fix to be released before publicly disclosing the
vulnerability. Reporters are credited in the published advisory unless they ask
otherwise.

## Scope

The configuration file is trusted input. Epazote runs `cmd` values from the
configuration through the system shell by design (`$SHELL`, falling back to
`sh`), so a configuration file that an untrusted party can write is equivalent
to giving that party shell access. Reports that rely on supplying a malicious
configuration, or on a hostile `$SHELL`, are out of scope.

In scope are, for example, issues in how Epazote handles responses from
supervised services, parses untrusted HTTP data, or leaks configured `headers`
and other credentials into logs, metrics, or outbound requests.
