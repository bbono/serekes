# Serekes Bot - Project Rules

## Architecture Principles
- Always separate business logic from infrastructure logic
- All timestamps must be in millisecond precision; convert any non-ms timestamps to ms
- Always use **Hexagonal Architecture (ports & adapters)** design principles when introducing changes to code

## Project Structure
- Bot source code: `./src/`
- Configuration: `./config.toml`
- `.sample-code/` contains code samples only — NOT part of the solution. Use only when explicitly asked.
- `.temp/` — never read files in this folder

## Documentation (docs/)
Generated/updated docs that must be kept in sync with code changes:
- `docs/WORKFLOW.md` — detailed bot workflow: how and when things happen, with workflow diagrams
- `docs/ARCHITECTURE.md` — software architecture of the serekes bot, including architecture diagram

## Diagram Rules
- Always use **Mermaid** diagrams in `.md` markdown documents (fenced with ` ```mermaid `)
- Do NOT use ASCII/box-drawing diagrams in markdown files
