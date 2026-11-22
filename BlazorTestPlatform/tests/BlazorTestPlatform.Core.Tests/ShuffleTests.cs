using BlazorTestPlatform.Configuration;

namespace BlazorTestPlatform.Core.Tests;

public sealed class ShuffleTests
{
    private static BtpSettings Settings(string? seed) =>
        BtpSettings.Load(Path.GetTempPath(), seed is null ? new Dictionary<string, string?>() : new Dictionary<string, string?> { ["BTP_SHUFFLE"] = seed });

    private static readonly string[] Items = [.. Enumerable.Range(1, 20).Select(i => $"story-{i}")];

    [Fact]
    public void A_seed_gives_the_same_shuffled_order_every_time()
    {
        var first = Settings("42").InRunOrder(Items).ToArray();

        Assert.Equal(first, Settings("42").InRunOrder(Items).ToArray());
        Assert.NotEqual(Items, first);
        Assert.Equal(Items.Order(StringComparer.Ordinal), first.Order(StringComparer.Ordinal));
        Assert.NotEqual(first, Settings("7").InRunOrder(Items).ToArray());
    }

    [Fact]
    public void Without_a_seed_the_order_is_kept()
    {
        Assert.Equal(Items, Settings(null).InRunOrder(Items).ToArray());
    }

    [Fact]
    public void A_seed_that_is_not_a_number_is_reported()
    {
        var error = Assert.Throws<FormatException>(() => Settings("yes"));

        Assert.Contains("BTP_SHUFFLE", error.Message, StringComparison.Ordinal);
    }
}
