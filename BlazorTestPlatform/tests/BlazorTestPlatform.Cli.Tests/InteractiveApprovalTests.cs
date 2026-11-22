using Assert = Xunit.Assert;
using TestContext = Xunit.TestContext;
using BlazorTestPlatform.Cli;
using BlazorTestPlatform.Results;

namespace BlazorTestPlatform.Cli.Tests;

public sealed class InteractiveApprovalTests
{
    // A 1x1 PNG.
    private static readonly byte[] Png = Convert.FromBase64String("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==");

    private static readonly VisualResult Shifted = new()
    {
        SnapshotName = "inputs-button--primary/desktop.light.chromium-linux",
        Status = VisualStatus.Mismatched,
        DiffPixels = 1234,
        DiffRatio = 0.0123,
        Change = VisualChangeKind.Shift,
        ChangeDetail = "moved 3 px down",
        ChangeConfidence = 0.92,
        ActualPath = "visual/inputs-button--primary/desktop.light.chromium-linux.actual.png",
    };

    [Theory]
    [InlineData("y\n", "Approve")]
    [InlineData("No\n", "Skip")]
    [InlineData("a\n", "ApproveRest")]
    [InlineData("q\n", "Quit")]
    [InlineData("", "EndOfInput")]
    [InlineData("maybe\n\ny\n", "Approve")]
    public void Answers_choose_what_happens_to_the_snapshot(string answers, string expected)
    {
        var output = new StringWriter();

        var choice = InteractiveApproval.Ask(new StringReader(answers), output, Shifted, "inputs-button--primary", Path.GetTempPath(), 2, 5);

        Assert.Equal(Enum.Parse<ApprovalChoice>(expected), choice);
    }

    [Fact]
    public void The_prompt_shows_what_changed_and_where_the_images_are()
    {
        var output = new StringWriter();
        var results = Path.Combine(Path.GetTempPath(), "btp-approve-prompt");

        InteractiveApproval.Ask(new StringReader("maybe\nn\n"), output, Shifted, "inputs-button--primary", results, 2, 5);

        var text = output.ToString();
        Assert.Contains("[2/5] inputs-button--primary/desktop.light.chromium-linux  (inputs-button--primary)", text, StringComparison.Ordinal);
        Assert.Contains("Mismatched: 1,234 px differ (1.23 %)", text.Replace("1.23%", "1.23 %", StringComparison.Ordinal), StringComparison.Ordinal);
        Assert.Contains("looks like: shift, moved 3 px down (92", text, StringComparison.Ordinal);
        Assert.Contains("actual   " + Path.GetFullPath(Path.Combine(results, Shifted.ActualPath!)), text, StringComparison.Ordinal);
        Assert.Contains("Answer y, n, a or q.", text, StringComparison.Ordinal);
    }

    [Fact]
    public async Task Btp_approve_interactive_approves_only_what_the_reviewer_accepts()
    {
        var project = Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), "btp-approve", Guid.NewGuid().ToString("N"))).FullName;
        var results = Path.Combine(project, "TestResults", "btp");
        var store = new ResultsStore(results);
        foreach (var story in new[] { "a--first", "b--second", "c--third" })
        {
            var actual = Path.Combine(results, "visual", story, "desktop.actual.png");
            Directory.CreateDirectory(Path.GetDirectoryName(actual)!);
            File.WriteAllBytes(actual, Png);
            store.Write(new StoryResult
            {
                Key = story + "|chromium",
                StoryId = story,
                Title = "T",
                Name = story,
                Driver = "chromium",
                Outcome = StoryOutcome.Failed,
                Visual = new VisualResult { SnapshotName = story + "/desktop", Status = VisualStatus.MissingBaseline, ActualPath = actual },
            });
        }

        // Skip the first, approve the second, quit before the third.
        var (exitCode, output) = await RunApproveAsync(project, "n\ny\nq\n");

        Assert.Equal(0, exitCode);
        Assert.False(File.Exists(Path.Combine(project, "__snapshots__", "a--first", "desktop.png")));
        Assert.True(File.Exists(Path.Combine(project, "__snapshots__", "b--second", "desktop.png")));
        Assert.False(File.Exists(Path.Combine(project, "__snapshots__", "c--third", "desktop.png")));
        Assert.Contains("1 baseline(s) updated, 2 left as they were.", output, StringComparison.Ordinal);
    }

    [Fact]
    public async Task Btp_approve_interactive_fails_when_nobody_answers()
    {
        var project = Directory.CreateDirectory(Path.Combine(Path.GetTempPath(), "btp-approve", Guid.NewGuid().ToString("N"))).FullName;
        var results = Path.Combine(project, "TestResults", "btp");
        var actual = Path.Combine(results, "visual", "a--first", "desktop.actual.png");
        Directory.CreateDirectory(Path.GetDirectoryName(actual)!);
        File.WriteAllBytes(actual, Png);
        new ResultsStore(results).Write(new StoryResult
        {
            Key = "a--first|chromium",
            StoryId = "a--first",
            Title = "T",
            Name = "First",
            Driver = "chromium",
            Outcome = StoryOutcome.Failed,
            Visual = new VisualResult { SnapshotName = "a--first/desktop", Status = VisualStatus.MissingBaseline, ActualPath = actual },
        });

        var (exitCode, _) = await RunApproveAsync(project, string.Empty);

        Assert.Equal(1, exitCode);
        Assert.False(File.Exists(Path.Combine(project, "__snapshots__", "a--first", "desktop.png")));
    }

    private static async Task<(int ExitCode, string Output)> RunApproveAsync(string project, string answers)
    {
        var start = new System.Diagnostics.ProcessStartInfo("dotnet")
        {
            WorkingDirectory = project,
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
        };
        foreach (var argument in new[] { McpTests.CliPath(), "approve", "--interactive" })
        {
            start.ArgumentList.Add(argument);
        }

        using var process = System.Diagnostics.Process.Start(start)!;
        await process.StandardInput.WriteAsync(answers);
        process.StandardInput.Close();
        var errors = process.StandardError.ReadToEndAsync(TestContext.Current.CancellationToken);
        var output = await process.StandardOutput.ReadToEndAsync(TestContext.Current.CancellationToken);
        await process.WaitForExitAsync(TestContext.Current.CancellationToken);
        _ = await errors;
        return (process.ExitCode, output);
    }
}
