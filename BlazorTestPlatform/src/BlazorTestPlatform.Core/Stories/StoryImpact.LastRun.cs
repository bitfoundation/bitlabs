using System.Collections.Concurrent;
using System.Diagnostics.CodeAnalysis;
using System.Runtime.Versioning;
using System.Text.Json;
using BlazorTestPlatform.Configuration;
using BlazorTestPlatform.Results;

namespace BlazorTestPlatform;

public sealed partial class StoryImpact
{
    /// <summary>
    /// The <c>BTP_CHANGED_SINCE</c> value that selects the stories affected by changes since the previous run, plus those
    /// that failed in it (Vitest's <c>--since-last-run</c>): edit, rerun, and only what the edit can affect runs again.
    /// </summary>
    public const string LastRun = "last-run";

    /// <summary>File in the results directory that records the working tree of the last run.</summary>
    public const string LastRunFileName = "last-run.json";

    // More candidate files than this are not compared one by one: all count as changed.
    private const int MaxComparedFiles = 2000;
    private const int GitBatch = 100;
    private const string Absent = "absent";
    private const string Unknown = "unknown";

    private static readonly ConcurrentDictionary<string, Lazy<LastRunSelection>> LastRunSelections = new(StringComparer.OrdinalIgnoreCase);

    /// <summary>
    /// Files whose content differs from what they held when the last run recorded the working tree
    /// (<see cref="RecordRun"/>) in <paramref name="resultsDirectory"/>: committed changes since then, edits, reverted
    /// edits and new files, relative to the repository root. Null when nothing was recorded or git cannot answer (not a
    /// repository, the recorded commit is gone): callers should then run everything.
    /// </summary>
    [UnsupportedOSPlatform("browser")]
    public static IReadOnlyList<string>? ChangedFilesSinceLastRun(string resultsDirectory, string? workingDirectory = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(resultsDirectory);
        return Compare(LoadRecord(resultsDirectory)?.Tree, CaptureWorkingTree(workingDirectory));
    }

    /// <summary>
    /// Records the working tree (the commit, and the content of every file that differs from it) in
    /// <paramref name="resultsDirectory"/>, as the reference for the next <see cref="ChangedFilesSinceLastRun"/>.
    /// False when git cannot answer. <see cref="FromEnvironment"/> records it when a <c>last-run</c> selection ends.
    /// </summary>
    [UnsupportedOSPlatform("browser")]
    public static bool RecordRun(string resultsDirectory, string? workingDirectory = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(resultsDirectory);
        return CaptureWorkingTree(workingDirectory) is { } tree && SaveRecord(resultsDirectory, tree, []);
    }

    // BTP_CHANGED_SINCE=last-run: read once per process (before this run writes results or records its tree), so every
    // test of the run sees the same selection; the tree as it was when the run started is recorded when the process exits.
    [RequiresUnreferencedCode("Reads the IL of component render methods to find the components they render.")]
    [UnsupportedOSPlatform("browser")]
    internal static StoryImpact SinceLastRun(StoryCatalog catalog, BtpSettings settings, string? workingDirectory)
    {
        var directory = settings.ResultsDirectoryPath;
        var selection = LastRunSelections.GetOrAdd(directory + "|" + workingDirectory, _ => new Lazy<LastRunSelection>(() =>
        {
            var failed = StoryRunFilter.FailedInPreviousRun(settings);
            var started = DateTimeOffset.UtcNow;
            var tree = CaptureWorkingTree(workingDirectory);
            var record = LoadRecord(directory);
            var created = new LastRunSelection(Compare(record?.Tree, tree), failed, record?.Pending ?? new HashSet<string>(StringComparer.Ordinal), started);
            if (tree is not null)
            {
                AppDomain.CurrentDomain.ProcessExit += (_, _) =>
                {
                    try
                    {
                        SaveRecord(directory, tree, created.StillPending(directory));
                    }
                    catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
                    {
                        // No record: the next run compares with the one before, or runs everything.
                    }
                };
            }

            return created;
        })).Value;

        var impact = selection.Changed is null
            ? new StoryImpact(catalog.Stories, runsEverything: true, [], new Dictionary<string, IReadOnlyList<string>>())
            : Analyze(catalog, selection.Changed);
        if (!impact.RunsEverything)
        {
            // Stories that failed last time run again even when nothing they depend on changed, and so do those the last
            // run selected but did not run (a filter, a shard, the failure limit): otherwise a change or a failure would
            // drop out of the next run without having been checked.
            var reasons = impact.Reasons.ToDictionary(r => r.Key, r => r.Value, StringComparer.Ordinal);
            foreach (var story in catalog.Stories)
            {
                var why = selection.Failed.Contains(story.Id) ? "(failed in the last run)"
                    : selection.PreviouslyPending.Contains(story.Id) ? "(selected before but not run yet)"
                    : null;
                if (why is not null)
                {
                    reasons[story.Id] = [.. reasons.GetValueOrDefault(story.Id) ?? [], why];
                }
            }

            impact = new StoryImpact(catalog.Stories.Where(s => reasons.ContainsKey(s.Id)).ToArray(), runsEverything: false, [], reasons);
        }

        selection.Track(catalog, impact.Stories);
        return impact;
    }

