# Security

## Reporting a vulnerability

Use **Report a vulnerability** in this repository's GitHub **Security** tab if
private reporting is available. If it is unavailable, open an issue requesting
a private reporting channel without exploit details or sensitive data. This
repository does not currently publish a separate security email address.

Include the affected version or commit, operating system, transport, deployment
configuration, and minimal reproduction steps. Redact credentials, private
paths, and conversation contents. Please avoid public exploit details until a
fix or disclosure plan has been discussed.

## Support scope

Security fixes target the current development branch and latest release. Older
versions have no guaranteed backports, and there is no response-time commitment.
Use a current checkout or release when reproducing an issue.

attyd is a single-user ACP client with no application login, authorization, or
tenant isolation. It binds to loopback by default. Host and browser Origin checks
reduce DNS-rebinding and cross-origin exposure; `--allowed-origin` supports
explicit proxy origins and does not authenticate users. Protect non-local
deployments with a trusted network boundary or authenticated reverse proxy.
Embedding the web interface in an iframe is unsupported and blocked by response headers.

Configured filesystem roots and `--read-only` apply to attyd's client operations.
They do not sandbox Agent processes, MCP providers, or terminal commands. ACP
Agent sign-in controls the Agent account, not access to the attyd service.

Issues in attyd's protocol handling, browser interface, host services, and
dependency integration belong here. Agent- or MCP-server-specific issues should
also be reported to their maintainers. See [deployment guidance](docs/usage.md#deployment-and-trust-boundaries).
