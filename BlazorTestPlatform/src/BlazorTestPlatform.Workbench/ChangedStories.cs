using System.Diagnostics.CodeAnalysis;

namespace BlazorTestPlatform.Workbench;

/// <summary>
/// The stories the working tree's changes affect (<see cref="WorkbenchOptions.ChangedSince"/>), for the sidebar's
/// "changed" markers, as Storybook 10.4's change-aware review: test impact analysis (<see cref="StoryImpact"/>) over the
/// files git reports changed since the reference. The analysis runs in the background, one at a time, at most every few
/// seconds; renders show the last result and never wait for git, except briefly for the very first one.
/// </summary>
public sealed class ChangedStories
{
    private static readonly TimeSpan Freshness = TimeSpan.FromSeconds(15);
    private static readonly TimeSpan FirstWait = TimeSpan.FromSeconds(2);
    private static readonly Snapshot Pending = new(new Dictionary<string, IReadOnlyList<string>>(), AnyStory: false, Available: false);

    private readonly Lock _gate = new();
    private string? _key;
    private (DateTime At, Snapshot Value)? _cached;
    private Task<Snapshot>? _refresh;

    /// <summary>What changed, as of one computation.</summary>
    /// <param name="Reasons">For each affected story id, the changed files that affect it.</param>
    /// <param name="AnyStory">True when a changed shared file (or build file) may affect any story.</param>
    /// <param name="Available">False when git could not answer (not a repository, unknown reference) or has not yet.</param>
    public sealed record Snapshot(IReadOnlyDictionary<string, IReadOnlyList<string>> Reasons, bool AnyStory, bool Available);

    /// <summary>
    /// The stories of <paramref name="catalog"/> affected since <paramref name="reference"/>, in the repository of
    /// <paramref name="workingDirectory"/> (default: the current directory): the latest result, refreshed in the background
    /// when older than a few seconds.
    /// </summary>
    public Snapshot For(StoryCatalog catalog, string reference, string? workingDirectory = null)
    {
        ArgumentNullException.ThrowIfNull(catalog);
        ArgumentException.ThrowIfNullOrWhiteSpace(reference);
        var key = reference.Trim() + "|" + workingDirectory;
        Task<Snapshot> refresh;
        lock (_gate)
        {
            if (_key != key)
            {
                // Another reference or repository: earlier results do not apply.
                _key = key;
                _cached = null;
                _refresh = null;
            }

            if (_cached is { } fresh && DateTime.UtcNow - fresh.At < Freshness)
            {
                return fresh.Value;
            }

            // One computation at a time (no thundering herd when many renders find the result stale).
            refresh = _refresh ??= Task.Run(() => Compute(catalog, reference.Trim(), workingDirectory, key));
            if (_cached is { } stale)
            {
                return stale.Value;
            }
        }

        // Nothing computed yet: wait a little for the first result, then render without markers.
        return refresh.Wait(FirstWait) ? refresh.Result : Pending;
    }

    [UnconditionalSuppressMessage("Trimming", "IL2026", Justification = "Off by default; a development workbench sets ChangedSince. In a trimmed app, render code may be missing and the analysis marks fewer components, which only affects the markers.")]
    private Snapshot Compute(StoryCatalog catalog, string reference, string? workingDirectory, string key)
    {
        Snapshot value;
        try
        {
            var files = OperatingSystem.IsBrowser() ? null : StoryImpact.ChangedFilesSince(reference, workingDirectory);
            if (files is null)
            {
                value = Pending;
            }
            else
            {
                var impact = StoryImpact.Analyze(catalog, files);
                value = new Snapshot(impact.RunsEverything ? new Dictionary<string, IReadOnlyList<string>>() : impact.Reasons, impact.RunsEverything, Available: true);
            }
        }
        catch (Exception ex) when (ex is ArgumentException or InvalidOperationException or IOException)
        {
            // An invalid reference (one starting with '-') or a git failure: no markers, never a broken page.
            value = Pending;
        }

        lock (_gate)
        {
            if (_key == key)
            {
                _cached = (DateTime.UtcNow, value);
                _refresh = null;
            }
        }

        return value;
    }
}
