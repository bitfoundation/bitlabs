# bitlabs

The laboratory to develop and test new ideas.

Each folder in this repository is a self-contained project with its own build, tests, license, and README.
Projects here are experimental: APIs, file formats, and features may change without notice.

## Projects

| Project | Stack | Description |
|---|---|---|
| [BlazorAccessibilityToolkit](BlazorAccessibilityToolkit/README.md) | .NET 10, Blazor, bUnit, Playwright, Roslyn | An accessibility engineering toolkit for Razor components and Blazor apps: audits components and pages (axe-core, optionally IBM Equal Access), drives them with a real keyboard, checks focus, names, roles, states, live regions and WAI-ARIA patterns, checks `.razor` files at compile time with 38 analyzer rules and code fixes, and turns the results into CI-ready reports and gates through the `blazor-a11y` CLI, which also serves the toolkit to coding agents over MCP. |
| [BlazorProfiler](BlazorProfiler/README.md) | .NET 10, Blazor | A Blazor-specific performance profiler that collects, correlates, and explains performance problems across interactions, component rendering, HTTP, JS interop, runtime/GC, circuits, and browser-perceived latency. Ships a dashboard, 53 diagnostic rules, an MCP endpoint for coding agents, and a CLI for CI gating. |
| [BlazorTestPlatform](BlazorTestPlatform/README.md) | .NET 10/11, Blazor, bUnit, Playwright | A quality platform for Blazor component libraries and apps: write a component story once and run it as a bUnit test, a real-browser interaction test, a visual snapshot, an accessibility audit, a render-performance check and a cross-hosting regression test, then browse it in a component workbench and review the results in a self-hosted test hub or the `btp` CLI. |
| [DevDeck](DevDeck/README.md) | Rust, Win32 + GDI | A native, ultra-light Windows Git client modeled on the VS Code Source Control view, built to run many instances at once with almost no memory or CPU cost (~0.8 MB executable, ~3 MB private memory per window). |
| [DotNetQuery](DotNetQuery/README.md) | .NET 8/10, Blazor, WPF, WinUI, Avalonia, MAUI, Uno | Server-state management for .NET, the idiomatic counterpart to TanStack Query: fetching, caching, deduplication, invalidation, retries, optimistic mutations, pagination, prerender hydration, persistence, offline queues, real-time sync, and DevTools, with first-class Blazor support and XAML adapters. |

### BlazorAccessibilityToolkit

```
cd BlazorAccessibilityToolkit
dotnet build BlazorAccessibilityToolkit.slnx
dotnet test BlazorAccessibilityToolkit.slnx
dotnet run --project demo/BlazorAccessibilityToolkit.Demo.WebApp
```

Requires the .NET 10 SDK (see [`global.json`](BlazorAccessibilityToolkit/global.json)); the browser suites need Playwright
browsers, and the Blazor Hybrid tests run on Windows only (WebView2). Start with
[getting started](BlazorAccessibilityToolkit/docs/getting-started.md); the guides are in [`BlazorAccessibilityToolkit/docs`](BlazorAccessibilityToolkit/docs).
CI runs on every change under `BlazorAccessibilityToolkit/` through the [BlazorAccessibilityToolkit workflow](.github/workflows/blazoraccessibilitytoolkit.yml).

### BlazorProfiler

```
cd BlazorProfiler
dotnet build
dotnet test tests/BlazorProfiler.Core.Tests
dotnet run --project demo/Server
```

Requires the .NET 10 SDK (see [`global.json`](BlazorProfiler/global.json)). Start with
[getting started](BlazorProfiler/docs/getting-started.md); the full documentation is in [`BlazorProfiler/docs`](BlazorProfiler/docs).
CI runs on every change under `BlazorProfiler/` through the [BlazorProfiler workflow](.github/workflows/blazorprofiler.yml).

### BlazorTestPlatform

```
cd BlazorTestPlatform
dotnet build BlazorTestPlatform.slnx
dotnet test --project tests/BlazorTestPlatform.Bunit.Tests
dotnet test --project demo/BlazorTestPlatform.Demo.UI.Tests
dotnet run --project demo/BlazorTestPlatform.Demo.Workbench
```

Requires the .NET 11 SDK (see [`global.json`](BlazorTestPlatform/global.json)) and the .NET 10 runtime; the browser
suites need Playwright browsers. Start with [getting started](BlazorTestPlatform/docs/getting-started.md); the guides
are in [`BlazorTestPlatform/docs`](BlazorTestPlatform/docs).
CI runs on every change under `BlazorTestPlatform/` through the [BlazorTestPlatform workflow](.github/workflows/blazortestplatform.yml).

### DevDeck

```
cd DevDeck
cargo build --release
target\release\devdeck.exe [path\to\repo]
cargo test
```

Requires Rust and `git` on `PATH`. A prebuilt Windows x64 build with install scripts is in
[`DevDeck/devdeck-win-64`](DevDeck/devdeck-win-64).
CI runs on every change under `DevDeck/` through the [DevDeck workflow](.github/workflows/devdeck.yml).

### DotNetQuery

```
cd DotNetQuery
dotnet build DotNetQuery.slnx -c Release
dotnet test DotNetQuery.slnx -c Release
dotnet run --project demo/TodoApp
```

Requires the .NET 8, 9 and 10 SDKs; the full solution (WPF and WinUI adapters included) builds on Windows. Start with
[getting started](DotNetQuery/docs/getting-started.md); the guides are in [`DotNetQuery/docs`](DotNetQuery/docs).
CI runs on every change under `DotNetQuery/` through the [DotNetQuery workflow](.github/workflows/dotnetquery.yml).

## License

Unless a project states otherwise, the code in this repository is licensed under the [MIT License](LICENSE).
Each project folder includes its own `LICENSE` file.
