# Gap analysis, fifth pass: releases since the fourth pass

22 November 2026. The [fourth pass](Gap%20analysis%2C%20fourth%20pass.md) ended with every unblocked item done or
deferred with a reason. This pass looked at what shipped since mid-November, and at one major release the fourth pass
missed (Vitest 5). Versions and dates were checked against the NuGet API, GitHub releases and the projects' own
documentation. Every claim about this repository was checked against the code before an item was listed.

## What changed in the ecosystem

* **Playwright 1.65** (Node 1.65.0 on 2026-11-05, 1.65.1 on 2026-11-13; Microsoft.Playwright 1.65.1 on 2026-11-18).
  * .NET: `Locator.NormalizeAsync()` produces C# locator code, `PlaywrightAssertionException` is new, and there are new
    `ActionTimeout` and `NavigationTimeout` settings.
  * Breaking: actions wait for elements that are not inert, and text the browser does not render is left out of
    accessible names.
  * Node: a `failures` reporter and an `-exclusive-` test lock.
  * 1.66 is not out. Its main branch has a "blink" mode for screenshot diffs and a deferred-retry strategy.
* **Vitest 5.0** (2026-09-03) **and 5.1** (2026-11-02).
  * A trace view for browser mode, and the ARIA tree printed when a locator fails.
  * Benchmarks with stored baselines, and failing unawaited assertions.
  * 5.1 adds `--last-failed`, interactive snapshot review and failure-only traces.
* **Storybook 10.3 to 10.6.** Storybook MCP, sidebar review that knows about git changes, Vitest `initialGlobals` for
  theme, viewport and locale matrices, and AI-curated visual changesets.
* **Microsoft.AspNetCore.Components.Testing** stays prerelease (11.0.0-preview.7, 2026-11-10). Its project file says it
  does not ship a stable version with .NET 11. It is a Playwright, MSTest and YARP end-to-end library, not a component
  test library.
* **bUnit:** no release after 2.15.2. **axe-core:** 4.14.1 (2026-11-16) is a patch.
* **WCAG 3.0** is still a working draft; WCAG 2.2 AA stays the right target.

## Gap list and status

