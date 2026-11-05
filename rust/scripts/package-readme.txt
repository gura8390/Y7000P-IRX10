拯救者工具箱 0.1.0 · Y7000P IRX10 / 83NN

将压缩包完整解压，运行「拯救者工具箱.exe」。发行构建会请求 Windows 管理员权限。
界面、托盘和设置沿用 2.34.0.3 的页面层级。点击右上角可查看设备、日志或切换主题。
硬件状态由本机实际读取。充电阈值需要原版支持的 Lenovo PWRMGRV 配置和 IBMPmDrv 接口。
使用硬件控制前，请先退出其他正在控制同一设备的程序。温控和传感器按应用中的设置启用。

如只查看状态，可以用 --read-only 启动。--data-dir 可指定独立的数据目录。
应用使用 Windows 自带的 Microsoft YaHei UI 或 Segoe UI。

软件以 GPL-3.0-only 许可发行。LICENSE 和 THIRD_PARTY_NOTICES.md 包含许可与来源说明。
licenses/ 目录包含随应用提供的 PawnIO 模块对应源码、原构建流程、MPL/LGPL 许可及原作者声明。
DEPENDENCY_LICENSES.txt 记录此构建锁定的依赖许可。
package-manifest.json 记录对应源码提交和包内文件的 SHA256。

项目源码与原版归属见 Lenovo Legion Toolkit 源码及本项目仓库。
https://github.com/gura8390/Y7000P-IRX10
