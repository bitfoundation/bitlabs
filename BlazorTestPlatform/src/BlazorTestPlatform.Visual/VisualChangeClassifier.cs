using System.Globalization;
using BlazorTestPlatform.Results;

namespace BlazorTestPlatform.Visual;

/// <summary>What a visual change looks like, with how sure the classifier is.</summary>
/// <param name="Kind">The kind of change.</param>
/// <param name="Confidence">How well the change fits the kind, in [0, 1].</param>
/// <param name="Detail">A short explanation (<c>moved 3 px down</c>), or null.</param>
public sealed record VisualChange(VisualChangeKind Kind, double Confidence, string? Detail);

/// <summary>
/// Classifies a screenshot difference without a model, as visual review services do with one: rendering noise (scattered
/// pixels, no solid area), a shift (the changed region is the baseline moved a few pixels), a color change (the same edges
/// in different colors), a resize, or a change of content. Reviewers start with content changes and can wave noise
/// through; the kind is a hint for triage, never a reason to pass a failing comparison.
/// </summary>
public static class VisualChangeClassifier
{
    private const int MaxShift = 8;

    // Samples per candidate shift (289 candidates) and per neighbourhood scan: full-page diffs stay well under a second.
    private const int MaxShiftSamples = 20_000;
    private const int MaxClusterSamples = 400_000;

    /// <summary>Classifies the difference <paramref name="comparison"/> found between the two images.</summary>
    public static VisualChange Classify(RgbaImage expected, RgbaImage actual, ImageCompareResult comparison)
    {
        ArgumentNullException.ThrowIfNull(expected);
        ArgumentNullException.ThrowIfNull(actual);
        ArgumentNullException.ThrowIfNull(comparison);

        if (comparison.SizeMismatch)
        {
            return new VisualChange(VisualChangeKind.Resize, 1, $"{expected.Width}x{expected.Height} to {actual.Width}x{actual.Height}");
        }

        if (comparison.DiffBounds is not { } bounds || comparison.DiffImage is not { } diff || comparison.DiffPixels == 0)
        {
            return new VisualChange(VisualChangeKind.Noise, 1, "no pixel beyond the threshold");
        }

        // Noise: few changed pixels, and almost none with changed neighbours (no solid area changed).
        var clustered = ClusteredShare(diff, bounds);
        if (comparison.DiffPixels <= 4 || (clustered < 0.2 && comparison.DiffRatio < 0.001))
        {
            return new VisualChange(VisualChangeKind.Noise, Math.Round(1 - clustered, 2), $"{comparison.DiffPixels} scattered pixel(s)");
        }

        // Rendering noise around glyph edges (a browser or font update): anti-aliasing differences far outnumber the pixels
        // that changed beyond it, and the change is tiny relative to the image. Measured: a Firefox update gave about 3.8
        // anti-aliased pixels per changed one; editing one letter of 14 px text gave 1.9 at most. The rule is the
        // classifier's closest call, so its confidence is capped.
        if (comparison.AntiAliasedPixels >= 3 * comparison.DiffPixels && comparison.DiffRatio < 0.0005)
        {
            var share = (double)comparison.AntiAliasedPixels / (comparison.AntiAliasedPixels + comparison.DiffPixels);
            return new VisualChange(VisualChangeKind.Noise, Math.Round(Math.Min(share, 0.8), 2), $"{comparison.DiffPixels} pixel(s) beside {comparison.AntiAliasedPixels} anti-aliasing differences");
        }

        // Shift: the changed region matches the baseline moved by a few pixels.
        if (BestShift(expected, actual, bounds) is { Residual: < 0.02 } shift && (shift.Dx != 0 || shift.Dy != 0))
        {
            return new VisualChange(VisualChangeKind.Shift, Math.Round(1 - (shift.Residual * 10), 2), Describe(shift.Dx, shift.Dy));
        }

        // Color: the same edges in the region, in other colors.
        var edges = EdgeAgreement(expected, actual, bounds);
        if (edges >= 0.9)
        {
            return new VisualChange(VisualChangeKind.Color, Math.Round(edges, 2), "same shapes, different colors");
        }

        return new VisualChange(VisualChangeKind.Content, Math.Round(Math.Clamp(1 - Math.Max(edges, clustered < 0.2 ? 0.5 : 0), 0.5, 1), 2), null);
    }

    // Share of changed pixels with at least two changed neighbours (the diff image marks them red, or magenta outside the overlap).
    private static double ClusteredShare(RgbaImage diff, PixelRectangle bounds)
    {
        int changed = 0, clustered = 0;
        var step = Math.Max(1, (int)Math.Ceiling(Math.Sqrt((double)bounds.Width * bounds.Height / MaxClusterSamples)));
        for (var y = bounds.Y; y < bounds.Y + bounds.Height; y += step)
        {
            for (var x = bounds.X; x < bounds.X + bounds.Width; x += step)
            {
                if (!Changed(diff, x, y))
                {
                    continue;
                }

                changed++;
                var neighbours = 0;
                for (var dy = -1; dy <= 1; dy++)
                {
                    for (var dx = -1; dx <= 1; dx++)
                    {
                        if ((dx != 0 || dy != 0) && Changed(diff, x + dx, y + dy))
                        {
                            neighbours++;
                        }
                    }
                }

                if (neighbours >= 2)
                {
                    clustered++;
                }
            }
        }

        return changed == 0 ? 0 : (double)clustered / changed;
    }

