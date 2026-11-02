using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Controllers;
using LenovoLegionToolkit.Lib.Thermal;

var scenarios = 0;
void Check(bool value, string name) { if (!value) throw new Exception(name); scenarios++; }
async Task Reject(Func<Task> action, string name)
{
    try { await action(); }
    catch (Exception ex) when (ex is InvalidOperationException or NotSupportedException or ArgumentOutOfRangeException)
    { scenarios++; return; }
    throw new Exception("Accepted invalid operation: " + name);
}

var fake = new FakeFirmware();
var transport = await ThermalCpuTransport.DiscoverAsync(fake);
Check(fake.Writes.Count == 0 && transport.Baseline == new CpuPowerEnvelope(80, 100), "Discovery is read-only and captures baseline");
await Reject(() => transport.WriteAsync(new(50, 70)), "Disabled transport");
await transport.EnableAsync();
Check(fake.Values[CpuThermalWriteCoordinator.TemperatureId] == 88, "Enable writes firmware headroom");
await transport.VerifyOwnershipAsync();
Check(fake.Writes.All(w => w.Id is CpuThermalWriteCoordinator.TemperatureId), "Enable does not touch GPU or fans");
await transport.WriteAsync(new(40, 50));
Check(fake.Writes[^2].Id == CpuThermalWriteCoordinator.LongPowerId && fake.Writes[^1].Id == CpuThermalWriteCoordinator.ShortPowerId, "Restriction lowers PL1 before PL2 when necessary");
await transport.WriteAsync(new(70, 90));
Check(fake.Writes[^2].Id == CpuThermalWriteCoordinator.ShortPowerId && fake.Writes[^1].Id == CpuThermalWriteCoordinator.LongPowerId, "Recovery raises PL2 before PL1 when necessary");
await Reject(() => transport.WriteAsync(new(90, 110)), "Cannot exceed user baseline");
await Reject(() => transport.SetTemperatureLimitAsync(91), "Cannot raise temperature beyond 90");
await transport.RestoreIfOwnedAsync();
Check(await transport.ReadAsync() == new CpuPowerEnvelope(80, 100) && fake.Values[CpuThermalWriteCoordinator.TemperatureId] == 100, "Verified owned baseline restores");
await transport.EnableAsync();
await transport.VerifyOwnershipAsync();
Check(await transport.ReadAsync() == new CpuPowerEnvelope(80, 100), "Clean restart resets internal ceiling to the verified baseline");
await transport.RestoreIfOwnedAsync();

foreach (var identity in new[]
{
    new ThermalMachineIdentity("Other", "83NN", "S9CN15WW", "i9-14900HX"),
    new ThermalMachineIdentity("LENOVO", "Other", "S9CN15WW", "i9-14900HX"),
    new ThermalMachineIdentity("LENOVO", "83NN", "Unverified", "i9-14900HX"),
    new ThermalMachineIdentity("LENOVO", "83NN", "S9CN15WW", "i7-14650HX")
})
{
    fake = new FakeFirmware { Identity = identity };
    await Reject(async () => { await ThermalCpuTransport.DiscoverAsync(fake); }, "Unverified machine profile");
}
fake = new FakeFirmware { Mode = 3 };
await Reject(async () => { await ThermalCpuTransport.DiscoverAsync(fake); }, "No implicit mode switching");
fake = new FakeFirmware();
fake.Ranges.RemoveAt(0);
await Reject(async () => { await ThermalCpuTransport.DiscoverAsync(fake); }, "Missing range");
fake = new FakeFirmware();
fake.Discrete.Add(new(CapabilityID.CPULongTermPowerLimit, 80));
await Reject(async () => { await ThermalCpuTransport.DiscoverAsync(fake); }, "Discrete firmware needs adapter");
fake = new FakeFirmware();
fake.Ranges[0] = new(CapabilityID.CPULongTermPowerLimit, 80, 10, 150, 2);
await Reject(async () => { await ThermalCpuTransport.DiscoverAsync(fake); }, "Different power grids");

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
fake.Values[CpuThermalWriteCoordinator.LongPowerId] = 70;
await Reject(() => transport.EnableAsync(), "Stale discovery baseline");
Check(!transport.IsEnabled && fake.Writes.Count == 0, "Failed stale enable releases lease and performs no writes");

fake = new FakeFirmware { OtherController = true };
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await Reject(() => transport.EnableAsync(), "Competing firmware process");

