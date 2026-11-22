using BlazorTestPlatform.Bunit.Tests.Components;
using BlazorTestPlatform.Configuration;
using BlazorTestPlatform.Results;

namespace BlazorTestPlatform.Bunit.Tests;

public sealed class ExclusiveRunStories : Stories<Counter>
{
    // Plays of this collection in flight, and the most seen at once by ordinary and by exclusive plays.
    internal static int Active;
    internal static int MaxWhileOrdinary;
    internal static int MaxWhileExclusive;

    public override string Title => "Tests/Exclusive runs";

    private static async Task TrackAsync(bool exclusive)
    {
        Interlocked.Increment(ref Active);
        try
        {
            for (var i = 0; i < 10; i++)
            {
                var seen = Volatile.Read(ref Active);
                if (exclusive)
                {
                    InterlockedMax(ref MaxWhileExclusive, seen);
                }
                else
                {
                    InterlockedMax(ref MaxWhileOrdinary, seen);
                }

                await Task.Delay(20);
            }
        }
        finally
        {
            Interlocked.Decrement(ref Active);
        }
    }

    private static void InterlockedMax(ref int target, int value)
    {
        int current;
        while (value > (current = Volatile.Read(ref target)) && Interlocked.CompareExchange(ref target, value, current) != current)
        {
        }
    }

    protected override void Define(StoryCollectionBuilder<Counter> stories)
    {
        stories.Add("Ordinary A", s => s.Play(_ => TrackAsync(exclusive: false)));
        stories.Add("Ordinary B", s => s.Play(_ => TrackAsync(exclusive: false)));
        stories.Add("Ordinary C", s => s.Play(_ => TrackAsync(exclusive: false)));
        stories.Add("Alone", s => s.Exclusive().Play(_ => TrackAsync(exclusive: true)));

        // A story whose play runs another story (as a test helper might): the inner run is part of the outer one.
        stories.Add("Runs another", s => s.Exclusive().Play(async _ =>
        {
            var inner = await BunitStoryRunner.RunAsync(
                StoryCatalog.FromCollections([new ExclusiveRunStories()]).Get("tests-exclusive-runs--alone"),
                new BunitStoryOptions { WriteResults = false, Settings = BtpSettings.Load(Path.Combine(Path.GetTempPath(), "btp-bunit-tests"), new Dictionary<string, string?>()) });
            if (inner.Outcome != StoryOutcome.Passed)
            {
                throw new StoryAssertionException($"The inner story failed: {inner.Error}");
            }
        }));
    }
}

public sealed class ExclusiveRunTests
{
    private static readonly StoryCatalog Catalog = StoryCatalog.FromCollections([new ExclusiveRunStories()]);

    private static BunitStoryOptions Options => new()
    {
        Settings = BtpSettings.Load(Path.Combine(Path.GetTempPath(), "btp-bunit-tests"), new Dictionary<string, string?>()),
        WriteResults = false,
    };

    [Fact]
    public async Task An_exclusive_story_runs_alone_while_ordinary_ones_overlap()
    {
        var token = TestContext.Current.CancellationToken;
        var a = BunitStoryRunner.RunAsync(Catalog.Get("tests-exclusive-runs--ordinary-a"), Options, token);
        var b = BunitStoryRunner.RunAsync(Catalog.Get("tests-exclusive-runs--ordinary-b"), Options, token);
        await Task.Delay(30, token);
        var alone = BunitStoryRunner.RunAsync(Catalog.Get("tests-exclusive-runs--alone"), Options, token);
        await Task.Delay(30, token);
        var c = BunitStoryRunner.RunAsync(Catalog.Get("tests-exclusive-runs--ordinary-c"), Options, token);

        var results = await Task.WhenAll(a, b, alone, c);

        Assert.All(results, r => Assert.Equal(StoryOutcome.Passed, r.Outcome));
        Assert.True(ExclusiveRunStories.MaxWhileOrdinary >= 2, $"Ordinary stories should overlap; at most {ExclusiveRunStories.MaxWhileOrdinary} ran at once.");
        Assert.Equal(1, ExclusiveRunStories.MaxWhileExclusive);
    }

    [Fact]
    public async Task A_story_run_inside_another_does_not_wait_for_the_gate_it_holds()
    {
        var run = BunitStoryRunner.RunAsync(Catalog.Get("tests-exclusive-runs--runs-another"), Options, TestContext.Current.CancellationToken);

        var finished = await Task.WhenAny(run, Task.Delay(TimeSpan.FromSeconds(20), TestContext.Current.CancellationToken));

        Assert.Same(run, finished);
        Assert.Equal(StoryOutcome.Passed, (await run).Outcome);
    }
}
