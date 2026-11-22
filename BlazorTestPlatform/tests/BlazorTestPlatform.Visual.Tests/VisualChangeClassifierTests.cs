using BlazorTestPlatform.Results;

namespace BlazorTestPlatform.Visual.Tests;

public sealed class VisualChangeClassifierTests
{
    private static RgbaImage Page(int width = 160, int height = 100)
    {
        var image = new RgbaImage(width, height);
        for (var y = 0; y < height; y++)
        {
            for (var x = 0; x < width; x++)
            {
                image.SetPixel(x, y, 255, 255, 255);
            }
        }

        return image;
    }

    private static RgbaImage With(RgbaImage image, int x0, int y0, int width, int height, byte r, byte g, byte b)
    {
        for (var y = y0; y < y0 + height; y++)
        {
            for (var x = x0; x < x0 + width; x++)
            {
                image.SetPixel(x, y, r, g, b);
            }
        }

        return image;
    }

    private static VisualChange Classify(RgbaImage expected, RgbaImage actual) =>
        VisualChangeClassifier.Classify(expected, actual, ImageComparer.Compare(expected, actual));

    [Fact]
    public void Scattered_pixels_are_noise()
    {
        var expected = With(Page(), 40, 30, 50, 20, 30, 30, 30);
        var actual = With(With(With(With(Page(), 40, 30, 50, 20, 30, 30, 30), 5, 5, 1, 1, 0, 0, 0), 150, 90, 1, 1, 0, 0, 0), 100, 10, 1, 1, 0, 0, 0);

        var change = Classify(expected, actual);

        Assert.Equal(VisualChangeKind.Noise, change.Kind);
        Assert.Equal("3 scattered pixel(s)", change.Detail);
    }

    [Fact]
    public void A_block_moved_a_few_pixels_is_a_shift()
    {
        var change = Classify(With(Page(), 40, 30, 50, 20, 30, 30, 30), With(Page(), 42, 33, 50, 20, 30, 30, 30));

        Assert.Equal(VisualChangeKind.Shift, change.Kind);
        Assert.Equal("moved 3 px down and 2 px right", change.Detail);
        Assert.True(change.Confidence > 0.8, $"confidence {change.Confidence}");
    }

    [Fact]
    public void The_same_shape_in_another_color_is_a_color_change()
    {
        var change = Classify(With(Page(), 40, 30, 50, 20, 30, 30, 30), With(Page(), 40, 30, 50, 20, 70, 50, 200));

        Assert.Equal(VisualChangeKind.Color, change.Kind);
    }

    [Fact]
    public void A_different_shape_is_a_content_change()
    {
        var change = Classify(With(Page(), 40, 30, 50, 20, 30, 30, 30), With(With(Page(), 40, 30, 15, 20, 30, 30, 30), 60, 60, 60, 25, 30, 30, 30));

        Assert.Equal(VisualChangeKind.Content, change.Kind);
    }

    [Fact]
    public void Another_size_is_a_resize()
    {
        var change = Classify(Page(160, 100), Page(160, 120));

        Assert.Equal(VisualChangeKind.Resize, change.Kind);
        Assert.Equal("160x100 to 160x120", change.Detail);
    }

    private static VisualChange ClassifyFixture(string name)
    {
        var directory = Path.Combine(AppContext.BaseDirectory, "Fixtures");
        return Classify(RgbaImage.FromPng(File.ReadAllBytes(Path.Combine(directory, name + ".before.png"))), RgbaImage.FromPng(File.ReadAllBytes(Path.Combine(directory, name + ".after.png"))));
    }

    [Fact]
    public void A_browser_update_that_redraws_glyph_edges_is_noise()
    {
        // The demo's Todo list baseline before and after Firefox 155 to 157 (Playwright 1.63 to 1.64): 15 changed pixels.
        var change = ClassifyFixture("firefox-update");

        Assert.Equal(VisualChangeKind.Noise, change.Kind);
        Assert.Equal("15 pixel(s) beside 57 anti-aliasing differences", change.Detail);
        Assert.True(change.Confidence <= 0.8);
    }

    [Fact]
    public void One_changed_letter_is_content_not_noise()
    {
        // "Save changes" and "Save chanqes" at 14 px in Chromium: almost as few pixels as the browser update, but a real change.
        Assert.Equal(VisualChangeKind.Content, ClassifyFixture("one-letter").Kind);
    }

    [Fact]
    public void Mismatched_snapshots_carry_the_kind_of_change()
    {
        var root = Path.Combine(Path.GetTempPath(), "btp-classify", Guid.NewGuid().ToString("N"));
        var writer = new SnapshotStore(Path.Combine(root, "baselines"), Path.Combine(root, "out"), Configuration.SnapshotUpdateMode.Missing);
        Assert.Equal(VisualStatus.NewBaseline, writer.Compare("card/desktop", With(Page(), 40, 30, 50, 20, 30, 30, 30).ToPng()).Status);
        var checker = new SnapshotStore(Path.Combine(root, "baselines"), Path.Combine(root, "out"), Configuration.SnapshotUpdateMode.None);

        var moved = checker.Compare("card/desktop", With(Page(), 40, 34, 50, 20, 30, 30, 30).ToPng());

        Assert.Equal(VisualStatus.Mismatched, moved.Status);
        Assert.Equal(VisualChangeKind.Shift, moved.Change);
        Assert.Equal("moved 4 px down", moved.ChangeDetail);
        Assert.StartsWith("shift, moved 4 px down (", moved.ChangeSummary, StringComparison.Ordinal);
    }
}
