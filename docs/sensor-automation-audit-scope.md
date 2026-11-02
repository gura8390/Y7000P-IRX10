# 传感器与自动化审计覆盖范围

日期：2026-11-02。此清单记录本次接手中传感器/自动化子任务的实际阅读范围；其他子任务的覆盖情况以总交付记录为准。

## 已完整阅读并审查

- `LenovoLegionToolkit.Lib/Controllers/Sensors/SensorsGroupController.cs`：初始化、硬件更新、订阅、通知、重置、影子循环和释放；原交付版本与本轮修改均已审阅。
- `LenovoLegionToolkit.Lib/Controllers/Sensors/SensorUpdateScheduler.cs`：独立截止时间、到期域合并、错过周期跳过、参数边界。
- `LenovoLegionToolkit.Lib/Controllers/Sensors/HardwareSensorSnapshot.cs`：指标所有权、逐指标时间、复制隔离与批量更新。
- `LenovoLegionToolkit.Lib/Controllers/Sensors/Providers/CpuSensorProvider.cs`。
- `LenovoLegionToolkit.Lib/Controllers/Sensors/Providers/GpuSensorProvider.cs`。
- `LenovoLegionToolkit.Lib/Controllers/Sensors/Providers/MemorySensorProvider.cs`。
- `LenovoLegionToolkit.Lib/Controllers/Sensors/Providers/StorageSensorProvider.cs`。
- `LenovoLegionToolkit.Lib/Controllers/Sensors/Providers/ISensorProvider.cs`。
- `LenovoLegionToolkit.Lib/Controllers/Sensors/Providers/PowerMonitor.cs`。
- `LenovoLegionToolkit.Lib/Extensions/SensorExtensions.cs`。
- `LenovoLegionToolkit.Lib.Automation/Pipeline/AutomationPipeline.cs`：执行与展开、直接/间接引用环、展开前验证、普通共享子图、过深/过大的引用图。
- `LenovoLegionToolkit.Lib.Automation/Steps/IAutomationStep.cs` 与 `QuickActionAutomationStep.cs`。
- `tests/Regression.Tests/Program.cs` 与项目文件：引用真实生产库；仅以假传感器与计数步骤替代外部硬件行为。
- `LenovoLegionToolkit.Lib/Controllers/CpuThermalController.cs`：启停串行、启动取消、挂起 I/O、重复停止、恢复失败锁存、状态通知。
- `tests/ThermalLifecycle.Tests`：链接真实控制器和 Thermal 核心源文件，以无硬件 host 替身验证生命周期。

## 仅按调用链定点阅读

- `LenovoLegionToolkit.Lib.Automation/AutomationProcessor.cs`：执行入口、传入的其他流水线列表及异常处理。
- `LenovoLegionToolkit.WPF/Controls/Automation/AutomationPipelineControl.cs`：验证警告调用点。
- `LenovoLegionToolkit.WPF/App.xaml.cs`：传感器异步初始化、影子模式启用与退出释放调用点。
- `LenovoLegionToolkit.Lib/Controllers/GPUController.cs`：GPU 状态读取的锁与异步调用。
- `SensorsController.cs`、`AbstractSensorsController.cs`：能力/风扇数据路径与现有缓存；没有对其全部原生接口进行逐项审计。
- `Enums.cs`：真实的完整 32 项 `SensorItem` 定义。
- `ThermalCpuTransport.cs`、`CpuThermalWriteCoordinator.cs`：按生命周期故障链阅读发现/启用、读回、共享物理所有权与恢复释放路径；完整适配器审计由对应子任务另记。
- 相关 `.csproj`、`Directory.Build.props`、`SensorExtensions`、日志与目录辅助方法：编译依赖与回归程序的运行副作用。

## 本子任务未覆盖

未逐行审计整个 LLT 仓库。键盘灯效、Spectrum、宏、显示、网络、所有 WMI 封装、CLI、安装器、更新器、完整 WPF 页面与第三方依赖内部实现不在本子任务的完整覆盖范围内。主动控温适配器、功率仲裁/执行器的覆盖由对应审计子任务单独记录。

自动回归可证明确定性调度、复制与纯执行图语义，也可验证真实 provider 对假传感器的读取。它们不能替代 Lenovo 本机传感器、BIOS/WMI、睡眠恢复、长期采样负载、外部软件并发和 GUI 退出验收。微基准报告的分配量与耗时仅代表内存快照更新；模拟调度计数不等于实机 I/O 性能。

编译/运行结果由最终验证日志给出；阅读清单本身不表示构建已经成功，更不表示全仓库不存在缺陷。

生命周期独立回归已使用本机 .NET 8 的 Release 构建执行通过 24 项检查，包含无需硬件监视驱动的明确固件单源路径、后台温度读取挂起时的单次在途限制、过期温度触发最低合法功耗与温度读取不阻塞停止恢复。它验证真实控制器和核心代码对模拟宿主行为的反应，不验证真实 WMI 及 BIOS 的行为。
