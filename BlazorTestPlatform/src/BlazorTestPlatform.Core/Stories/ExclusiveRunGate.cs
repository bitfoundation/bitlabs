namespace BlazorTestPlatform;

/// <summary>
/// Lets a story run alone (<c>Exclusive()</c>), as Playwright's exclusive test lock: runners in a process share this gate,
/// ordinary story runs together and an exclusive one only once the others finish, while new runs wait for it. Performance
/// stories measured against baselines get a quiet machine; nothing else is slowed beyond the exclusive run itself.
/// </summary>
/// <remarks>
/// A readers-writer gate with writer preference (a turnstile), so an exclusive run is not starved by a stream of ordinary
/// ones. It spans the runners of one process (bUnit and browsers alike); test frameworks that run assemblies in separate
/// processes need their own setting to run those alone.
/// </remarks>
internal static class ExclusiveRunGate
{
    private static readonly SemaphoreSlim Turnstile = new(1, 1);
    private static readonly SemaphoreSlim Room = new(1, 1);
    private static readonly SemaphoreSlim Readers = new(1, 1);
    private static int _readers;

    // Set inside a gated run: a story run started from within it (a test helper running another story) is part of it,
    // so it does not wait at the gate the outer run holds, which would deadlock.
    private static readonly AsyncLocal<bool> Inside = new();

    /// <summary>Enters the gate; dispose the result when the run ends.</summary>
    public static async Task<IAsyncDisposable> EnterAsync(bool exclusive, CancellationToken cancellationToken)
    {
        if (Inside.Value)
        {
            return new Exit(() => Task.CompletedTask);
        }

        return await EnterGateAsync(exclusive, cancellationToken).ConfigureAwait(false);
    }

    /// <summary>
    /// Marks the caller's flow as inside a gated run, so story runs started from it pass the gate. Runners call it in their
    /// own method right after entering: a value set inside <see cref="EnterAsync"/> would not flow back out of it.
    /// </summary>
    public static void MarkInside() => Inside.Value = true;

    private static async Task<IAsyncDisposable> EnterGateAsync(bool exclusive, CancellationToken cancellationToken)
    {
        if (exclusive)
        {
            await Turnstile.WaitAsync(cancellationToken).ConfigureAwait(false);
            try
            {
                await Room.WaitAsync(cancellationToken).ConfigureAwait(false);
            }
            catch
            {
                Turnstile.Release();
                throw;
            }

            return new Exit(() =>
            {
                Room.Release();
                Turnstile.Release();
                return Task.CompletedTask;
            });
        }

        // Pass the turnstile: waits while an exclusive run waits or runs.
        await Turnstile.WaitAsync(cancellationToken).ConfigureAwait(false);
        Turnstile.Release();

        await Readers.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            if (++_readers == 1)
            {
                try
                {
                    await Room.WaitAsync(cancellationToken).ConfigureAwait(false);
                }
                catch
                {
                    _readers--;
                    throw;
                }
            }
        }
        finally
        {
            Readers.Release();
        }

        return new Exit(async () =>
        {
            await Readers.WaitAsync(CancellationToken.None).ConfigureAwait(false);
            try
            {
                if (--_readers == 0)
                {
                    Room.Release();
                }
            }
            finally
            {
                Readers.Release();
            }
        });
    }

    private sealed class Exit(Func<Task> release) : IAsyncDisposable
    {
        private int _done;

        public async ValueTask DisposeAsync()
        {
            if (Interlocked.Exchange(ref _done, 1) == 0)
            {
                await release().ConfigureAwait(false);
            }
        }
    }
}
