# Y7000P IRX10 · Lenovo Legion Toolkit fork

面向 **Lenovo Y7000P IRX10 / 83NN、i9-14900HX、BIOS S9CN15WW** 的个人维护分支。当前 `main` 和 **v2.34.0.3** 保存 C# / WPF 版本；Rust 重构将独立保存在 `rust-rewrite` 分支，完成同功能验证后再比较性能与内存。

基于 [LenovoLegionToolkit-Team/LenovoLegionToolkit](https://github.com/LenovoLegionToolkit-Team/LenovoLegionToolkit) 提交 `0bb045be29c4fd772ba9887a3507b6a9b0b56d58`。感谢 Bartosz Cichecki、LenovoLegionToolkit-Team、Kaguya、Dr. Skinner 和所有上游贡献者。本分支不是联想官方软件或上游官方发行版。保留 [GPL-3.0 许可证](LICENSE)、上游版权和[原版说明](docs/UPSTREAM_README.md)。

## 下载与启动

从 [本仓库 Releases](https://github.com/gura8390/Y7000P-IRX10/releases) 下载 `Y7000P-IRX10-CSharp-2.34.0.3-win-x64.zip`，核对同页 SHA-256 后解压，运行 `Lenovo Legion Toolkit.exe`。框架依赖便携包需要 [Microsoft .NET 9 Desktop Runtime x64](https://dotnet.microsoft.com/download/dotnet/9.0)，不包含安装器、验收负载工具、个人日志或用户配置。

公共二进制由同一冻结业务源码独立构建，禁用自有程序集调试信息并映射源码路径，与此前本地 R3 测试包哈希不同；原 R3 和本机安装未替换。第三方程序集保持 NuGet 原样，许可及源码链接见包内 `THIRD-PARTY-NOTICES.md` / `licenses/`。源码包从本发布提交的 `git archive` 生成。

内置上游更新来源保持原业务代码的设置；本分支更新请以本仓库 Release 为准，不要把上游提示当成本分支更新承诺。

## 2.34.0.3 改动

- CPU 独立采样、不可变批量快照、按订阅周期调度，避免硬盘跟随 CPU 频繁轮询。
- 可选 CPU 热策略：85°C 预控、88°C 软件目标；只在固件合法范围和已验证用户预算内收紧，默认关闭。GPU 调度保留原逻辑。
- 命令等待有截止时间；超时不提前释放在途物理操作；停止验证恢复，外部改动不被强行覆盖。
- 自动化循环执行前校验，启动批次等待后代；失败传播并阻止温控自动启动。
- 同进程原子保存、旧文件和失败快照备份；未知 JSON 字段、区分大小写的未知键和原始数字词法保留。
- 修复传感器索引、订阅生命周期、内存显示和三处主题资源引用。

## 温控入口与验收边界

入口：**设置 → 应用行为 → 向下滚动到「硬件传感器」→ 点击整行文字**，打开传感器设置窗口。右侧切换按钮只控制硬件传感器开关，不是打开设置窗口。CPU 温控默认关闭。

修改 CPU 功耗预算前先停止温控并确认恢复；重新启用时读取新基线。本分支软件合同仅在上述目标机器核对，不保证其他型号或 BIOS 兼容。

**温度和满载验收由使用者自行进行。** 软件测试、离屏 UI 和简单启动检查不能证明温度达标，也不保证 CPU 永不超过 90°C。本次公开发布不运行压力负载、切模式或启用温控写入。

冻结源码门禁：Regression470、Startup13、Thermal5319、Firmware67+345、Lifecycle619、离屏WPF765（两主题各80资源键）。数量包含重复断言、模型和替身，不等于独立实机场景。公共构建复验另见 [公开验证摘要](docs/public-validation.md)。

原子保存限于同进程同规范路径当前目标，不承诺跨进程、两文件联合事务或断电恢复；异常注入不等于断电测试。见 [架构](docs/thermal-architecture.md) 与 [公开交接](docs/phase2-handoff.txt)。

## 构建

Windows x64、.NET SDK 9：

```powershell
dotnet restore LenovoLegionToolkit.WPF/LenovoLegionToolkit.WPF.csproj --locked-mode
dotnet publish LenovoLegionToolkit.WPF/LenovoLegionToolkit.WPF.csproj -c Release -p:Platform=x64 -p:DebugType=None -p:DebugSymbols=false -p:PathMap="$PWD=/_/src" -o artifacts/app
```

精确依赖保存在 `packages.lock.json`。CI 只构建和运行隔离测试，不需要上游签名密钥，不创建或覆盖 Release。旧上游发布和自动关闭 issue 工作流以 `.disabled` 留档。