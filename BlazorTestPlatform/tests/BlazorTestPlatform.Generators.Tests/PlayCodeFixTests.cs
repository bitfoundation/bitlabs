using System.Collections.Immutable;
using BlazorTestPlatform.CodeFixes;
using BlazorTestPlatform.Generators;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CodeActions;
using Microsoft.CodeAnalysis.CodeFixes;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.Diagnostics;

namespace BlazorTestPlatform.Generators.Tests;

public sealed class PlayCodeFixTests
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

    // Applies the fix offered for the diagnostic at the given index; returns the fixed source and the remaining diagnostics
    // (analyzer and compiler warnings, so a fix that trades BTP006 for CS4014 would show).
    private static async Task<(string Source, ImmutableArray<Diagnostic> Remaining)> FixAsync(string stories, int index)
    {
        using var workspace = new AdhocWorkspace();
        var references = ((string)AppContext.GetData("TRUSTED_PLATFORM_ASSEMBLIES")!)
            .Split(Path.PathSeparator)
            .Select(path => MetadataReference.CreateFromFile(path));
        var project = workspace.AddProject("Stories", LanguageNames.CSharp)
            .WithCompilationOptions(new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary, nullableContextOptions: NullableContextOptions.Enable))
            .WithMetadataReferences(references);
        var document = project.AddDocument("Components.cs", Components).Project.AddDocument("Stories.cs", stories);

        var diagnostic = (await DiagnosticsAsync(document.Project)).Where(d => d.Id == "BTP006").OrderBy(d => d.Location.SourceSpan.Start).ElementAt(index);
        var actions = new List<CodeAction>();
        await new PlayCodeFixProvider().RegisterCodeFixesAsync(new CodeFixContext(document, diagnostic, (action, _) => actions.Add(action), CancellationToken.None));

        var operation = (await Assert.Single(actions).GetOperationsAsync(CancellationToken.None)).OfType<ApplyChangesOperation>().Single();
        var fixedDocument = operation.ChangedSolution.GetDocument(document.Id)!;
        return ((await fixedDocument.GetTextAsync()).ToString(), await DiagnosticsAsync(fixedDocument.Project));
    }

    private static async Task<ImmutableArray<Diagnostic>> DiagnosticsAsync(Project project)
    {
        var compilation = (await project.GetCompilationAsync())!;
        var errors = compilation.GetDiagnostics().Where(d => d.Severity == DiagnosticSeverity.Error).ToArray();
        Assert.True(errors.Length == 0, string.Join(Environment.NewLine, errors.Select(e => e.ToString())));
        var warnings = compilation.GetDiagnostics().Where(d => d.Id == "CS4014");
        return [.. warnings, .. await compilation.WithAnalyzers([new PlayAnalyzer()]).GetAnalyzerDiagnosticsAsync()];
    }

    private const string Source = """
        using System.Threading.Tasks;
        using BlazorTestPlatform;

        public sealed class ButtonStories : Stories<Button>
        {
            protected override void Define(StoryCollectionBuilder<Button> stories) =>
                stories.Add("Primary", s => s.Play(ctx =>
                {
                    ctx.Expect(ctx.GetByRole("button")).ToBeVisibleAsync();
                    ctx.GetByRole("button").ClickAsync();
                    return Task.CompletedTask;
                }));

            private static void Press(StoryContext ctx) => ctx.Harness<ButtonHarness>().PressAsync();
        }
        """;

    [Fact]
    public async Task A_lambda_becomes_async_and_awaits_every_dropped_call()
    {
        var (source, remaining) = await FixAsync(Source, 0);

        Assert.True(source.Contains("s.Play(async ctx =>", StringComparison.Ordinal), source);
        Assert.Contains("await ctx.Expect(ctx.GetByRole(\"button\")).ToBeVisibleAsync();", source, StringComparison.Ordinal);
        Assert.Contains("await ctx.GetByRole(\"button\").ClickAsync();", source, StringComparison.Ordinal);
        Assert.DoesNotContain("Task.CompletedTask", source, StringComparison.Ordinal);

        // Only the helper's call is left, and nothing turned into CS4014.
        Assert.Equal(["BTP006"], remaining.Select(d => d.Id).ToArray());
    }

    [Fact]
    public async Task A_void_helper_returns_a_task()
    {
        var (source, remaining) = await FixAsync(Source, 2);

        Assert.Contains("private static async Task Press(StoryContext ctx) => await ctx.Harness<ButtonHarness>().PressAsync();", source, StringComparison.Ordinal);
        Assert.Equal(2, remaining.Count(d => d.Id == "BTP006"));
    }

    [Fact]
    public async Task No_fix_when_the_function_returns_other_values()
    {
        using var workspace = new AdhocWorkspace();
        var references = ((string)AppContext.GetData("TRUSTED_PLATFORM_ASSEMBLIES")!).Split(Path.PathSeparator).Select(path => MetadataReference.CreateFromFile(path));
        var project = workspace.AddProject("Stories", LanguageNames.CSharp)
            .WithCompilationOptions(new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary))
            .WithMetadataReferences(references);
        var document = project.AddDocument("Components.cs", Components).Project.AddDocument("Stories.cs", """
            using System.Threading.Tasks;
            using BlazorTestPlatform;

            public static class Helpers
            {
                public static Task Step(StoryContext ctx)
                {
                    ctx.GetByRole("button").ClickAsync();
                    return Task.Delay(10);
                }
            }
            """);
        var diagnostic = (await DiagnosticsAsync(document.Project)).Single(d => d.Id == "BTP006");
        var actions = new List<CodeAction>();

        await new PlayCodeFixProvider().RegisterCodeFixesAsync(new CodeFixContext(document, diagnostic, (action, _) => actions.Add(action), CancellationToken.None));

        Assert.Empty(actions);
    }

    private static async Task<IReadOnlyList<CodeAction>> OffersAsync(string source)
    {
        using var workspace = new AdhocWorkspace();
        var references = ((string)AppContext.GetData("TRUSTED_PLATFORM_ASSEMBLIES")!).Split(Path.PathSeparator).Select(path => MetadataReference.CreateFromFile(path));
        var project = workspace.AddProject("Stories", LanguageNames.CSharp)
            .WithCompilationOptions(new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary))
            .WithMetadataReferences(references);
        var document = project.AddDocument("Components.cs", Components).Project.AddDocument("Stories.cs", source);
        var diagnostic = (await DiagnosticsAsync(document.Project)).Single(d => d.Id == "BTP006");
        var actions = new List<CodeAction>();
        await new PlayCodeFixProvider().RegisterCodeFixesAsync(new CodeFixContext(document, diagnostic, (action, _) => actions.Add(action), CancellationToken.None));
        return actions;
    }

    [Theory]
    [InlineData("""
        public interface IStep { void Run(StoryContext ctx); }
        public sealed class Step : IStep { public void Run(StoryContext ctx) => ctx.GetByRole("button").ClickAsync(); }
        """)]
    [InlineData("""
        public abstract class Base { public abstract void Run(StoryContext ctx); }
        public sealed class Step : Base { public override void Run(StoryContext ctx) => ctx.GetByRole("button").ClickAsync(); }
        """)]
    [InlineData("""
        public static class Steps
        {
            public static void Run(StoryContext ctx, out int count) { count = 1; ctx.GetByRole("button").ClickAsync(); }
        }
        """)]
    [InlineData("""
        public static class Steps
        {
            public static System.Collections.Generic.IEnumerable<int> Run(StoryContext ctx) { ctx.GetByRole("button").ClickAsync(); yield return 1; }
        }
        """)]
    [InlineData("""
        public static class Steps
        {
            public static void Handler(StoryContext ctx) => ctx.GetByRole("button").ClickAsync();
            public static System.Action<StoryContext> AsDelegate => Handler;
        }
        """)]
    [InlineData("""
        public static class Steps
        {
            public static void Run(StoryContext ctx) { System.Action<StoryContext> act = c => c.GetByRole("button").ClickAsync(); act(ctx); }
        }
        """)]
    public async Task No_fix_where_async_would_break_the_code_or_hide_failures(string types)
    {
        Assert.Empty(await OffersAsync("using System.Threading.Tasks;\nusing BlazorTestPlatform;\n" + types));
    }

    [Fact]
    public async Task Fire_and_forget_calls_that_are_not_play_calls_stay_unawaited()
    {
        var (source, _) = await FixAsync("""
            using System.Threading.Tasks;
            using BlazorTestPlatform;

            public static class Steps
            {
                public static void Run(StoryContext ctx)
                {
                    Task.Run(() => { });
                    ctx.GetByRole("button").ClickAsync();
                }
            }
            """, 0);

        Assert.Contains("public static async Task Run(StoryContext ctx)", source, StringComparison.Ordinal);
        Assert.Contains("    Task.Run(() => { });", source, StringComparison.Ordinal);
        Assert.Contains("await ctx.GetByRole(\"button\").ClickAsync();", source, StringComparison.Ordinal);
    }

    [Fact]
    public async Task A_local_function_without_modifiers_gets_async_in_front_of_its_return_type()
    {
        var (source, _) = await FixAsync("""
            using System.Threading.Tasks;
            using BlazorTestPlatform;

            public static class Steps
            {
                public static Task Run(StoryContext ctx)
                {
                    Task Press() { ctx.GetByRole("button").ClickAsync(); return Task.CompletedTask; }
                    return Press();
                }
            }
            """, 0);

        Assert.Contains("        async Task Press() { await ctx.GetByRole(\"button\").ClickAsync(); return; }", source, StringComparison.Ordinal);
    }
}
