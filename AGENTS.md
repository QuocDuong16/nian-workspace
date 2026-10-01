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
- Treat Serena memories as maintained project knowledge, not task logs or generated documentation.
- Store only stable, non-obvious knowledge that would otherwise require meaningful rediscovery from the codebase.
- Prefer updating an existing relevant memory over creating a new one.
- Create a new memory when durable project knowledge has no appropriate existing home.
- Do not store transient task details, exact dependency versions, or line-level implementation facts in memories.
- Source code remains authoritative if a memory and implementation disagree.

## Serena memory maintenance

After making a code or configuration change, explicitly review whether the completed work changes durable project knowledge.

Update Serena memories when the change:
- makes an existing memory inaccurate or incomplete
- introduces new stable architecture or component boundaries
- introduces or changes project-wide conventions
- changes build, test, development, deployment, or operational workflows
- changes authentication or authorization invariants
- introduces important integration behavior or cross-component contracts
- introduces persistent operational assumptions
- establishes other stable, non-obvious knowledge that future agents would otherwise need to rediscover

Do not update memories for:
- routine dependency/version bumps
- lockfile changes
- formatting or mechanical refactors
- isolated bug fixes whose behavior is obvious from the code
- one-off implementation details
- transient task state
- facts that can be trivially recovered from a single source file

Before considering a task complete:

1. Check whether any Serena memory used during the task is now inaccurate or incomplete.
2. Check whether the implementation introduced durable project knowledge not currently represented in Serena memories.
3. Update the relevant existing memory when appropriate.
4. Create a new memory only when the knowledge is durable and no existing memory is an appropriate home.
5. If no durable project knowledge changed, do not write a memory merely to record that the task occurred.

A task is not complete if it leaves relevant Serena memories materially inconsistent with the implemented code.

<!-- SERENA:END -->