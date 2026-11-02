//! Explicit, hardware-free comparison fixture. It never creates a Runtime,
//! platform worker, config repository, recorder or production application.
use crate::ui_model::{UiBridge, UiModel};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::Threading::GetCurrentProcessId,
    UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
};

pub(crate) fn run(args: &[String]) -> Result<()> {
    ensure!(cfg!(jiks_benchmark_fixture)&&!cfg!(debug_assertions),"Use the explicitly isolated optimized benchmark build");
    if args.first().map(String::as_str) == Some("--broker") {
        let duration = args
            .get(1)
            .context("fixture broker duration missing")?
            .parse::<u64>()?;
        ensure!(
            (1000..=120000).contains(&duration),
            "fixture duration out of bounds"
        );
        let mut signal = String::new();
        std::io::stdin().read_line(&mut signal)?;
        ensure!(
            signal.trim() == "start",
            "fixture broker requires the UI-ready handshake"
        );
        let start = Instant::now();
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let mut tick = 0u64;
        while start.elapsed().as_millis() < (duration as u128) {
            writeln!(
                out,
                "{}",
                json!({"tick":tick,"text":format!("确定性状态 {:04} · 250 ms",tick)})
            )?;
            out.flush()?;
            tick += 1;
            std::thread::sleep(Duration::from_millis(250));
        }
        return Ok(());
    }
    let mut output = None;
    let mut hidden = false;
    let mut duration = 15000u64;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--output" => {
                index += 1;
                output = Some(std::path::PathBuf::from(
                    args.get(index).context("missing output")?,
                ));
            }
            "--hidden" => hidden = true,
            "--duration-ms" => {
                index += 1;
                duration = args.get(index).context("missing duration")?.parse()?;
            }
            other => bail!("unknown isolated benchmark argument {other}"),
        };
        index += 1;
    }
    ensure!(
        (1000..=120000).contains(&duration),
        "fixture duration out of bounds"
    );
    let output = output.context("isolated benchmark requires explicit --output")?;
    ensure!(!output.exists(), "fixture result already exists");
    let started = Instant::now();
    let model = Arc::new(Mutex::new(UiModel::default()));
    {
        let mut state = model.lock().unwrap();
        state.notice = "隔离界面基准：不读取设备，不保存设置".into();
        for (key, value) in [
            ("thermal.enabled", json!(false)),
            ("thermal.shadow", json!(false)),
            ("sensors.cpu_frequency", json!("average")),
            ("sensors.temperature_source", json!("auto")),
            ("sensors.voltage_mode", json!("average")),
            ("sensors.voltage_core", json!(0)),
            ("sensors.gpu", json!("discrete")),
            ("sensors.memory_mode", json!("gigabytes")),
            ("sensors.temperature_unit", json!("C")),
            ("thermal.sampling_ms", json!(250)),
            ("thermal.firmware_limit_c", json!(88)),
        ] {
            state.values.insert(key.into(), value);
        }
        state.revision += 1;
    }
    let (sender, receiver) = mpsc::sync_channel(8);
    let consumer = std::thread::spawn(move || while receiver.recv().is_ok() {});
    let bridge = UiBridge::new(model, sender);
    let mut child = Command::new(std::env::current_exe()?)
        .args(["ui-benchmark", "--broker", &duration.to_string()])
        .creation_flags(0x08000000)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let child_id = child.id();
    let stdout = child.stdout.take().context("fixture broker stdout")?;
    let mut child_input = child
        .stdin
        .take()
        .context("fixture broker handshake pipe")?;
    let report = Arc::new(Mutex::new(
        json!({"implementation":"Rust native","scope":"actual SensorSettings editor only","optimized_release":!cfg!(debug_assertions),"fixture_only_build":cfg!(jiks_benchmark_fixture),"hidden":hidden,"broker_pid":child_id,"hardware_calls":0,"configuration_writes":0,"tick_interval_ms":250,"responses_ms":[],"ticks":0}),
    ));
    let worker_report = report.clone();
    let worker = std::thread::spawn(move || -> Result<()> {
        unsafe {
            let mut hwnd: HWND = std::ptr::null_mut();
            for _ in 0..1000 {
                EnumWindows(Some(find_window), (&mut hwnd as *mut HWND) as isize);
                if !hwnd.is_null() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            ensure!(!hwnd.is_null(), "fixture editor HWND was not created");
            let _close = CloseFixture(hwnd);
            let window_ms = started.elapsed().as_secs_f64() * 1000.;
            println!("BENCH_WINDOW");
            let dpi = GetDpiForWindow(hwnd);
            let mut window: RECT = std::mem::zeroed();
            let mut client: RECT = std::mem::zeroed();
            ensure!(
                GetWindowRect(hwnd, &mut window) != 0 && GetClientRect(hwnd, &mut client) != 0,
                "fixture viewport measurement failed"
            );
            let outer_width = 900 + (window.right - window.left) - (client.right - client.left);
            let outer_height = 900 + (window.bottom - window.top) - (client.bottom - client.top);
            ensure!(
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    40,
                    40,
                    outer_width,
                    outer_height,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                ) != 0,
                "fixture viewport resize failed"
            );
            ensure!(
                GetClientRect(hwnd, &mut client) != 0
                    && client.right - client.left == 900
                    && client.bottom - client.top == 900,
                "fixture actual client viewport is not 900x900"
            );
            if hidden {
                ShowWindow(hwnd, SW_HIDE);
            }
            worker_report.lock().unwrap()["window_created_ms"] = json!(window_ms);
            worker_report.lock().unwrap()["dpi"] = json!(dpi);
            worker_report.lock().unwrap()["actual_client_pixels"] =
                json!([client.right - client.left, client.bottom - client.top]);
            worker_report.lock().unwrap()["client_size_gate"] = json!(true);
            let mut status: HWND = std::ptr::null_mut();
            EnumChildWindows(hwnd, Some(find_status), (&mut status as *mut HWND) as isize);
            ensure!(
                !status.is_null(),
                "fixture could not locate actual thermal status label"
            );
            let mut logical_font: LOGFONTW = std::mem::zeroed();
            let handle = SendMessageW(status, WM_GETFONT, 0, 0) as HGDIOBJ;
            ensure!(
                GetObjectW(
                    handle,
                    std::mem::size_of::<LOGFONTW>() as i32,
                    (&mut logical_font as *mut LOGFONTW).cast()
                ) != 0,
                "fixture font metadata unavailable"
            );
            let family_end = logical_font
                .lfFaceName
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(logical_font.lfFaceName.len());
            worker_report.lock().unwrap()["status_font_height_px"] =
                json!(logical_font.lfHeight.abs());
            worker_report.lock().unwrap()["status_font_family"] = json!(String::from_utf16_lossy(
                &logical_font.lfFaceName[..family_end]
            ));
            child_input.write_all(b"start\n")?;
            child_input.flush()?;
            drop(child_input);
            for line in BufReader::new(stdout).lines() {
                ensure!(
                    GetClientRect(hwnd, &mut client) != 0
                        && client.right - client.left == 900
                        && client.bottom - client.top == 900,
                    "fixture client viewport changed after readiness"
                );
                let packet: Value = serde_json::from_str(&line?)?;
                let text = packet["text"].as_str().context("fixture status text")?;
                let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
                let probe = Instant::now();
                let mut result = 0usize;
                let acknowledged = SendMessageTimeoutW(
                    hwnd,
                    WM_SETTEXT,
                    0,
                    wide.as_ptr() as isize,
                    SMTO_ABORTIFHUNG | SMTO_BLOCK,
                    2000,
                    &mut result,
                ) != 0;
                ensure!(acknowledged, "fixture UI message acknowledgement timed out");
                let elapsed = probe.elapsed().as_secs_f64() * 1000.;
                ensure!(
                    SendMessageTimeoutW(
                        status,
                        WM_SETTEXT,
                        0,
                        wide.as_ptr() as isize,
                        SMTO_ABORTIFHUNG | SMTO_BLOCK,
                        2000,
                        &mut result
                    ) != 0,
                    "fixture status update timed out"
                );
                let mut report = worker_report.lock().unwrap();
                if report.get("backend_ready_ms").is_none() {
                    report["backend_ready_ms"] = json!(started.elapsed().as_secs_f64() * 1000.);
                    println!("BENCH_READY {child_id}");
                }
                report["responses_ms"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(elapsed));
                report["ticks"] = json!(packet["tick"].as_u64().unwrap_or(0) + 1);
            }
            Ok(())
        }
    });
    let ui_result = crate::ui::benchmark_sensor_settings(bridge);
    let worker_result = worker
        .join()
        .map_err(|_| anyhow::anyhow!("fixture worker panicked"))?;
    let child_output = child.wait_with_output()?;
    consumer
        .join()
        .map_err(|_| anyhow::anyhow!("fixture consumer panicked"))?;
    let mut report = report.lock().unwrap();
    report["verified_shutdown"] =
        json!(ui_result.is_ok() && worker_result.is_ok() && child_output.status.success());
    report["broker_exit_code"] = json!(child_output.status.code());
    report["broker_stderr"] = json!(String::from_utf8_lossy(&child_output.stderr));
    report["elapsed_ms"] = json!(started.elapsed().as_secs_f64() * 1000.);
    std::fs::write(output, serde_json::to_vec_pretty(&*report)?)?;
    ui_result?;
    worker_result?;
    ensure!(child_output.status.success(), "fixture broker failed");
    Ok(())
}
struct CloseFixture(HWND);
impl Drop for CloseFixture {
    fn drop(&mut self) {
        unsafe {
            PostMessageW(self.0, WM_CLOSE, 0, 0);
        }
    }
}
unsafe extern "system" fn find_window(hwnd: HWND, parameter: isize) -> i32 {
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == GetCurrentProcessId() {
            let mut title = [0u16; 256];
            let length = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
            if length > 0
                && String::from_utf16_lossy(&title[..length as usize]).contains("传感器设置")
            {
                *(parameter as *mut HWND) = hwnd;
                return 0;
            }
        }
        1
    }
}
unsafe extern "system" fn find_status(hwnd: HWND, parameter: isize) -> i32 {
    unsafe {
        let mut title = [0u16; 256];
        let length = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
        if length > 0 && String::from_utf16_lossy(&title[..length as usize]).contains("尚未启用")
        {
            *(parameter as *mut HWND) = hwnd;
            return 0;
        }
        1
    }
}
