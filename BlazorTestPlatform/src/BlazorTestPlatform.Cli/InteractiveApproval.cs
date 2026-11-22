using System.Globalization;
using BlazorTestPlatform.Results;

namespace BlazorTestPlatform.Cli;

/// <summary>What the reviewer answered for one changed snapshot.</summary>
internal enum ApprovalChoice
{
    /// <summary>Make this snapshot the baseline.</summary>
    Approve,

    /// <summary>Leave the baseline as it is.</summary>
    Skip,

    /// <summary>Approve this snapshot and every remaining one without asking.</summary>
    ApproveRest,

    /// <summary>Stop; approve nothing more.</summary>
    Quit,

    /// <summary>The input ended before an answer (no terminal, as on CI): approve nothing more, and fail.</summary>
    EndOfInput,
}

/// <summary>
/// <c>btp approve --interactive</c>: asks about each changed or missing snapshot in the terminal, showing what changed
/// (the change classification, pixel count and image files), as Vitest's interactive snapshot review.
/// </summary>
internal static class InteractiveApproval
{
    /// <summary>Describes <paramref name="visual"/> on <paramref name="output"/> and reads the answer from <paramref name="input"/>.</summary>
    /// <param name="input">Answers, one per line; the end of the input stops with <see cref="ApprovalChoice.EndOfInput"/>.</param>
    /// <param name="output">Where the description and the prompt go.</param>
    /// <param name="visual">The snapshot.</param>
    /// <param name="storyId">Its story.</param>
    /// <param name="resultsDirectory">The results directory the image paths are relative to.</param>
    /// <param name="position">1-based position, for the <c>[2/5]</c> counter.</param>
    /// <param name="count">How many snapshots are reviewed.</param>
    public static ApprovalChoice Ask(TextReader input, TextWriter output, VisualResult visual, string storyId, string resultsDirectory, int position, int count)
    {
        ArgumentNullException.ThrowIfNull(input);
        ArgumentNullException.ThrowIfNull(output);
        ArgumentNullException.ThrowIfNull(visual);

        output.WriteLine();
        output.WriteLine(string.Create(CultureInfo.InvariantCulture, $"[{position}/{count}] {visual.SnapshotName}  ({storyId})"));
        output.WriteLine(visual.Status == VisualStatus.MissingBaseline
            ? "  new snapshot: no baseline yet"
            : string.Create(CultureInfo.InvariantCulture, $"  {visual.Status}: {visual.DiffPixels:N0} px differ ({visual.DiffRatio:P2})"));
        if (visual.ChangeSummary is { } change)
        {
            output.WriteLine($"  looks like: {change}");
        }

        if (visual.Note is { Length: > 0 } note)
        {
            output.WriteLine($"  note: {note}");
        }

        foreach (var (label, path) in new[] { ("baseline", visual.BaselinePath), ("actual", visual.ActualPath), ("diff", visual.DiffPath) })
        {
            if (path is { Length: > 0 })
            {
                output.WriteLine($"  {label,-8} {Path.GetFullPath(Path.Combine(resultsDirectory, path))}");
            }
        }

        while (true)
        {
            output.Write("  Approve? [y]es, [n]o, [a]ll remaining, [q]uit: ");
            var answer = input.ReadLine();
            if (answer is null)
            {
                output.WriteLine();
                return ApprovalChoice.EndOfInput;
            }

            switch (answer.Trim().ToLowerInvariant())
            {
                case "y" or "yes":
                    return ApprovalChoice.Approve;
                case "n" or "no" or "s" or "skip":
                    return ApprovalChoice.Skip;
                case "a" or "all":
                    return ApprovalChoice.ApproveRest;
                case "q" or "quit":
                    return ApprovalChoice.Quit;
                default:
                    output.WriteLine("  Answer y, n, a or q.");
                    break;
            }
        }
    }
}
