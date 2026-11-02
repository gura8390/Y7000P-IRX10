using System;
using System.Diagnostics;
using System.Threading;
using System.Threading.Tasks;

namespace LenovoLegionToolkit.Lib.Thermal;

public interface ICpuPowerTransport
{
    Task WriteAsync(CpuPowerEnvelope target);
    Task<CpuPowerEnvelope> ReadAsync();
}
public enum ApplyStatus { Rejected, Verified, Failed, UnknownCompletion }
public sealed record ApplyResult(ApplyStatus Status, string Reason);

// Single flight includes readback. Timed-out I/O retains ownership until physically complete.
public sealed class HardwareCommandExecutor(ICpuPowerTransport transport, CpuPowerCapability capability,
    Func<long> now, Func<(long Policy, long Mode)> epochs)
{
    private readonly SemaphoreSlim _gate = new(1, 1);
    private int _pending;
    public Task<ApplyResult> ApplyAsync(PowerCommand command, TimeSpan timeout, CancellationToken token = default) =>
        ExecuteAsync(command, timeout, true, token);

    public Task<ApplyResult> VerifyAsync(PowerCommand command, TimeSpan timeout, CancellationToken token = default) =>
        ExecuteAsync(command, timeout, false, token);

    private async Task<ApplyResult> ExecuteAsync(PowerCommand command, TimeSpan timeout, bool write, CancellationToken token)
    {
        if (timeout <= TimeSpan.Zero || timeout.TotalMilliseconds > int.MaxValue)
            throw new ArgumentOutOfRangeException(nameof(timeout));
        ArgumentNullException.ThrowIfNull(command);
        token.ThrowIfCancellationRequested();
        if (!IsCurrent(command))
            return new(ApplyStatus.Rejected, "Expired, superseded or unsupported command.");
        var started = Stopwatch.GetTimestamp();
        // Bounded admission prevents unbounded stale command backlogs.
        if (Interlocked.Increment(ref _pending) > 8)
        {
            Interlocked.Decrement(ref _pending);
            return new(ApplyStatus.Rejected, "Command capacity exceeded.");
        }
        bool acquired = false;
        try
        {
            // The timeout covers admission AND I/O, including a stalled preceding command.
            var remaining = RemainingBudget(command, timeout, started);
            if (remaining <= TimeSpan.Zero || !await _gate.WaitAsync(remaining, token).ConfigureAwait(false))
                return new(ApplyStatus.Rejected, "Deadline elapsed before hardware operation started.");
            acquired = true;
            token.ThrowIfCancellationRequested();
            if (!IsCurrent(command))
                return new(ApplyStatus.Rejected, "Expired, superseded or unsupported command.");
            remaining = RemainingBudget(command, timeout, started);
            if (remaining <= TimeSpan.Zero)
                return new(ApplyStatus.Rejected, "Deadline elapsed before hardware operation started.");
            // Some vendor adapters block synchronously before returning their Task. Keep
            // that prefix off the caller thread so the same timeout still bounds its wait.
            var operation = Task.Run(() => ApplyCoreAsync(command, write));
            try { return await operation.WaitAsync(remaining, token).ConfigureAwait(false); }
            catch (Exception ex) when (ex is TimeoutException || ex is OperationCanceledException)
            {
                // Cancellation stops waiting, NOT physical I/O. A continuation owns the gate.
                acquired = false;
                _ = ReleaseAfterCompletionAsync(operation);
                return new(ApplyStatus.UnknownCompletion, "Physical operation still owns the executor.");
            }
        }
        finally
        {
            if (acquired) _gate.Release();
            Interlocked.Decrement(ref _pending);
        }
    }
    private bool IsCurrent(PowerCommand command)
    {
        var epoch = epochs();
        return command.ExpiresAt > now() && command.PolicyEpoch == epoch.Policy && command.ModeEpoch == epoch.Mode
            && capability.IsAligned(command.Target);
    }
    private TimeSpan RemainingBudget(PowerCommand command, TimeSpan timeout, long started)
    {
        var clock = now();
        if (command.ExpiresAt <= clock) return TimeSpan.Zero;
        var remaining = timeout - Stopwatch.GetElapsedTime(started);
        // Decimal subtraction avoids overflowing if supplied clocks are invalid.
        var deadlineMilliseconds = (decimal)command.ExpiresAt - clock;
        return (double)deadlineMilliseconds < remaining.TotalMilliseconds
            ? TimeSpan.FromMilliseconds((double)deadlineMilliseconds) : remaining;
    }
    private async Task<ApplyResult> ApplyCoreAsync(PowerCommand command, bool write)
    {
        try
        {
            if (!IsCurrent(command)) return new(ApplyStatus.Rejected, "Command superseded before physical operation started.");
            if (write) await transport.WriteAsync(command.Target).ConfigureAwait(false);
            var actual = await transport.ReadAsync().ConfigureAwait(false);
            var epoch = epochs();
            return actual == command.Target && capability.IsAligned(actual)
                && command.ModeEpoch == epoch.Mode && command.PolicyEpoch == epoch.Policy
                ? new(ApplyStatus.Verified, "Readback confirmed.")
                : new(ApplyStatus.Failed, "Readback mismatch or mode/policy changed during I/O.");
        }
        catch (Exception ex) { return new(ApplyStatus.Failed, $"{ex.GetType().Name}: {ex.Message}"); }
    }
    private async Task ReleaseAfterCompletionAsync(Task<ApplyResult> operation)
    {
        try { await operation.ConfigureAwait(false); }
        finally { _gate.Release(); }
    }
}
