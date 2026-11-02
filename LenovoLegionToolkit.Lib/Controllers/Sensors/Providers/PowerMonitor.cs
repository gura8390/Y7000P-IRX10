using System;

namespace LenovoLegionToolkit.Lib.Controllers.Sensors.Providers;

public class PowerMonitor
{
    private const float MAX_VALID = 400f;
    private const float MIN_VALID = 0f;

    public event Action? ResetNeeded;

    public float Read(float raw)
    {
        if (!float.IsFinite(raw)) return -1;
        if (raw > MAX_VALID)
        {
            ResetNeeded?.Invoke();
            return -1;
        }

        if (raw <= MIN_VALID)
        {
            return -1;
        }

        // A stable power limit can legitimately produce identical readings indefinitely.
        // Equality alone is not evidence that the sensor or its driver is stuck.
        return raw;
    }

    public void Reset()
    {
        // No cached equality-based state: freshness belongs to the sampling controller.
    }
}
