# v0.1.0

Initial release of `nian-workspace`, a workspace-scoped MCP server for local coding clients.

Highlights:

- stdio and loopback-only Streamable HTTP transports;
- read-only-by-default filesystem, search, and Git inspection tools;
- explicit `--write`, `--exec`, and `--allow-shell` permission progression;
- hardened workspace-boundary handling, bounded command output, and process-tree timeout cleanup;
- Secure MCP Tunnel workflow for connecting a local workspace to supported remote MCP clients;
- release archives with SHA-256 checksums for native Linux x86_64 plus cross-linked Linux arm64 and Windows x86_64 GNU builds.
