namespace LenovoLegionToolkit.Lib;
public enum SensorItem { CpuTemperature, CpuPower, MemoryUtilization }
[System.Flags]
public enum HardwareUpdateScope
{
    None = 0, Cpu = 1, Gpu = 2, Memory = 4, Fans = 8, Storage = 16,
    AllNonStorage = Cpu | Gpu | Memory | Fans, All = AllNonStorage | Storage
}
