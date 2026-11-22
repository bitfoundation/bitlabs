using System.Globalization;
using System.Text.RegularExpressions;
using BlazorTestPlatform.Drivers;

namespace BlazorTestPlatform;

/// <summary>
/// "Did you mean" for locators that found nothing: the elements of the story's accessibility tree that come closest to
/// what the locator asked for (same role with a similar name, or a similar name under another role), written as the
/// locator code that finds them. Playwright's agents heal locators the same way, from the accessibility tree.
/// </summary>
internal static partial class LocatorSuggestions
{
    private const int MaxSuggestions = 3;
    private const double MinSimilarity = 0.5;

    // Names longer than this are cut before comparing (paragraph text): suggestions stay cheap on large trees.
    private const int MaxNameLength = 120;

    [GeneratedRegex("""^\s*- (?<role>[a-z]+)(?: "(?<name>(?:[^"\\]|\\.)*)")?""")]
    private static partial Regex TreeLine();

    /// <summary>Up to three locators, as C# code, for elements like the one <paramref name="locator"/> looks for.</summary>
    public static IReadOnlyList<string> For(LocatorChain locator, IEnumerable<string> treeLines)
    {
        ArgumentNullException.ThrowIfNull(locator);
        ArgumentNullException.ThrowIfNull(treeLines);
        var wanted = locator.Steps.LastOrDefault(s => s.Kind == LocatorStepKind.Find)?.Query;
        var (role, name) = wanted switch
        {
            RoleQuery query => (query.RoleName, query.Name),
            TextQuery query => (null, query.Value),
            LabelQuery query => (null, query.Value),
            TitleQuery query => (null, query.Value),
            AltTextQuery query => (null, query.Value),
            PlaceholderQuery query => (null, query.Value),
            _ => ((string?)null, (string?)null),
        };
        if (role is null && string.IsNullOrEmpty(name))
        {
            return [];
        }

        var candidates = treeLines
            .Select(line => TreeLine().Match(line))
            .Where(m => m.Success && m.Groups["name"].Success && m.Groups["name"].Value.Length > 0)
            .Select(m => (Role: m.Groups["role"].Value, Name: Unescape(m.Groups["name"].Value)))
            .Distinct()
            .Select(c => (c.Role, c.Name, Score: Score(role, name, c.Role, c.Name)))
            .Where(c => c.Score >= MinSimilarity)
            .OrderByDescending(c => c.Score)
            .ThenBy(c => c.Name, StringComparer.Ordinal)
            .Take(MaxSuggestions);
        return [.. candidates.Select(c => $"ctx.GetByRole(\"{Escape(c.Role)}\", \"{Escape(c.Name)}\")")];
    }

    // The same role counts most; then how close the names are (a missing name matches any name of that role a little).
    private static double Score(string? wantedRole, string? wantedName, string role, string name)
    {
        var sameRole = wantedRole is not null && string.Equals(wantedRole, role, StringComparison.OrdinalIgnoreCase);
        if (string.IsNullOrEmpty(wantedName))
        {
            return sameRole ? MinSimilarity : 0;
        }

        // A name was asked for: only similar names count, whatever the role ("Save" must not suggest "Cancel").
        var similarity = Similarity(Cut(wantedName), Cut(name));
        if (similarity < MinSimilarity)
        {
            return 0;
        }

        return wantedRole is null ? similarity : sameRole ? 0.4 + (0.6 * similarity) : 0.8 * similarity;
    }

    // 1 for equal names (ignoring case), less the edit distance relative to the longer one; containing the other is close.
    private static double Similarity(string a, string b)
    {
        a = a.Trim().ToLower(CultureInfo.InvariantCulture);
        b = b.Trim().ToLower(CultureInfo.InvariantCulture);
        if (a.Length == 0 || b.Length == 0)
        {
            return 0;
        }

        if (a.Contains(b, StringComparison.Ordinal) || b.Contains(a, StringComparison.Ordinal))
        {
            return Math.Max(0.75, 1 - (Math.Abs(a.Length - b.Length) / (double)Math.Max(a.Length, b.Length)));
        }

        var previous = new int[b.Length + 1];
        var current = new int[b.Length + 1];
        for (var j = 0; j <= b.Length; j++)
        {
            previous[j] = j;
        }

        for (var i = 1; i <= a.Length; i++)
        {
            current[0] = i;
            for (var j = 1; j <= b.Length; j++)
            {
                current[j] = Math.Min(Math.Min(current[j - 1] + 1, previous[j] + 1), previous[j - 1] + (a[i - 1] == b[j - 1] ? 0 : 1));
            }

            (previous, current) = (current, previous);
        }

        return 1 - (previous[b.Length] / (double)Math.Max(a.Length, b.Length));
    }

    private static string Cut(string value) => value.Length <= MaxNameLength ? value : value[..MaxNameLength];

    // Snapshot names escape quotes and backslashes; anything else stays as written.
    private static string Unescape(string value) => value.Replace("\\\"", "\"", StringComparison.Ordinal).Replace("\\\\", "\\", StringComparison.Ordinal);

    private static string Escape(string value) => value.Replace("\\", "\\\\", StringComparison.Ordinal).Replace("\"", "\\\"", StringComparison.Ordinal);
}
