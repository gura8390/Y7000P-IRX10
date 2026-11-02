// Only the Windows host types outside the linked production transport are represented here.
// Every test injects FakeFirmware; these unsupported host calls cannot issue hardware I/O.
using System;
using System.Collections.Generic;
using System.Management;
using System.Threading.Tasks;

namespace LenovoLegionToolkit.Lib
{
    public enum CapabilityID { CPUShortTermPowerLimit = 0x0101FF00, CPULongTermPowerLimit = 0x0102FF00, CPUTemperatureLimit = 0x0104FF00, CpuCurrentTemperature = 0x05040000 }
    public readonly record struct RangeCapability(CapabilityID Id, int DefaultValue, int Min, int Max, int Step);
    public readonly record struct DiscreteCapability(CapabilityID Id, int Value);
    public readonly record struct HardwareId(string Vendor, string Device) { public static readonly HardwareId Empty = new("", ""); }
}

namespace LenovoLegionToolkit.Lib.System.Management
{
    public static partial class WMI
    {
        public static class LenovoDefaultValueInDifferentModeData { }
        public static class LenovoCapabilityData01
        { public static Task<IEnumerable<RangeCapability>> ReadAsync() => throw new NotSupportedException("Inject FakeFirmware in tests."); }
        public static class LenovoDiscreteData
        { public static Task<IEnumerable<DiscreteCapability>> ReadAsync() => throw new NotSupportedException("Inject FakeFirmware in tests."); }
        public static class LenovoGameZoneData
        { public static Task<int> GetSmartFanModeAsync() => throw new NotSupportedException("Inject FakeFirmware in tests."); }
        private static Task CallAsync(string scope, FormattableString query, string method, Dictionary<string, object> parameters) =>
            throw new NotSupportedException("Inject FakeFirmware in tests.");
        private static Task<T> CallAsync<T>(string scope, FormattableString query, string method,
            Dictionary<string, object> parameters, Func<PropertyDataCollection, T> converter) =>
            throw new NotSupportedException("Inject FakeFirmware in tests.");
    }
}
