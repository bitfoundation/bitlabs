namespace BlazorTestPlatform.Demo.UI.Tests;

/// <summary>
/// Test impact analysis: a pull request that changes <c>Button.razor</c> needs the button's stories and the stories of
/// every component that renders a Button, not the whole catalog. In CI, <c>BTP_CHANGED_SINCE=origin/main</c> feeds
/// <c>StoryImpact.FromEnvironment</c> the files the branch changed.
/// </summary>
public sealed class ImpactAnalysisTests
{
    [Fact]
    public void A_component_change_selects_its_stories_and_the_stories_of_components_that_render_it()
    {
        var impact = StoryImpact.Analyze(ComponentStoryTests.Catalog, ["demo/BlazorTestPlatform.Demo.UI/Button.razor"]);

        Assert.False(impact.RunsEverything);
        var components = impact.Stories.Select(s => s.Title).Distinct().ToArray();
        Assert.Contains("Inputs/Button", components);
        Assert.Contains("Patterns/Sign-up form", components);    // renders TextField, Button and Alert
        Assert.Contains("Feedback/Cookie consent", components);
        Assert.Contains("Data/Todo list", components);
        Assert.DoesNotContain("Navigation/Tabs", components);
        Assert.DoesNotContain("Display/Badge", components);
        Assert.InRange(impact.Stories.Count, 1, ComponentStoryTests.Catalog.Count / 2);
    }

    [Fact]
    public void Documentation_selects_nothing_and_unplaceable_code_selects_everything()
    {
        Assert.Empty(StoryImpact.Analyze(ComponentStoryTests.Catalog, ["README.md", "docs/stories.md"]).Stories);

        var shared = StoryImpact.Analyze(ComponentStoryTests.Catalog, ["src/BlazorTestPlatform.Core/Stories/StoryBuilder.cs"]);
        Assert.True(shared.RunsEverything);
        Assert.Equal(ComponentStoryTests.Catalog.Count, shared.Stories.Count);
    }
}

/// <summary>
/// The workbench's change markers (<c>WorkbenchOptions.ChangedSince</c>): stories affected by a repository's uncommitted
/// changes, computed by impact analysis over the files git reports.
/// </summary>
public sealed class ChangedStoriesTests
{
    private static void Git(string directory, params string[] arguments)
    {
        var start = new System.Diagnostics.ProcessStartInfo("git") { WorkingDirectory = directory, RedirectStandardOutput = true, RedirectStandardError = true };
        foreach (var argument in arguments)
        {
            start.ArgumentList.Add(argument);
        }

        using var process = System.Diagnostics.Process.Start(start)!;
        process.WaitForExit();
        Assert.True(process.ExitCode == 0, $"git {string.Join(' ', arguments)}: {process.StandardError.ReadToEnd()}");
    }

    [Fact]
    public void Uncommitted_changes_mark_the_stories_they_affect()
    {
        var repository = Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), "btp-changed", Guid.NewGuid().ToString("N"))).FullName;
        var button = Path.Combine(repository, "demo", "BlazorTestPlatform.Demo.UI", "Button.razor");
        Directory.CreateDirectory(Path.GetDirectoryName(button)!);
        File.WriteAllText(button, "<button>@ChildContent</button>");
        File.WriteAllText(Path.Combine(repository, "README.md"), "demo");
        Git(repository, "init", "-q");
        Git(repository, "-c", "user.email=test@example.com", "-c", "user.name=test", "add", ".");
        Git(repository, "-c", "user.email=test@example.com", "-c", "user.name=test", "commit", "-q", "-m", "start");
        var changes = new BlazorTestPlatform.Workbench.ChangedStories();

        var clean = changes.For(ComponentStoryTests.Catalog, "HEAD", repository);
        File.AppendAllText(button, "\n<!-- changed -->");
        var edited = new BlazorTestPlatform.Workbench.ChangedStories().For(ComponentStoryTests.Catalog, "HEAD", repository);

        Assert.True(clean.Available);
        Assert.Empty(clean.Reasons);
        Assert.False(edited.AnyStory);
        Assert.Contains("inputs-button--primary", edited.Reasons.Keys);
        Assert.Contains(edited.Reasons.Keys, id => id.StartsWith("patterns-sign-up-form--", StringComparison.Ordinal));
        Assert.DoesNotContain(edited.Reasons.Keys, id => id.StartsWith("navigation-tabs--", StringComparison.Ordinal));
        Assert.Equal(["demo/BlazorTestPlatform.Demo.UI/Button.razor"], edited.Reasons["inputs-button--primary"]);
        Assert.False(new BlazorTestPlatform.Workbench.ChangedStories().For(ComponentStoryTests.Catalog, "refs/heads/nope", repository).Available);

        // A reference git would read as an option gives no markers, never a broken page.
        Assert.False(new BlazorTestPlatform.Workbench.ChangedStories().For(ComponentStoryTests.Catalog, "--output=x", repository).Available);
    }
}