fake = new FakeFirmware();
fake.Values[CpuThermalWriteCoordinator.TemperatureId] = 84;
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await transport.EnableAsync();
Check(fake.Values[CpuThermalWriteCoordinator.TemperatureId] == 84, "Keep the user's stricter original temperature limit");
await Reject(() => transport.SetTemperatureLimitAsync(88), "Do not relax the user's original temperature limit");
await transport.RestoreIfOwnedAsync();

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await transport.EnableAsync();
await transport.WriteAsync(new(50, 70));
var writesBeforeRejectedUserChange = fake.Writes.Count;
await Reject(() => CpuThermalWriteCoordinator.WriteAsync(CpuThermalWriteCoordinator.LongPowerId, 120, fake.WriteAsync, fake.ReadAsync),
    "Changing a CPU budget while control owns it is explicitly rejected");
Check(fake.Writes.Count == writesBeforeRejectedUserChange && !transport.IsSuperseded,
    "A rejected budget change causes no firmware write and does not invalidate the active owner");
await transport.VerifyOwnershipAsync();
await transport.RestoreIfOwnedAsync();
Check(await transport.ReadAsync() == new CpuPowerEnvelope(80, 100), "Rejected user request cannot trap the next baseline at a thermal ceiling");
await CpuThermalWriteCoordinator.WriteAsync(CpuThermalWriteCoordinator.LongPowerId, 90, fake.WriteAsync, fake.ReadAsync);
Check(fake.Values[CpuThermalWriteCoordinator.LongPowerId] == 90, "After stopping, the requested user power is applied without substitution");
var freshUserTransport = await ThermalCpuTransport.DiscoverAsync(fake);
Check(freshUserTransport.Baseline == new CpuPowerEnvelope(90, 100), "Fresh discovery sees the actual new user budget");

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await transport.EnableAsync();
await transport.WriteAsync(new(50, 70));
writesBeforeRejectedUserChange = fake.Writes.Count;
foreach (var (id, value) in new[] { (CpuThermalWriteCoordinator.ShortPowerId, 40),
    (CpuThermalWriteCoordinator.TemperatureId, 85), (0x01030000u, 100), (0x01060000u, 100), (0x01050000u, 100), (0x0102FF00u, 80) })
    await Reject(() => CpuThermalWriteCoordinator.WriteAsync(id, value, fake.WriteAsync, fake.ReadAsync),
        "All CPU settings including mode-specific IDs are rejected while thermal ownership is active");
int userOcCalls = 0;
await Reject(() => CpuThermalWriteCoordinator.ExecuteUserCpuWriteAsync(() => { userOcCalls++; return Task.CompletedTask; }),
    "CPU overclocking cannot mutate hardware while thermal control owns CPU settings");
Check(fake.Writes.Count == writesBeforeRejectedUserChange && userOcCalls == 0 && !transport.IsSuperseded,
    "Rejected CPU mutations have no physical callbacks or ownership changes");
await transport.VerifyOwnershipAsync();
await transport.RestoreIfOwnedAsync();
Check(await transport.ReadAsync() == new CpuPowerEnvelope(80, 100), "All rejected mutations preserve an exactly restorable baseline");

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await transport.EnableAsync();
await transport.WriteAsync(new(40, 50));
fake.Values[CpuThermalWriteCoordinator.ShortPowerId] = 45;
var writesBeforeConflictingRestore = fake.Writes.Count;
await Reject(() => transport.RestoreIfOwnedAsync(), "An external coupled limit that conflicts with the owned baseline cannot report restoration success");
Check(fake.Writes.Count == writesBeforeConflictingRestore && fake.Values[CpuThermalWriteCoordinator.LongPowerId] == 40
    && fake.Values[CpuThermalWriteCoordinator.ShortPowerId] == 45 && fake.Values[CpuThermalWriteCoordinator.TemperatureId] == 88,
    "Conflicting restoration preserves external power and the verified temperature protection");