    // The stories this process's last-run selection would leave pending if it ended now (for tests).
    internal static IReadOnlyCollection<string> PendingAfterRun(BtpSettings settings, string? workingDirectory) =>
        LastRunSelections.TryGetValue(settings.ResultsDirectoryPath + "|" + workingDirectory, out var selection)
            ? selection.Value.StillPending(settings.ResultsDirectoryPath)
            : [];

    // One process's last-run selection: what it selected, so that the stories it did not finish stay selected next time.
    private sealed class LastRunSelection(IReadOnlyList<string>? changed, IReadOnlySet<string> failed, IReadOnlySet<string> previouslyPending, DateTimeOffset started)
    {
        private readonly ConcurrentDictionary<string, bool> _selected = new(StringComparer.Ordinal);
        private readonly ConcurrentDictionary<string, bool> _seen = new(StringComparer.Ordinal);

        public IReadOnlyList<string>? Changed => changed;

        public IReadOnlySet<string> Failed => failed;

        public IReadOnlySet<string> PreviouslyPending => previouslyPending;

        public void Track(StoryCatalog catalog, IEnumerable<Story> selected)
        {
            foreach (var story in catalog.Stories)
            {
                _seen[story.Id] = true;
            }

            foreach (var story in selected)
            {
                _selected[story.Id] = true;
            }
        }

        // Selected stories without a finished result from this run (none written, or a failed one, which the failed
        // results select anyway), and earlier pending stories of catalogs this process never saw.
        public IReadOnlyCollection<string> StillPending(string resultsDirectory)
        {
            var ran = new ResultsStore(resultsDirectory).ReadAll()
                .Where(r => r.StartedAt >= started)
                .GroupBy(r => r.StoryId, StringComparer.Ordinal)
                .Where(g => g.All(r => r.Outcome != StoryOutcome.Failed))
                .Select(g => g.Key)
                .ToHashSet(StringComparer.Ordinal);
            return _selected.Keys.Where(id => !ran.Contains(id))
                .Concat(previouslyPending.Where(id => !_seen.ContainsKey(id)))
                .Distinct(StringComparer.Ordinal)
                .Order(StringComparer.Ordinal)
                .ToArray();
        }
    }

    private sealed record LastRunRecord(WorkingTree Tree, IReadOnlySet<string> Pending);

    // The working tree: HEAD, and the git blob id of each file that differs from it (Absent for deleted files, Unknown for
    // directories such as submodules), keyed by the path relative to the repository root.
    internal sealed record WorkingTree(string Root, string Commit, IReadOnlyDictionary<string, string> Files);

    [UnsupportedOSPlatform("browser")]
    internal static WorkingTree? CaptureWorkingTree(string? workingDirectory)
    {
        var root = Git(workingDirectory, "rev-parse", "--show-toplevel")?.Trim();
        var commit = string.IsNullOrEmpty(root) ? null : Git(root, "rev-parse", "--verify", "HEAD")?.Trim();
        if (string.IsNullOrEmpty(root) || string.IsNullOrEmpty(commit))
        {
            return null;
        }

        var modified = Git(root, "diff", "--name-only", "--no-renames", "-z", "HEAD");
        var untracked = Git(root, "ls-files", "--others", "--exclude-standard", "--full-name", "-z");
        if (modified is null || untracked is null)
        {
            return null;
        }

        var paths = Paths(modified).Concat(Paths(untracked)).Distinct(StringComparer.Ordinal).ToArray();
        var files = HashWorkingFiles(root, paths);
        return files is null ? null : new WorkingTree(root, commit, files);
    }

    [UnsupportedOSPlatform("browser")]
    internal static IReadOnlyList<string>? Compare(WorkingTree? previous, WorkingTree? current)
    {
        if (previous is null || current is null)
        {
            return null;
        }

        var root = current.Root;
        var committed = string.Equals(previous.Commit, current.Commit, StringComparison.Ordinal)
            ? string.Empty
            : Git(root, "diff", "--name-only", "--no-renames", "-z", previous.Commit, current.Commit);
        if (committed is null)
        {
            // The recorded commit is gone (rebased away, garbage collected): nothing can be ruled out.
            return null;
        }

        var candidates = Paths(committed).Concat(previous.Files.Keys).Concat(current.Files.Keys).Distinct(StringComparer.Ordinal).ToArray();
        if (candidates.Length > MaxComparedFiles)
        {
            return candidates;
        }

        // A file the record does not list held its committed content then; one the working tree does not list holds it now.
        var before = CommittedBlobs(root, previous.Commit, candidates.Where(f => !previous.Files.ContainsKey(f)));
        var after = CommittedBlobs(root, current.Commit, candidates.Where(f => !current.Files.ContainsKey(f)));
        if (before is null || after is null)
        {
            return null;
        }

        return candidates.Where(file =>
        {
            var then = previous.Files.GetValueOrDefault(file) ?? before.GetValueOrDefault(file) ?? Absent;
            var now = current.Files.GetValueOrDefault(file) ?? after.GetValueOrDefault(file) ?? Absent;
            return then == Unknown || now == Unknown || !string.Equals(then, now, StringComparison.Ordinal);
        }).ToArray();
    }

