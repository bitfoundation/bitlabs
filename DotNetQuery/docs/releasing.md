# Versioning and release policy

## Semantic versioning

Every package follows [SemVer 2.0](https://semver.org/). All packages are versioned and released together, from the
single `VersionPrefix` in `Directory.Build.props`, so a set of `DotNetQuery*` packages with the same version
always works together. `VersionPrefix` holds the version being developed; CI packages carry a `-ci.<run>` suffix so they
are never mistaken for a release.

| Change | Before 1.0 | From 1.0 |
| --- | --- | --- |
| Breaking change to a public API or to documented behavior | minor (`0.9` -> `0.10`) | major |
| New feature, new public API, new optional parameter on a new overload | minor | minor |
| Bug fix, performance improvement, documentation | patch | patch |

"Public API" means everything recorded in the packages' `PublicAPI.Shipped.txt` / `PublicAPI.Unshipped.txt`
baselines, which the build enforces for every package: a change to the public surface fails the build until the
baseline is updated in the same pull request, so every API change is visible in review. Package validation
(`EnablePackageValidation`) checks that every target framework of a package exposes a compatible API and - once
`PackageValidationBaselineVersion` names the last published version - that a release stays binary-compatible with it.

"Documented behavior" includes the guarantees in the README (one fetch per key, ordered notifications, no stranded
observers, safe writes, offline awareness) and the semantics described in `docs/`. A fix that changes behavior
somebody may rely on is listed under **Changed** in the changelog and in the [migration guide](migration.md), even
when it ships in a minor release.

## What counts as breaking

- Removing or renaming a public type or member, changing a signature, or adding an abstract member to a public
  interface or abstract class that users implement.
- Changing a default (`StaleTime`, retry count, network mode, ...) or the meaning of an option.
- Changing the persisted format so that existing stored data cannot be read (persisted envelopes are versioned; use
  `Buster` and `Migrations` for application data).
- Raising the minimum supported target framework or a dependency's minimum major version.

Not breaking: adding members to concrete types, new overloads that do not create ambiguity, new optional features
that are off by default, internal changes, and new diagnostics IDs (`DNQ0xx`) at warning level.

## Supported frameworks

The libraries target the .NET **LTS** releases in support (currently `net8.0` and `net10.0`, the WPF and WinUI adapters
included) and run unchanged on the STS releases in between; CI runs the test suite on the newest .NET runtime (the
`net10.0` build rolled forward) so a new release is validated before it ships. A target for an STS release is added only
when an API needs it. Platform adapters follow their platform's support: MAUI targets the supported MAUI release
(`net10.0-*`), Uno the frameworks Uno supports (`net9.0`, `net10.0`). Support for a framework is dropped only in a
release that may break (minor before 1.0, major after), after the framework is out of Microsoft support.

## Deprecation

A public API is first marked `[Obsolete("Use X instead.")]` (a warning) in a minor release, kept for at least one
further minor release, and removed in the next breaking release. The changelog lists deprecations.

## Releasing

### One-time setup (repository owner)

DotNetQuery lives in the `DotNetQuery/` folder of the [bitlabs](https://github.com/bitfoundation/bitlabs) repository,
which hosts several projects. Its CI is the DotNetQuery workflow (`.github/workflows/dotnetquery.yml` at the repository
root); there is no release workflow, so packages are published by hand from a clean checkout of the release commit.

1. **nuget.org** - resolve the package ID collision (`DotNetQuery` and `DotNetQuery.Blazor` currently belong to other
   accounts), reserve the `DotNetQuery.` prefix if possible, and create an API key scoped to the `DotNetQuery*`
   packages for publishing.

### Each release

Run the commands from the `DotNetQuery/` folder.

1. All jobs of the DotNetQuery workflow green on `main`: build, tests (core and Blazor on both target frameworks,
   generator, WPF/WinUI/Uno, Playwright browser tests), coverage, vulnerability scan, demo smoke tests, AOT harness,
   trimmed WebAssembly publish, MAUI build.
2. On a release branch, run `eng/prepare-release.ps1 -Version X.Y.Z` (PowerShell 5.1 or 7; `-DryRun` previews it). It
   moves every `PublicAPI.Unshipped.txt` entry into `PublicAPI.Shipped.txt`, ships the analyzer rules of
   `AnalyzerReleases.Unshipped.md` as `## Release X.Y.Z` in `AnalyzerReleases.Shipped.md`, sets `VersionPrefix`, and
   dates the changelog's `## Unreleased` section.
3. Update the [migration guide](migration.md), review the diff, merge to `main`.
4. On Windows, from the merge commit, build, test and pack every package; the MAUI adapter is outside the solution and
   needs the `maui-android` workload:

   ```bash
   dotnet build DotNetQuery.slnx -c Release
   dotnet test DotNetQuery.slnx -c Release --no-build
   dotnet pack DotNetQuery.slnx -c Release --no-build -o artifacts
   dotnet pack src/adapters/DotNetQuery.Maui -c Release -o artifacts
   ```

   For a release (not a pre-release), check that no `PublicAPI.Unshipped.txt` has entries left.
5. Push the packages: `dotnet nuget push "artifacts/*.nupkg" --api-key <key> --source https://api.nuget.org/v3/index.json`
   (the symbol packages next to them are pushed with them).
6. Tag the merge commit `dotnetquery-vX.Y.Z` (or `dotnetquery-vX.Y.Z-preview.N` for a pre-release; the `dotnetquery-`
   prefix keeps the tags apart from the other projects of the repository), push the tag, and create a GitHub release
   for it titled `DotNetQuery X.Y.Z` with the changelog section of the version and the packages attached.
7. After the first release of a line, set `PackageValidationBaselineVersion` in `Directory.Build.props` to it.
8. Bump `VersionPrefix` to the next version under development and open a new `## Unreleased` changelog section.

## Pre-releases

Previews use SemVer pre-release labels (`0.9.0-preview.1`): with `VersionPrefix` 0.9.0, pass
`-p:VersionSuffix=preview.1` to `dotnet build` and both `dotnet pack` commands, and tag `dotnetquery-v0.9.0-preview.1`.
Pre-release APIs may change between previews; they are frozen when the release is cut.
