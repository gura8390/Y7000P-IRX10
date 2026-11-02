#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use anyhow::Result;
mod application;
mod benchmark_fixture;
mod ui;
mod ui_model;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        let args = std::env::args().skip(1).collect::<Vec<_>>();
        let noninteractive = args.iter().any(|s| {
            [
                "broker",
                "doctor",
                "cli",
                "ui-smoke",
                "ui-layout",
                "ui-benchmark",
                "--quit-after-ms",
            ]
            .contains(&s.as_str())
        });
        if !noninteractive {
            let text = format!("机控未能完成启动或退出：{error:#}");
            let title = "机控";
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                    std::ptr::null_mut(),
                    text.encode_utf16()
                        .chain(Some(0))
                        .collect::<Vec<_>>()
                        .as_ptr(),
                    title
                        .encode_utf16()
                        .chain(Some(0))
                        .collect::<Vec<_>>()
                        .as_ptr(),
                    windows_sys::Win32::UI::WindowsAndMessaging::MB_OK
                        | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                );
            }
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(jiks_benchmark_fixture)]
    {
        anyhow::ensure!(
            arguments.first().map(String::as_str) == Some("ui-benchmark"),
            "此优化asInvoker程序仅为隔离UI基准fixture，禁止启动正常应用、doctor或broker"
        );
    }
    match arguments.as_slice() {
        [command, rest @ ..] if command == "cli" => {
            unsafe {
                windows_sys::Win32::System::Console::AttachConsole(
                    windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
                );
            }
            let local = std::path::PathBuf::from(
                std::env::var_os("LOCALAPPDATA")
                    .ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA缺失"))?,
            )
            .join("Jiks");
            let (root, rest) = match rest {
                [flag, path, tail @ ..] if flag == "--data-dir" => {
                    (std::path::PathBuf::from(path), tail)
                }
                _ => (local, rest),
            };
            let request = match rest {
                [kind] if kind == "status" => jiks_runtime::cli::Request::Status,
                [kind, name, payload] if kind == "execute" => jiks_runtime::cli::Request::Execute {
                    command: name.clone(),
                    payload: serde_json::from_str(payload)?,
                },
                _ => {
                    anyhow::bail!("用法：Jiks cli [--data-dir 目录] status | execute 命令 JSON对象")
                }
            };
            let response = jiks_runtime::cli::request(&root, request)?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            anyhow::ensure!(response.ok, "{}", response.message);
        }
        [command, rest @ ..] if command == "ui-benchmark" => benchmark_fixture::run(rest)?,
        [command] if command == "ui-smoke" => smoke_ui(12, true)?,
        [command] if command == "ui-layout" => smoke_ui(120, false)?,
        [command] if command == "doctor" => {
            let identity = jiks_platform::read_machine_identity()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "application": "机控 / Jiks", "version": env!("CARGO_PKG_VERSION"),
                    "read_only": true, "supported": identity.is_supported(), "identity": identity
                }))?
            );
        }
        [command, rest @ ..] if command == "broker" => {
            let options = application::Options::parse(rest)?;
            jiks_runtime::ipc::broker(options.data_root, options.read_only)?;
        }
        _ => application::run(application::Options::parse(&arguments)?)?,
    }
    Ok(())
}

fn smoke_ui(seconds: u64, exercise_reentrancy: bool) -> Result<()> {
    use std::sync::{Arc, Mutex, mpsc};
    use ui_model::{UiAction, UiBridge, UiModel};
    let model = Arc::new(Mutex::new(UiModel::default()));
    model.lock().unwrap().notice = "界面检查：不操作设备或保存设置".into();
    model
        .lock()
        .unwrap()
        .set_capability("device.details", true, "");
    if !exercise_reentrancy {
        use serde_json::json;
        use ui_model::{UiChoice, UiDocument, UiItem, UiMetric};
        let mut state = model.lock().unwrap();
        state.notice = "隔离布局检查 · 确定性假遥测；没有设备读取或设置保存".into();
        for command in [
            "app.preferences.save",
            "sensors.settings.save",
            "settings.fields.save",
            "osd.apply",
            "device.information.refresh",
        ] {
            state.set_capability(command, true, "");
        }
        state.set_capability("thermal.apply", false, "隔离界面fixture不运行温控");
        for (key, value) in [
            ("power.mode", json!(3)),
            ("thermal.enabled", json!(false)),
            ("thermal.shadow", json!(false)),
            ("thermal.sampling_ms", json!(250)),
            ("thermal.firmware_limit_c", json!(88)),
            ("sensors.temperature_unit", json!("C")),
            ("sensors.cpu_frequency", json!("average")),
            ("sensors.temperature_source", json!("core_max")),
            ("sensors.voltage_mode", json!("average")),
            ("sensors.voltage_core", json!(0)),
            ("sensors.gpu", json!("discrete")),
            ("sensors.memory_mode", json!("gb")),
            ("osd.visible", json!(true)),
            ("osd.enabled", json!(true)),
        ] {
            state.values.insert(key.into(), value);
        }
        state.options.insert(
            "power.mode".into(),
            vec![
                UiChoice::new("安静模式", 1),
                UiChoice::new("均衡模式", 2),
                UiChoice::new("野兽模式", 3),
            ],
        );
        for document in [
            "application",
            "hardware_sensors",
            "dashboard",
            "sensor_dashboard",
            "notifications",
            "osd",
        ] {
            state.documents.insert(
                document.into(),
                UiDocument {
                    title: document.into(),
                    value: json!({}),
                    writable: true,
                    reason: String::new(),
                },
            );
        }
        state.metrics = [
            ("CpuTemperature", "CPU 温度", "65.0 °C"),
            ("CpuFrequency", "CPU 频率", "3200 MHz"),
            ("GpuCoreTemperature", "GPU 核心温度", "55.0 °C"),
            ("GpuFrequency", "GPU 频率", "300 MHz"),
            ("RamUsage", "内存已用", "8.00 GB"),
            ("Fps", "提交FPS", "60.0"),
            ("FrameTime", "提交间隔", "16.67 ms"),
        ]
        .into_iter()
        .map(|(key, label, value)| UiMetric {
            key: key.into(),
            label: label.into(),
            value: value.into(),
            detail: "隔离固定样本，非本机采样".into(),
        })
        .collect();
        let groups=["处理器","显卡","内存","磁盘","显示器","主板","网络","操作系统","电池","固件","机型"].into_iter().map(|title|json!({"title":title,"rows":[{"label":"信息","value":"确定性界面样本，非实机查询"}]})).collect::<Vec<_>>();
        state.collections.insert(
            "device.information".into(),
            vec![UiItem {
                id: "layout-fixture".into(),
                label: "设备信息".into(),
                detail: "仅布局检查".into(),
                value: json!({"groups":groups}),
            }],
        );
        state.touch_metadata();
    }
    let (send, receive) = mpsc::sync_channel::<UiAction>(16);
    let consumer_model = Arc::clone(&model);
    let consumer = std::thread::spawn(move || {
        while let Ok(action) = receive.recv() {
            let mut model = consumer_model.lock().unwrap_or_else(|e| e.into_inner());
            let request_id = action.request_id();
            if matches!(action, UiAction::Shutdown { .. }) {
                model.shutdown_ready = true;
            }
            model.finish(request_id, true, "界面检查完成；没有执行硬件动作或保存设置");
        }
    });
    let mut bridge =
        UiBridge::new(model, send).with_smoke_close_after(std::time::Duration::from_secs(seconds));
    bridge.smoke_exercise_reentrancy = exercise_reentrancy;
    let result = ui::run(bridge);
    let _ = consumer.join();
    result
}
