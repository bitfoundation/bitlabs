using BlazorTestPlatform.Bunit.Tests.Components;
using BlazorTestPlatform.Configuration;
using BlazorTestPlatform.Results;

namespace BlazorTestPlatform.Bunit.Tests;

public sealed class StepMarkupStories : Stories<Counter>
{
    public override string Title => "Tests/Step markup";

    protected override void Define(StoryCollectionBuilder<Counter> stories)
    {
        stories.Add("Wrong count", s => s.Tags("fails-on-purpose").Play(async ctx =>
        {
            await ctx.GetByRole("button", "Increment").ClickAsync();
            await ctx.GetByRole("button", "Increment").ClickAsync();
            await ctx.Expect(ctx.GetByRole("status")).ToHaveTextAsync("Current count: 3");
        }));
        stories.Add("Right count", s => s.Play(async ctx =>
        {
            await ctx.GetByRole("button", "Increment").ClickAsync();
            await ctx.Expect(ctx.GetByRole("status")).ToHaveTextAsync("Current count: 1");
        }));
    }
}

public sealed class StepMarkupTests
{
    private static readonly StoryCatalog Catalog = StoryCatalog.FromCollections([new StepMarkupStories()]);
    private static readonly BtpSettings Settings = BtpSettings.Load(Path.Combine(Path.GetTempPath(), "btp-bunit-tests"), new Dictionary<string, string?> { ["BTP_TIMEOUT_MS"] = "200" });

    [Fact]
    public async Task Failed_runs_keep_the_DOM_after_each_step()
    {
        var result = await BunitStoryRunner.RunAsync(Catalog.Get("tests-step-markup--wrong-count"), new BunitStoryOptions { Settings = Settings, WriteResults = false }, TestContext.Current.CancellationToken);

        Assert.Equal(StoryOutcome.Failed, result.Outcome);
        var steps = Flatten(result.Steps).Where(s => s.Markup is not null).ToArray();
        Assert.Collection(
            steps,
            first => Assert.Contains("Current count: 1", first.Markup, StringComparison.Ordinal),
            second => Assert.Contains("Current count: 2", second.Markup, StringComparison.Ordinal),
            failed =>
            {
                // The failing assertion shows the DOM it gave up on.
                Assert.True(failed.Failed);
                Assert.Contains("Current count: 2", failed.Markup, StringComparison.Ordinal);
            });
        Assert.DoesNotContain(Flatten(result.Steps), s => s.Kind is StepKind.Render or StepKind.Step && s.Markup is not null);
    }

    [Fact]
    public async Task Passed_runs_and_opted_out_runs_keep_none()
    {
        var passed = await BunitStoryRunner.RunAsync(Catalog.Get("tests-step-markup--right-count"), new BunitStoryOptions { Settings = Settings, WriteResults = false }, TestContext.Current.CancellationToken);
        var optedOut = await BunitStoryRunner.RunAsync(Catalog.Get("tests-step-markup--wrong-count"), new BunitStoryOptions { Settings = Settings, WriteResults = false, RecordStepMarkup = false }, TestContext.Current.CancellationToken);

        Assert.Equal(StoryOutcome.Passed, passed.Outcome);
        Assert.All(Flatten(passed.Steps), s => Assert.Null(s.Markup));
        Assert.Equal(StoryOutcome.Failed, optedOut.Outcome);
        Assert.All(Flatten(optedOut.Steps), s => Assert.Null(s.Markup));
    }

    [Fact]
    public void Only_the_last_markups_are_kept_and_long_ones_are_cut()
    {
        var steps = Enumerable.Range(0, 5).Select(i => new StepRecord { Title = $"step {i}", StartMilliseconds = i, Markup = new string('x', 10 + i) }).ToList();

        StepRecorder.LimitMarkup(steps, keep: 2, maxLength: 12);

        Assert.Equal([null, null, null], steps.Take(3).Select(s => s.Markup));
        Assert.StartsWith("xxxxxxxxxxxx\n<!-- cut: 1 more characters -->", steps[3].Markup, StringComparison.Ordinal);
        Assert.StartsWith("xxxxxxxxxxxx\n<!-- cut: 2 more characters -->", steps[4].Markup, StringComparison.Ordinal);
    }

    private static IEnumerable<StepRecord> Flatten(IEnumerable<StepRecord> steps) => steps.SelectMany(s => Flatten(s.Children).Prepend(s));
}
