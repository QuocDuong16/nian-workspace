<!-- SERENA:START -->

## Codebase navigation with Serena

Serena MCP is the primary tool for understanding and navigating this repository.

Before manually exploring source code:

1. Use Serena memories to recover stable project context when relevant.
2. Use Serena semantic tools to locate symbols, references, implementations, and file-level symbol overviews.
3. Use Serena to narrow the task to the smallest relevant set of symbols/files.
4. Read the actual source for the symbols being changed or where implementation details must be verified.
5. Treat source code as authoritative; Serena memories and indexes are navigation/context aids.

Prefer Serena for:
- symbol lookup
- references and callers
- implementations
- symbol/file overviews
- architecture exploration
- locating relevant tests
- semantic code navigation

Do not:
- scan the repository tree manually before trying Serena
- use grep/find/ripgrep for code-symbol discovery when Serena can answer it
- reread broad areas of the codebase that are already covered by Serena memories
- use OpenWiki or generated wiki documentation for codebase understanding

Fallback to normal file/search tools only when:
- Serena cannot resolve the target
- the target is non-code content not represented by the language server
- exact text search is specifically required
- Serena results appear stale or incomplete

When modifying code, always verify the relevant implementation in source before editing.

## Serena memories

- Read `mem:core` first when broader project context is needed.
- Follow referenced memories progressively; do not load all memories by default.
- Update memories only for stable, non-obvious project knowledge that would otherwise require expensive rediscovery.
- Do not store transient task details, exact dependency versions, or line-level implementation facts in memories.

## Serena memory maintenance

After making a change, update Serena memories only when the change
invalidates stable project knowledge already stored there.

Do not update memories for:
- routine dependency/version bumps
- lockfile changes
- formatting changes
- one-off implementation details
- transient task state

Update memories when changing:
- architecture or component boundaries
- stable project conventions
- build/test/development workflow
- authentication or authorization invariants
- persistent operational assumptions
- other non-obvious knowledge that future agents would otherwise
  need to rediscover

<!-- SERENA:END -->