| # | Gap | Seen in | Status |
|---|---|---|---|
| 1 | Package pins behind what consumers resolve | `dotnet list package --outdated` | **Done**. AngleSharp 1.10.5 and AngleSharp.Css 1.5.0: bUnit 2.15.2 already required at least 1.10.1 and 1.4.1, so bUnit users ran untested newer versions. Also TUnit 1.87.12, and TimeProvider.Testing 11.0.1 for `net11.0` (10.11.0 stays for `net10.0`). Kept on purpose: Roslyn 4.14 (generator consumers would need a newer SDK), NUnit 4 as the build target (tested on NUnit 5), System.CommandLine 2.x (3.0 breaks its API) |
| 2 | A locator that finds nothing gives no hint | Vitest 5 | **Done**: assertions and actions that find no element append the story's accessibility tree (first 30 lines) to the error, in every driver |
| 3 | Flip between expected and actual images in place | Playwright 1.66 (main), Storybook | **Done** in the HTML report (a Blink button; with reduced motion it flips on each press). The hub already had a strobe mode |
| 4 | Playwright 1.65 | Playwright | **Done**: Microsoft.Playwright 1.65.1. Re-rendering in the `v1.65.1-noble` image changed no Linux baseline, and the comparison run passed 622 of 622. The breaking "actions wait for elements that are not inert" change broke nothing: the demo's dialog makes the page behind it inert, and stories act inside the dialog |
| 5 | axe-core 4.14.1 | axe-core | **Done** without forking the supply chain: `BTP_AXE_SCRIPT` / `accessibility.axeScript` runs a given axe-core script instead of the bundled one (verified with a stub that reports its version, and a non-axe file that fails clearly); the bundled engine stays the default until `Deque.AxeCore.Commons` ships 4.14.1 |
| 6 | Generate locator code for an element an agent picked (`NormalizeAsync`) | Playwright 1.65 | **Done** for failures: the not-found hint suggests the closest elements of the accessibility tree as locator code ("Did you mean: ctx.GetByRole(...)"), in every driver; the workbench recorder already writes locator code for elements picked by hand |
| 7 | Failure-only artifacts, rerun of the last failures, interactive baseline review | Playwright, Vitest 5.1 | Already present: `BTP_DIAGNOSTICS` defaults to keeping artifacts on failure only; `BTP_LAST_FAILED`; `btp approve`, `btp snapshots accept` and the hub's visual review |
| 8 | Story selection by git change | Storybook 10.4 | Already present: impact analysis maps changed files to stories (`ImpactAnalysis`, the hub's impact page) |
| 9 | Theme, viewport and locale matrix | Storybook 10.5 | Already present: matrices over themes and viewports, culture and time-zone modes (`WithCulture`, `WithTimeZone`) |
| 10 | Exclusive test lock (a quiet machine for performance stories) | Playwright 1.65 | **Done**: `Exclusive()` stories run alone among the bUnit and browser runs of their process (a readers-writer gate with writer preference); separate test processes are not held |
| 11 | AI-classified visual diffs (noise, insignificant, meaningful) | Argos, Lastest | **Done** without a model: `VisualChangeClassifier` labels each difference noise, shift (with the offset), color, resize or content, with a confidence, in the report, the hub and `btp mcp`. Calibrated on real screenshots: a Firefox update is noise, a one-letter text edit stays content |
| 12 | Failing unawaited assertions | Vitest 5 | **Done**: analyzer BTP006, with a code fix, for dropped assertion, action and harness tasks in synchronous code, where CS4014 is silent |

## Further gaps from the same survey

The survey reported more than the table above listed. These were checked against the code too, and all were missing.

| # | Gap | Seen in | Status |
|---|---|---|---|
| 13 | JUnit `file` attribute for CI annotations | Playwright 1.66 (main) | **Done**: test cases carry `file` (relative to `--source-root`, else the repository) and `line` |
| 14 | Reports with failures only | Playwright `onlyFailures` | **Done**: `btp report --only-failures` for the HTML report and the JUnit XML; summaries keep the whole run's counts |
| 15 | `required` state in ARIA snapshots | Playwright 1.66 (main) | **Done** in both engines, at the same position, checked by the cross-engine parity test |
| 16 | Separate action and navigation timeouts | Playwright 1.65 (.NET) | **Done**: `BTP_ACTION_TIMEOUT_MS` and `BTP_NAVIGATION_TIMEOUT_MS` (and `btp.json` keys) |
| 17 | Seeded shuffle | Vitest 5.1 | **Done**: `BTP_SHUFFLE=<seed>` orders the framework packages' data rows and `catalog.ForRun()` reproducibly |
| 18 | Change-aware story review | Storybook 10.4 | **Done**: the workbench sidebar marks stories affected since a git reference (`WorkbenchOptions.ChangedSince`) |

## Remaining findings

The last findings of the survey, checked against the code after the tables above were done.

| # | Gap | Seen in | Status |
|---|---|---|---|
| 19 | DOM snapshots per step for failed runs | Vitest 5 browser mode, Playwright trace viewer | **Done** for bUnit: the markup after each of the last ten actions, assertions and waits of a failed run, under the steps in the HTML report and the hub. Browser runs already keep a Playwright trace |
| 20 | Interactive review of changed snapshots in the terminal | Vitest 5.1 | **Done**: `btp approve --interactive` (approve, skip, approve the rest, quit), showing the change classification and image paths |
| 21 | Rerun what changed since the last run | Vitest 5.1 `--since-last-run` | **Done**: `BTP_CHANGED_SINCE=last-run`, comparing file contents with the working tree the previous run recorded, plus the stories that failed in it |
| 22 | Deferred retries (retry failures at the end of the run) | Playwright 1.66 (main) | **Not adopted.** The test framework (xUnit, NUnit, MSTest, TUnit) schedules the story cases, so a runner cannot move a retry to the end of the run without holding the test open. BTP retries in a fresh browser context, which removes the shared state deferred retries avoid, and `BTP_LAST_FAILED=1` reruns the failures as a separate run when timing is the suspect |

