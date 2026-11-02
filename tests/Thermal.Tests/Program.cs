using System;
using System.Linq;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.Thermal;

int checks = 0;
void Check(bool value, string name) { if (!value) throw new Exception(name); checks++; }
var policy = new ThermalPolicy();
ThermalDecision Eval(double package, double max, long now, ThermalDecision? previous = null) =>
    policy.Evaluate(new(package, now), new(max, now), now, previous);
var hot = Eval(80, 90, 0);
Check(hot.Phase == ThermalPhase.Hot && hot.PowerFraction < 1, "Core hotspot triggers");
var hold = Eval(89, 89, 1000, hot);
Check(hold.PowerFraction == hot.PowerFraction, "No recovery in hysteresis band");
var cool = Eval(80, 80, 2000, hold);
Check(Eval(80, 80, 21000, cool).PowerFraction == cool.PowerFraction, "Recovery dwell");
Check(Eval(80, 80, 22000, cool).PowerFraction > cool.PowerFraction, "Slow recovery");
var fault = policy.Evaluate(new(80, 0), new(80, 0), 5000, hot);
Check(fault.Phase == ThermalPhase.SensorFault && fault.PowerFraction <= hot.PowerFraction, "Stale telemetry");
foreach (var bad in new[] { double.NaN, double.PositiveInfinity, -1, 0d })
 Check(Eval(bad, 80, 6000, fault).Phase == ThermalPhase.SensorFault, "Invalid reading");
var state = hot;
for (int i=1;i<500;i++) {
 state = Eval(100,100,i*2000,state);
 Check(state.PowerFraction >= .35 && state.PowerFraction <= 1, "Saturation");
}
Check(policy.Evaluate(new(80,0),new(80,0),0,state).Phase == ThermalPhase.SensorFault, "Clock regression");
var cap = new CpuPowerCapability(10,100,10,150,1);
var cmd = PowerConstraintArbiter.Resolve(new(80,100),new[]{new PowerConstraint(new(50,70),1,1,9000)},cap,0,1,1,10000);
Check(cmd.Target == new CpuPowerEnvelope(50,70) && cmd.ExpiresAt == 9000, "Strictest budget and deadline");
try { PowerConstraintArbiter.Resolve(new(80,100),new[]{new PowerConstraint(new(50,70),0,1,9000)},cap,0,1,1,10000); throw new Exception("Accepted old epoch"); }
catch (InvalidOperationException) { checks++; }
long clock=0;
var transport = new FakeTransport();
var executor = new HardwareCommandExecutor(transport,cap,()=>clock,()=>(1,1));
var first = await executor.ApplyAsync(cmd,TimeSpan.FromMilliseconds(20));
Check(first.Status == ApplyStatus.UnknownCompletion, "Timeout reports unknown");
var secondTask = executor.ApplyAsync(cmd,TimeSpan.FromSeconds(1));
await Task.Delay(30);
Check(transport.Writes == 1, "Timed-out physical I/O retains ownership");
clock=10000;
transport.Release.TrySetResult();
var second = await secondTask;
Check(second.Status == ApplyStatus.Rejected && transport.Writes == 1, "Queued expired request rejected");
var snapshot = new LenovoLegionToolkit.Lib.HardwareSensorSnapshot()
    .WithSample(LenovoLegionToolkit.Lib.SensorItem.CpuTemperature, 85, 100);
var partial = snapshot.WithSample(LenovoLegionToolkit.Lib.SensorItem.MemoryUtilization, 50, 500);
Check(partial.GetSampleTick(LenovoLegionToolkit.Lib.SensorItem.CpuTemperature) == 100, "Partial update preserves CPU age");
Check(snapshot[LenovoLegionToolkit.Lib.SensorItem.MemoryUtilization] == -1, "Snapshot copy is immutable");
Check(partial.GetSampleTick(LenovoLegionToolkit.Lib.SensorItem.MemoryUtilization) == 500, "Updated metric has own timestamp");
checks += await ThermalRegressionTests.RunAsync();
checks += SchedulingRegressionTests.Run();
checks += await ThermalModelTests.RunAsync();
Console.WriteLine($"PASS: {checks} policy, saturation, arbitration, scheduling and I/O ownership checks");
sealed class FakeTransport : ICpuPowerTransport {
 public readonly TaskCompletionSource Release = new(TaskCreationOptions.RunContinuationsAsynchronously);
 public int Writes; private CpuPowerEnvelope? value;
 public async Task WriteAsync(CpuPowerEnvelope target) { Writes++; await Release.Task; value=target; }
 public Task<CpuPowerEnvelope> ReadAsync() => Task.FromResult(value!);
}