Check(transport.LastRestorationError is not null, "Conflicting coupled restoration latches its unverified state");
await Reject(() => transport.RestoreIfOwnedAsync(), "A second stop cannot erase a coupled restoration failure");

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await transport.EnableAsync();
await transport.WriteAsync(new(40, 50));
fake.Values[CpuThermalWriteCoordinator.ShortPowerId] = 120;
await transport.RestoreIfOwnedAsync();
Check(await transport.ReadAsync() == new CpuPowerEnvelope(80, 120), "Compatible external power changes are preserved while owned fields restore");

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await transport.EnableAsync();
await CpuThermalWriteCoordinator.WriteAsync(0x02020000, 999, fake.WriteAsync, fake.ReadAsync);
Check(fake.Values[0x02020000] == 999 && !transport.IsSuperseded, "GPU writes are unaffected by CPU thermal gate");
fake.Values[CpuThermalWriteCoordinator.TemperatureId] = 99;
await Reject(() => transport.VerifyOwnershipAsync(), "External temperature override detected without power change");
await Reject(async () => { await transport.ReadAsync(); }, "Executor readback checks firmware temperature even when power target is unchanged");
await transport.RestoreIfOwnedAsync();
Check(fake.Values[CpuThermalWriteCoordinator.TemperatureId] == 99, "Do not restore temperature changed externally");

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await transport.EnableAsync();
fake.Values[CpuThermalWriteCoordinator.LongPowerId] = 60;
var writesBeforeOwnershipLoss = fake.Writes.Count;
await Reject(() => transport.WriteAsync(new(50, 70)), "A target change cannot hide an external power override");
Check(fake.Writes.Count == writesBeforeOwnershipLoss && fake.Values[CpuThermalWriteCoordinator.LongPowerId] == 60,
    "Write ownership mismatch is rejected before physical mutation");
await transport.RestoreIfOwnedAsync();

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await transport.EnableAsync();
fake.Values[CpuThermalWriteCoordinator.TemperatureId] = 99;
writesBeforeOwnershipLoss = fake.Writes.Count;
await Reject(() => transport.WriteAsync(new(50, 70)), "A target change cannot hide an external temperature override");
Check(fake.Writes.Count == writesBeforeOwnershipLoss, "Temperature ownership is checked before power writes");
await transport.RestoreIfOwnedAsync();

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await transport.EnableAsync();
await transport.WriteAsync(new(40, 50));
fake.Mode = 3;
await Reject(() => transport.VerifyOwnershipAsync(), "Leaving custom mode suspends active control");
var writesBeforeModeRestore = fake.Writes.Count;
await transport.RestoreIfOwnedAsync();
Check(fake.Writes.Count == writesBeforeModeRestore, "No stale writes to another firmware mode");

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await transport.EnableAsync();
var blocked = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
var started = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
fake.BeforeWrite = async () => { started.TrySetResult(); await blocked.Task; };
var pendingWrite = transport.WriteAsync(new(40, 50));
await started.Task.WaitAsync(TimeSpan.FromSeconds(2));
var pendingRestore = transport.RestoreIfOwnedAsync();
await Task.Delay(30);
Check(!pendingRestore.IsCompleted && transport.IsEnabled, "Restoration retains ownership while physical I/O remains pending");
fake.BeforeWrite = null;
blocked.TrySetResult();
await pendingWrite;
await pendingRestore;
Check(await transport.ReadAsync() == new CpuPowerEnvelope(80, 100), "Late physical completion drains before baseline restoration");

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
await transport.EnableAsync();
await transport.WriteAsync(new(40, 50));
fake.IgnoreWrites = true;
await Reject(() => transport.RestoreIfOwnedAsync(), "Restoration readback failure");
Check(!transport.IsEnabled && transport.LastRestorationError is not null, "Failed restoration preserves an explicit unverified state after I/O ends");
await Reject(() => transport.RestoreIfOwnedAsync(), "Repeated stop cannot wash away failed restoration");
await Reject(() => transport.EnableAsync(), "Unverified restoration requires a fresh discovery");

// Firmware bounds observed by the elevated read-only probe on 2026-11-02.
fake = new FakeFirmware { Mode = 3 };
fake.Ranges.Clear();
fake.Ranges.AddRange(new[]
{
    new RangeCapability((CapabilityID)0x01020300, 120, 50, 135, 1),
    new RangeCapability(CapabilityID.CPULongTermPowerLimit, 70, 50, 135, 1),
    new RangeCapability((CapabilityID)0x01010300, 185, 60, 195, 1),
    new RangeCapability(CapabilityID.CPUShortTermPowerLimit, 125, 60, 195, 1),
    new RangeCapability((CapabilityID)0x01040300, 100, 85, 100, 1),
    new RangeCapability(CapabilityID.CPUTemperatureLimit, 100, 85, 100, 1)
});
fake.Values[CpuThermalWriteCoordinator.LongPowerId] = 70;
fake.Values[CpuThermalWriteCoordinator.ShortPowerId] = 125;
await Reject(async () => { await ThermalCpuTransport.DiscoverAsync(fake); }, "Observed Performance mode cannot silently become Custom");
fake.Mode = 255; // Simulated mode selection, never a physical mode switch.
transport = await ThermalCpuTransport.DiscoverAsync(fake);
Check(transport.Capability.MinLong == 50 && transport.Capability.MaxLong == 135
    && transport.Capability.MinShort == 60 && transport.Capability.MaxShort == 195
    && transport.TemperatureMinimum == 85, "Observed multi-mode capability IDs select the exact Custom rows");