    private static IEnumerable<string> Paths(string output) => output.Split('\0', StringSplitOptions.RemoveEmptyEntries).Select(p => p.Trim('\n', '\r')).Where(p => p.Length > 0);

    // git hash-object applies the repository's filters (line endings), so an edit that was reverted hashes to the
    // committed blob again.
    [UnsupportedOSPlatform("browser")]
    private static Dictionary<string, string>? HashWorkingFiles(string root, IReadOnlyList<string> paths)
    {
        var files = new Dictionary<string, string>(StringComparer.Ordinal);
        var existing = new List<string>();
        foreach (var path in paths)
        {
            var full = Path.Combine(root, path);
            if (File.Exists(full))
            {
                existing.Add(path);
            }
            else
            {
                files[path] = Directory.Exists(full) ? Unknown : Absent;
            }
        }

        foreach (var batch in existing.Chunk(GitBatch))
        {
            var output = Git(root, ["hash-object", "--", .. batch]);
            var hashes = output?.Split('\n', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
            if (hashes is null || hashes.Length != batch.Length)
            {
                return null;
            }

            for (var i = 0; i < batch.Length; i++)
            {
                files[batch[i]] = hashes[i];
            }
        }

        return files;
    }

    // Blob ids of paths in a commit (paths missing from it are left out): "<mode> <type> <id>\t<path>" entries.
    [UnsupportedOSPlatform("browser")]
    private static Dictionary<string, string>? CommittedBlobs(string root, string commit, IEnumerable<string> paths)
    {
        var blobs = new Dictionary<string, string>(StringComparer.Ordinal);
        foreach (var batch in paths.Chunk(GitBatch))
        {
            // Literal paths: a name such as Item[1].razor is not a pattern.
            var output = Git(root, ["--literal-pathspecs", "ls-tree", "-r", "-z", "--full-tree", commit, "--", .. batch]);
            if (output is null)
            {
                return null;
            }

            foreach (var entry in Paths(output))
            {
                var tab = entry.IndexOf('\t', StringComparison.Ordinal);
                var fields = tab < 0 ? [] : entry[..tab].Split(' ');
                if (fields.Length == 3)
                {
                    blobs[entry[(tab + 1)..]] = fields[1] == "blob" ? fields[2] : Unknown;
                }
            }
        }

        return blobs;
    }

    private static LastRunRecord? LoadRecord(string resultsDirectory)
    {
        var path = Path.Combine(Path.GetFullPath(resultsDirectory), LastRunFileName);
        try
        {
            using var document = JsonDocument.Parse(File.ReadAllText(path));
            var commit = document.RootElement.GetProperty("commit").GetString();
            if (string.IsNullOrEmpty(commit))
            {
                return null;
            }

            var files = new Dictionary<string, string>(StringComparer.Ordinal);
            foreach (var file in document.RootElement.GetProperty("files").EnumerateObject())
            {
                files[file.Name] = file.Value.GetString() ?? Unknown;
            }

            var pending = new HashSet<string>(StringComparer.Ordinal);
            if (document.RootElement.TryGetProperty("pending", out var ids) && ids.ValueKind == JsonValueKind.Array)
            {
                pending.UnionWith(ids.EnumerateArray().Select(id => id.GetString()).OfType<string>());
            }

            return new LastRunRecord(new WorkingTree(string.Empty, commit, files), pending);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or JsonException or KeyNotFoundException or InvalidOperationException)
        {
            // No record yet (or an unreadable one): run everything.
            return null;
        }
    }

    private static bool SaveRecord(string resultsDirectory, WorkingTree tree, IReadOnlyCollection<string> pending)
    {
        try
        {
            var directory = Directory.CreateDirectory(Path.GetFullPath(resultsDirectory)).FullName;
            var path = Path.Combine(directory, LastRunFileName);
            var temp = $"{path}.{Environment.ProcessId}.tmp";
            using (var stream = File.Create(temp))
            using (var writer = new Utf8JsonWriter(stream, new JsonWriterOptions { Indented = true }))
            {
                writer.WriteStartObject();
                writer.WriteString("commit", tree.Commit);
                writer.WriteString("recordedAt", DateTimeOffset.UtcNow);
                writer.WriteStartObject("files");
                foreach (var (file, hash) in tree.Files.OrderBy(f => f.Key, StringComparer.Ordinal))
                {
                    writer.WriteString(file, hash);
                }

                writer.WriteEndObject();
                writer.WriteStartArray("pending");
                foreach (var id in pending)
                {
                    writer.WriteStringValue(id);
                }

                writer.WriteEndArray();
                writer.WriteEndObject();
            }

            File.Move(temp, path, overwrite: true);
            return true;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            return false;
        }
    }
}
