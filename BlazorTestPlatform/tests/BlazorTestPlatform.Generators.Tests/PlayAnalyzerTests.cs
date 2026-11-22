using System.Collections.Immutable;
using BlazorTestPlatform.Generators;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.Diagnostics;

namespace BlazorTestPlatform.Generators.Tests;

public sealed class PlayAnalyzerTests
{
    private const string Components = """
        using System.Threading.Tasks;
        using Microsoft.AspNetCore.Components;
        using BlazorTestPlatform;

        public sealed class Button : ComponentBase
        {
            [Parameter] public string? Title { get; set; }
        }

        public sealed class ButtonHarness : ComponentHarness
        {
            protected override By HostQuery => By.Role("button");
            public Task PressAsync() => Host.ClickAsync();
        }
        """;

    internal static async Task<ImmutableArray<Diagnostic>> AnalyzeAsync(string stories)
    {
        var compilation = Compile(stories);

        // The samples must compile: the analyzer is tested on valid C#, as users write it.
        Assert.Empty(compilation.GetDiagnostics().Where(d => d.Severity == DiagnosticSeverity.Error));

        // Concurrent analysis reports in any order: sort by position.
        var diagnostics = await compilation.WithAnalyzers([new PlayAnalyzer()]).GetAnalyzerDiagnosticsAsync();
        return [.. diagnostics.OrderBy(d => d.Location.SourceTree?.FilePath, StringComparer.Ordinal).ThenBy(d => d.Location.SourceSpan.Start)];
    }

    internal static CSharpCompilation Compile(string stories)
    {
        var references = ((string)AppContext.GetData("TRUSTED_PLATFORM_ASSEMBLIES")!)
            .Split(Path.PathSeparator)
            .Select(path => MetadataReference.CreateFromFile(path));
        return CSharpCompilation.Create(
            "Stories",
            [CSharpSyntaxTree.ParseText(Components, path: "Components.cs"), CSharpSyntaxTree.ParseText(stories, path: "Stories.cs")],
            references,
            new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary, nullableContextOptions: NullableContextOptions.Enable));
    }

    internal static string Stories(string play) => $$"""
        using System.Threading.Tasks;
        using BlazorTestPlatform;

        public sealed class ButtonStories : Stories<Button>
        {
            protected override void Define(StoryCollectionBuilder<Button> stories) =>
                stories.Add("Primary", s => s.Play({{play}}));

            private static void Helper(StoryContext ctx) => ctx.Harness<ButtonHarness>().PressAsync();
        }
        """;

    [Fact]
    public async Task Dropped_assertions_and_harness_calls_in_synchronous_code_are_reported()
    {
        var diagnostics = await AnalyzeAsync(Stories("""
            ctx =>
            {
                ctx.Expect(ctx.GetByRole("button")).ToBeVisibleAsync();
                ctx.GetByRole("button").ClickAsync();
                return Task.CompletedTask;
            }
            """));

        Assert.Equal(["ToBeVisibleAsync", "ClickAsync", "PressAsync"], diagnostics.Select(d => d.GetMessage().Split('\'')[1]).ToArray());
        Assert.All(diagnostics, d => Assert.Equal("BTP006", d.Id));
    }

    [Theory]
    [InlineData("async ctx => await ctx.Expect(ctx.GetByRole(\"button\")).ToBeVisibleAsync()")]
    [InlineData("ctx => ctx.Expect(ctx.GetByRole(\"button\")).ToBeVisibleAsync()")]
    [InlineData("ctx => { _ = ctx.GetByRole(\"button\").ClickAsync(); return Task.Delay(1); }")]
    [InlineData("async ctx => { await Task.Yield(); ctx.GetByRole(\"button\").ClickAsync(); }")]
    public async Task Awaited_returned_discarded_or_compiler_covered_calls_are_not_reported(string play)
    {
        var diagnostics = await AnalyzeAsync(Stories(play));

        // Only the helper's dropped harness call; an async lambda's unawaited call is CS4014's to report.
        Assert.Equal(["PressAsync"], diagnostics.Select(d => d.GetMessage().Split('\'')[1]).ToArray());
    }
}