await transport.EnableAsync(88);
await transport.WriteAsync(new(50, 60));
Check(await transport.ReadAsync() == new CpuPowerEnvelope(50, 60), "Observed firmware lower bounds support fail-closed CPU restriction");
await Reject(() => transport.SetTemperatureLimitAsync(84), "Observed minimum firmware temperature is 85");
await transport.RestoreIfOwnedAsync();

fake = new FakeFirmware();
transport = await ThermalCpuTransport.DiscoverAsync(fake);
var aggregate = await transport.ReadCpuTemperatureAsync();
Check(aggregate.Celsius == 55 && aggregate.IsFresh(Environment.TickCount64, 2000), "Firmware telemetry has its own real sample tick and aggregate reading");
fake.Values[0x05040000] = 0;
await Reject(async () => { await transport.ReadCpuTemperatureAsync(); }, "Invalid firmware aggregate fails closed");
Check(transport.LastTemperatureReadError is not null, "Firmware getter preserves its actionable failure for diagnostics");
fake.Values[0x05040000] = 55;
await transport.EnableAsync();
var getterEntered = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
var getterReleased = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
fake.BeforeRead = async id => { if (id == 0x05040000) { getterEntered.TrySetResult(); await getterReleased.Task; } };
var pendingAggregate = transport.ReadCpuTemperatureAsync();
await getterEntered.Task.WaitAsync(TimeSpan.FromSeconds(2));
Check(!(await transport.ReadCpuTemperatureAsync()).IsFresh(Environment.TickCount64, 2000), "A pending firmware getter does not admit a second physical getter");
await transport.WriteAsync(new(50, 70)).WaitAsync(TimeSpan.FromSeconds(2));
Check(await transport.ReadAsync() == new CpuPowerEnvelope(50, 70), "A pending aggregate getter does not own the CPU write gate");
getterReleased.TrySetResult();
await pendingAggregate;
await transport.RestoreIfOwnedAsync();

Console.WriteLine($"PASS: {scenarios} firmware transport and CPU ownership scenarios (fake firmware, no physical writes)");
await FaultMatrixTests.RunAsync();

sealed class FakeFirmware : IThermalCpuFirmware
{
    public ThermalMachineIdentity Identity = new("LENOVO", "83NN", "S9CN15WW", "Intel(R) Core(TM) i9-14900HX");
    public int Mode = 255;
    public bool OtherController;
    public bool IgnoreWrites;
    public Func<Task>? BeforeWrite;
    public Func<uint, Task>? BeforeRead;
    public readonly List<(uint Id, int Value)> Writes = [];
    public readonly Dictionary<uint, int> Values = new()
    { [CpuThermalWriteCoordinator.LongPowerId] = 80, [CpuThermalWriteCoordinator.ShortPowerId] = 100, [CpuThermalWriteCoordinator.TemperatureId] = 100, [0x05040000] = 55 };
    public readonly List<RangeCapability> Ranges =
    [new(CapabilityID.CPULongTermPowerLimit, 80, 10, 150, 1), new(CapabilityID.CPUShortTermPowerLimit, 100, 10, 190, 1), new(CapabilityID.CPUTemperatureLimit, 100, 80, 100, 1)];
    public readonly List<DiscreteCapability> Discrete = [];
    public Task<ThermalMachineIdentity> GetIdentityAsync() => Task.FromResult(Identity);
    public Task<IReadOnlyList<RangeCapability>> GetCapabilitiesAsync() => Task.FromResult<IReadOnlyList<RangeCapability>>(Ranges);
    public Task<IReadOnlyList<DiscreteCapability>> GetDiscreteCapabilitiesAsync() => Task.FromResult<IReadOnlyList<DiscreteCapability>>(Discrete);
    public Task<int> GetModeAsync() => Task.FromResult(Mode);
    public async Task<int> ReadAsync(uint id) { if (BeforeRead is { } wait) await wait(id); return Values[id]; }
    public async Task WriteAsync(uint id, int value)
    {
        if (BeforeWrite is { } wait) await wait();
        Writes.Add((id, value));
        if (!IgnoreWrites) Values[id] = value;
        if (Values[CpuThermalWriteCoordinator.LongPowerId] > Values[CpuThermalWriteCoordinator.ShortPowerId])
            throw new InvalidOperationException("Invalid transient PL1 > PL2");
    }
    public void EnsureExclusiveProcess() { if (OtherController) throw new InvalidOperationException("Another firmware controller is running."); }
}