    private static bool Changed(RgbaImage diff, int x, int y)
    {
        if (x < 0 || y < 0 || x >= diff.Width || y >= diff.Height)
        {
            return false;
        }

        var (r, g, _, _) = diff.GetPixel(x, y);
        return r == 255 && g == 0;
    }

    // The translation of the baseline that best explains the actual image over the changed region (grown by the search
    // radius), as the share of luminance samples that still differ. Large regions are skipped: a shift of a whole page is
    // rare and expensive to search.
    private static (int Dx, int Dy, double Residual)? BestShift(RgbaImage expected, RgbaImage actual, PixelRectangle bounds)
    {
        var x0 = Math.Max(0, bounds.X - MaxShift);
        var y0 = Math.Max(0, bounds.Y - MaxShift);
        var x1 = Math.Min(actual.Width, bounds.X + bounds.Width + MaxShift);
        var y1 = Math.Min(actual.Height, bounds.Y + bounds.Height + MaxShift);
        var step = Math.Max(1, (int)Math.Ceiling(Math.Sqrt((double)(x1 - x0) * (y1 - y0) / MaxShiftSamples)));

        (int, int, double)? best = null;
        for (var dy = -MaxShift; dy <= MaxShift; dy++)
        {
            for (var dx = -MaxShift; dx <= MaxShift; dx++)
            {
                int samples = 0, different = 0;
                for (var y = y0; y < y1; y += step)
                {
                    var sy = y - dy;
                    if (sy < 0 || sy >= expected.Height)
                    {
                        continue;
                    }

                    for (var x = x0; x < x1; x += step)
                    {
                        var sx = x - dx;
                        if (sx < 0 || sx >= expected.Width)
                        {
                            continue;
                        }

                        samples++;
                        if (Math.Abs(Luma(actual, x, y) - Luma(expected, sx, sy)) > 24)
                        {
                            different++;
                        }
                    }
                }

                if (samples > 0)
                {
                    var residual = (double)different / samples;
                    if (best is null || residual < best.Value.Item3)
                    {
                        best = (dx, dy, residual);
                    }
                }
            }
        }

        return best;
    }

    // Agreement (intersection over union) of the edge maps of both images over the changed region, grown by two pixels:
    // a recolored element changes the pixels inside its outline, and its outline is just around them.
    private static double EdgeAgreement(RgbaImage expected, RgbaImage actual, PixelRectangle bounds)
    {
        int both = 0, either = 0;
        var x0 = Math.Max(0, bounds.X - 2);
        var y0 = Math.Max(0, bounds.Y - 2);
        var x1 = Math.Min(actual.Width - 1, bounds.X + bounds.Width + 2);
        var y1 = Math.Min(actual.Height - 1, bounds.Y + bounds.Height + 2);
        for (var y = y0; y < y1; y++)
        {
            for (var x = x0; x < x1; x++)
            {
                var a = Edge(expected, x, y);
                var b = Edge(actual, x, y);
                if (a || b)
                {
                    either++;
                    if (a && b)
                    {
                        both++;
                    }
                }
            }
        }

        return either == 0 ? 0 : (double)both / either;
    }

    private static bool Edge(RgbaImage image, int x, int y)
    {
        var luma = Luma(image, x, y);
        return Math.Abs(luma - Luma(image, x + 1, y)) + Math.Abs(luma - Luma(image, x, y + 1)) > 40;
    }

    // Luminance over white (transparent pixels read as the page behind them).
    private static int Luma(RgbaImage image, int x, int y)
    {
        var (r, g, b, a) = image.GetPixel(x, y);
        var alpha = a / 255.0;
        var lr = (r * alpha) + (255 * (1 - alpha));
        var lg = (g * alpha) + (255 * (1 - alpha));
        var lb = (b * alpha) + (255 * (1 - alpha));
        return (int)((0.299 * lr) + (0.587 * lg) + (0.114 * lb));
    }

    private static string Describe(int dx, int dy)
    {
        var parts = new List<string>();
        if (dy != 0)
        {
            parts.Add(string.Create(CultureInfo.InvariantCulture, $"{Math.Abs(dy)} px {(dy > 0 ? "down" : "up")}"));
        }

        if (dx != 0)
        {
            parts.Add(string.Create(CultureInfo.InvariantCulture, $"{Math.Abs(dx)} px {(dx > 0 ? "right" : "left")}"));
        }

        return "moved " + string.Join(" and ", parts);
    }
}
