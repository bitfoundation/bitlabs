using BlazorTestPlatform.Bunit.Tests.Components;
using BlazorTestPlatform.Configuration;
using BlazorTestPlatform.Results;

namespace BlazorTestPlatform.Bunit.Tests;

public sealed class LocatorHintStories : Stories<Counter>
{
    public override string Title => "Tests/Locator hints";

    protected override void Define(StoryCollectionBuilder<Counter> stories)
    {
        stories.Add("Click a missing button", s => s.Tags("fails-on-purpose").Play(ctx => ctx.GetByRole("button", "Increase").ClickAsync(timeout: TimeSpan.FromMilliseconds(200))));
        stories.Add("Expect a missing button", s => s.Tags("fails-on-purpose").Play(ctx => ctx.Expect(ctx.GetByRole("button", "Increase")).ToBeVisibleAsync(TimeSpan.FromMilliseconds(200))));
        stories.Add("Expect a wrong text", s => s.Tags("fails-on-purpose").Play(ctx => ctx.Expect(ctx.GetByRole("button", "Increment")).ToHaveTextAsync("Add", TimeSpan.FromMilliseconds(200))));
    }
}

public sealed class LocatorHintTests
{
    private static readonly StoryCatalog Catalog = StoryCatalog.FromCollections([new LocatorHintStories()]);

    private static BunitStoryOptions Options => new()
    {
        Settings = BtpSettings.Load(Path.Combine(Path.GetTempPath(), "btp-bunit-tests"), new Dictionary<string, string?>()),
        WriteResults = false,
    };

    [Theory]
    [InlineData("tests-locator-hints--click-a-missing-button")]
    [InlineData("tests-locator-hints--expect-a-missing-button")]
    public async Task Finding_nothing_lists_what_the_story_offers(string id)
    {
        var result = await BunitStoryRunner.RunAsync(Catalog.Get(id), Options, TestContext.Current.CancellationToken);

        Assert.Equal(StoryOutcome.Failed, result.Outcome);
        Assert.Contains("Accessibility tree:", result.Error, StringComparison.Ordinal);
        Assert.Contains("    - button \"Increment\"", result.Error, StringComparison.Ordinal);
        Assert.Contains("  Did you mean: ctx.GetByRole(\"button\", \"Increment\")", result.Error, StringComparison.Ordinal);
    }

    [Fact]
    public async Task A_found_element_with_the_wrong_state_does_not_list_the_tree()
    {
        var result = await BunitStoryRunner.RunAsync(Catalog.Get("tests-locator-hints--expect-a-wrong-text"), Options, TestContext.Current.CancellationToken);

        Assert.Equal(StoryOutcome.Failed, result.Outcome);
        Assert.DoesNotContain("Accessibility tree:", result.Error, StringComparison.Ordinal);
    }
}
