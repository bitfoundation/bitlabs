using BlazorTestPlatform.Bunit.Tests.Components;
using BlazorTestPlatform.Configuration;
using BlazorTestPlatform.Results;

namespace BlazorTestPlatform.Bunit.Tests;

public sealed class ActionTimeoutStories : Stories<Counter>
{
    public override string Title => "Tests/Action timeout";

    protected override void Define(StoryCollectionBuilder<Counter> stories) =>
        stories.Add("Missing button", s => s.Tags("fails-on-purpose").Play(ctx => ctx.GetByRole("button", "Missing").ClickAsync()));
}

public sealed class ActionTimeoutTests
{
    private static readonly StoryCatalog Catalog = StoryCatalog.FromCollections([new ActionTimeoutStories()]);

    [Fact]
    public async Task Actions_wait_for_the_action_timeout_and_assertions_keep_theirs()
    {
        var settings = BtpSettings.Load(Path.Combine(Path.GetTempPath(), "btp-bunit-tests"), new Dictionary<string, string?> { ["BTP_ACTION_TIMEOUT_MS"] = "150" });

        var result = await BunitStoryRunner.RunAsync(Catalog.Get("tests-action-timeout--missing-button"), new BunitStoryOptions { Settings = settings, WriteResults = false }, TestContext.Current.CancellationToken);

        Assert.Equal(StoryOutcome.Failed, result.Outcome);
        Assert.Contains("(waited 150 ms)", result.Error, StringComparison.Ordinal);
        Assert.Equal(TimeSpan.FromSeconds(5), settings.DefaultTimeout);
    }

    [Fact]
    public void Timeouts_come_from_the_environment_or_btp_json()
    {
        var directory = Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), "btp-timeouts", Guid.NewGuid().ToString("N"))).FullName;
        File.WriteAllText(Path.Combine(directory, "btp.json"), """{ "actionTimeoutMilliseconds": 2000, "navigationTimeoutMilliseconds": 90000 }""");

        var fromFile = BtpSettings.Load(directory, new Dictionary<string, string?>());
        var fromEnvironment = BtpSettings.Load(directory, new Dictionary<string, string?> { ["BTP_NAVIGATION_TIMEOUT_MS"] = "120000" });

        Assert.Equal(TimeSpan.FromSeconds(2), fromFile.ActionTimeout);
        Assert.Equal(TimeSpan.FromSeconds(90), fromFile.NavigationTimeout);
        Assert.Equal(TimeSpan.FromMinutes(2), fromEnvironment.NavigationTimeout);
        Assert.Null(BtpSettings.Load(Path.GetTempPath(), new Dictionary<string, string?>()).ActionTimeout);
    }
}
