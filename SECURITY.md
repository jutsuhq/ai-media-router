# Security Policy

## Reporting a vulnerability

Please report vulnerabilities privately through
[GitHub Security Advisories](https://github.com/jutsuhq/ai-media-router/security/advisories/new),
not in a public issue. Include the version (`ai-media-router --version` or the commit), how you run it
(stdio MCP, `serve`, Docker) and the steps to reproduce.

We aim to acknowledge a report within 3 working days and to agree on a fix and disclosure date with you.
Credit is given in the advisory unless you ask otherwise.

## Supported versions

The project is pre-1.0. Only the latest release and `main` receive security fixes.

## Security model

- **Your keys stay local.** Provider keys are read from the environment or `config.toml` `[env]` and are sent
  only to the hosts that provider's spec names.
- **`serve` binds to loopback.** `ai-media-router serve` refuses a non-loopback `--bind` address unless
  `AI_MEDIA_ROUTER_TOKEN` is set. With the token set, every REST and MCP request needs
  `Authorization: Bearer <token>`, and `save_spec` / `reset_spec` stay disabled unless `--allow-spec-writes`
  is given. Put a TLS-terminating proxy in front of any server reachable from another machine.
- **Provider specs are code-like.** A spec decides which URLs receive your keys and files. Review a third-party
  spec before loading it, the same way you would review a script.

Issues that are in scope: key leakage, auth bypass on `serve`, path traversal in the library or exports,
spec handling that sends credentials to an unexpected host, and command injection through ffmpeg or `command` auth.
