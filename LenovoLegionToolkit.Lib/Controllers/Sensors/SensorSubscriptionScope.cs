using System.Collections.Generic;
using LenovoLegionToolkit.Lib.Controllers.Sensors.Providers;

namespace LenovoLegionToolkit.Lib.Controllers.Sensors;

public static class SensorSubscriptionScope
{
    // A dashboard item can depend on hardware data or on the firmware/battery refresh
    // driven by SensorsUpdated. Display groups are dependencies, never snapshot owners.
    public static HardwareUpdateScope FromSensorItems(IEnumerable<SensorItem> items, IEnumerable<ISensorProvider> providers)
    {
        var selected = items as HashSet<SensorItem> ?? new HashSet<SensorItem>(items);
        var scope = HardwareUpdateScope.None;
        foreach (var provider in providers)
            if (selected.Overlaps(provider.ProvidedSensorItems)) scope |= provider.Scope;

        foreach (var item in selected)
            scope |= item switch
            {
                SensorItem.GpuTemperatures => HardwareUpdateScope.Gpu,
                SensorItem.CpuFanSpeed or SensorItem.GpuFanSpeed or SensorItem.PchFanSpeed
                    or SensorItem.PchTemperature or SensorItem.BatteryState or SensorItem.BatteryLevel => HardwareUpdateScope.Fans,
                _ => HardwareUpdateScope.None
            };
        return scope;
    }
}
