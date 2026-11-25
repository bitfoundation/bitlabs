# bitlabs rules

Each top-level folder is a self-contained project. These rules apply to every project, and every new or moved-in
project must follow them from its first commit.

## Authorship

1. The author and copyright holder of every project is **Bit Foundation**, never a person or "<Project> contributors":
   - `LICENSE` in the project folder: a copy of the root [`LICENSE`](LICENSE) (`Copyright (c) 2026 Bit Foundation`).
   - .NET: `<Authors>Bit Foundation</Authors>` and `<Copyright>Copyright (c) 2026 Bit Foundation</Copyright>` in the
     project's `Directory.Build.props` (and `<Company>Bit Foundation</Company>` when `Company` is set).
   - Rust: `authors = ["Bit Foundation"]` in `Cargo.toml`; Windows version resources use `CompanyName` "Bit Foundation"
     and `LegalCopyright` "Copyright (c) 2026 Bit Foundation".
   - Any other manifest with an author, publisher or copyright field (package.json, VSIX, MSIX, ...) uses the same.
   - Third-party code vendored into a project keeps its original copyright and license.

## Project layout

2. Every project folder has its own `README.md`, `LICENSE`, `.gitignore` and `.editorconfig` (`root = true`), and a
   `CHANGELOG.md` with an `## Unreleased` section. Base a new `.editorconfig` on an existing one of the same stack
   (for .NET, [`BlazorTestPlatform/.editorconfig`](BlazorTestPlatform/.editorconfig) without its project-specific
   diagnostic suppressions; for Rust, [`DevDeck/.editorconfig`](DevDeck/.editorconfig)).
3. There are no repository-wide build files (`Directory.Build.props`, `Directory.Packages.props`, `global.json`): each
   project has its own.

## Links

4. Package and documentation links point into this repository, not to a personal or former repository:
   - `PackageProjectUrl`: `https://github.com/bitfoundation/bitlabs/tree/main/<Project>`
   - `RepositoryUrl`: `https://github.com/bitfoundation/bitlabs`
   - links to files: `https://github.com/bitfoundation/bitlabs/blob/main/<Project>/<path>`
   - MCP server names: `io.github.bitfoundation/<name>`, with `"subfolder": "<Project>"` in `server.json`.

## CI

5. Every project has a workflow `.github/workflows/<project in lowercase>.yml` named after the project. It runs on
   `push` to `main` and on `pull_request` filtered to `'<Project>/**'` and the workflow file itself, plus
   `workflow_dispatch`; sets `permissions: contents: read` (widened per job only where needed),
   `defaults.run.working-directory: <Project>` and `DOTNET_NOLOGO`/`DOTNET_CLI_TELEMETRY_OPTOUT` for .NET. Paths in
   `uses:` steps (global.json, artifacts, caches) are relative to the repository root, so they start with `<Project>/`.
   Artifact and cache names start with the lowercase project name.

## Root README

6. Add the project to the table in the root [`README.md`](README.md) (alphabetical order) and add a section with the
   build, test and run commands, the requirements, the link to its docs and the link to its CI workflow.

## Moving a project into bitlabs

7. Bring over everything the source repository tracks (compare `git ls-files` of both), convert its CI to rule 5,
   apply rules 1 to 6, and record the move in a "Moved into bitlabs" section of the project's `CHANGELOG.md`.

## Commits

8. Commit messages use conventional commits scoped to the project with a short lowercase name, as in
   `feat(devdeck): ...`, `feat(profiler): ...`, `feat(testplatform): ...` or `feat(dotnetquery): ...`; changes to
   `.github/` alone use `github`.
