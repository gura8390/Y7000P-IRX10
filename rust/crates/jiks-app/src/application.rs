use crate::{ui, ui_model::*};
use anyhow::{Context, Result, bail};
use jiks_runtime::{
    ipc::BrokerClient,
    service::{Command, Snapshot},
};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

pub struct Options {
    pub data_root: PathBuf,
    pub read_only: bool,
    close_after: Option<Duration>,
    benchmark_log: Option<PathBuf>,
    minimized: bool,
}
impl Options {
    pub fn parse(arguments: &[String]) -> Result<Self> {
        let mut options = Self {
            data_root: PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA缺失")?)
                .join("Jiks"),
            read_only: false,
            close_after: None,
            benchmark_log: None,
            minimized: false,
        };
        let mut args = arguments.iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--read-only" => options.read_only = true,
                "--minimized" => options.minimized = true,
                "--data-dir" => {
                    options.data_root = PathBuf::from(args.next().context("--data-dir需要目录")?);
                }
                "--quit-after-ms" => {
                    options.close_after = Some(Duration::from_millis(
                        args.next().context("--quit-after-ms需要毫秒数")?.parse()?,
                    ));
                }
                "--benchmark-log" => {
                    options.benchmark_log = Some(PathBuf::from(
                        args.next().context("--benchmark-log需要路径")?,
                    ));
                }
                _ => bail!("未知参数 {argument}；可用 doctor、ui-smoke、--read-only、--data-dir"),
            }
        }
        // The file has a separate exact-line whitelist. Retained legacy lines
        // are never replayed as command-line arguments or broker commands.
        let saved = jiks_runtime::startup_arguments::load(&options.data_root)?;
        options.minimized |= saved.minimized;
        options.read_only |= saved.read_only;
        if options.close_after.is_some() && !options.read_only {
            bail!("定时退出测试仅允许只读运行");
        }
        Ok(options)
    }
}

pub fn run(options: Options) -> Result<()> {
    let Some(instance) = jiks_runtime::cli::InstanceLease::acquire(&options.data_root)? else {
        return jiks_runtime::cli::show_existing(&options.data_root);
    };
    anyhow::ensure!(instance.is_held(), "实例锁已失效");
    let instance_key = jiks_runtime::cli::InstanceLease::key(&options.data_root)?;
    let start = Instant::now();
    let executable = std::env::current_exe()?;
    let client = Arc::new(BrokerClient::spawn(
        &executable,
        &options.data_root,
        options.read_only,
    )?);
    let model = Arc::new(Mutex::new(UiModel::default()));
    let (actions, receiver) = mpsc::sync_channel::<UiAction>(16);
    let (window_signal, window_ready) = mpsc::channel();
    let thread_client = client.clone();
    let thread_model = model.clone();
    let mut benchmark_path = options.benchmark_log.clone();
    let worker = std::thread::Builder::new()
        .name("jiks-ui-bridge".into())
        .spawn(move || -> Result<()> {
            let mut revision = 0;
            let mut backend_ready_ms = None;
            let mut window_ready_ms = None;
            let mut output_last = Instant::now();
            loop {
                if window_ready_ms.is_none() && window_ready.try_recv().is_ok() {
                    window_ready_ms = Some(start.elapsed().as_secs_f64() * 1000.);
                }
                let snapshot = thread_client.snapshot();
                if snapshot.backend_ready && backend_ready_ms.is_none() {
                    backend_ready_ms = Some(start.elapsed().as_secs_f64() * 1000.);
                }
                if snapshot.revision != revision {
                    let mut model = thread_model.lock().unwrap_or_else(|p| p.into_inner());
                    apply_snapshot(&mut model, &snapshot);
                    revision = snapshot.revision;
                }
                if let Some(path) = &benchmark_path
                    && output_last.elapsed() >= Duration::from_secs(1)
                {
                    let record = benchmark_record(
                        &snapshot,
                        options.read_only,
                        thread_client.process_id(),
                        start,
                        window_ready_ms,
                        backend_ready_ms,
                        false,
                    );
                    match serde_json::to_vec_pretty(&record)
                        .map_err(anyhow::Error::from)
                        .and_then(|bytes| std::fs::write(path, bytes).map_err(anyhow::Error::from))
                    {
                        Ok(()) => {}
                        Err(error) => {
                            thread_model
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .finish(
                                    0,
                                    false,
                                    format!("基准日志未能保存；界面和停止通道继续运行：{error:#}"),
                                );
                            benchmark_path = None;
                        }
                    }
                    output_last = Instant::now();
                }
                match receiver.recv_timeout(Duration::from_millis(80)) {
                    Ok(action) => {
                        let id = action.request_id();
                        if let UiAction::Execute {
                            command, payload, ..
                        } = &action
                        {
                            if let Some(result) =
                                local_tool(command, payload, &options.data_root, &snapshot)
                            {
                                let (ok, message) = match result {
                                    Ok(message) => (true, message),
                                    Err(error) => (false, format!("{error:#}")),
                                };
                                thread_model
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .finish(id, ok, message);
                                continue;
                            }
                        }
                        let command = match action {
                            UiAction::Execute {
                                request_id,
                                command,
                                payload,
                            } => Command::Execute {
                                request_id,
                                command,
                                payload,
                            },
                            UiAction::SaveDocument {
                                request_id,
                                document,
                                value,
                            } => Command::SaveDocument {
                                request_id,
                                document,
                                value,
                            },
                            UiAction::Shutdown { request_id } => Command::Shutdown { request_id },
                        };
                        if let Err(error) = thread_client.submit(command) {
                            thread_model
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .finish(id, false, format!("{error:#}"));
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        if !snapshot.shutdown_ready {
                            thread_client.submit(Command::Shutdown { request_id: 0 })?;
                        }
                        break;
                    }
                }
            }
            if let Some(path) = &benchmark_path {
                let snapshot = thread_client.snapshot();
                let record = benchmark_record(
                    &snapshot,
                    options.read_only,
                    thread_client.process_id(),
                    start,
                    window_ready_ms,
                    backend_ready_ms,
                    true,
                );
                if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
                    let _ = std::fs::write(path, bytes);
                }
            }
            // Native UI returns only after verified shutdown. If its creation failed,
            // child stdin will close when this Arc drops; the child owns restoration.
            Ok(())
        })?;
    let mut bridge = UiBridge::new(model, actions);
    bridge.instance_key = Some(instance_key);
    bridge.startup_language_prompt = !options.read_only;
    bridge.start_minimized = options.minimized;
    bridge.window_ready = Some(window_signal);
    if let Some(duration) = options.close_after {
        bridge.smoke_close_after = Some(duration);
        bridge.smoke_exercise_reentrancy = false;
    }
    let result = ui::run(bridge);
    let bridge_result = worker
        .join()
        .map_err(|_| anyhow::anyhow!("界面后台线程异常"))?;
    result?;
    bridge_result?;
    let verification = client.wait_verified();
    if let Some(error) = client
        .snapshot()
        .values
        .get("backend.terminal_error")
        .and_then(|v| v.as_str())
    {
        bail!("{error}");
    }
    verification?;
    Ok(())
}
fn local_tool(
    command: &str,
    payload: &serde_json::Value,
    data_root: &std::path::Path,
    state: &Snapshot,
) -> Option<Result<String>> {
    if command == "update.open_release" {
        return Some((|| {
            let url = state
                .values
                .get("update.release_url")
                .and_then(serde_json::Value::as_str)
                .context("请先检查到有效的Jiks发行版本")?;
            let tag = url
                .strip_prefix("https://github.com/gura8390/Y7000P-IRX10/releases/tag/")
                .filter(|tag| {
                    !tag.is_empty()
                        && tag.len() <= 128
                        && tag
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"._-/".contains(&byte))
                })
                .context("更新链接不属于本项目的可信发行页")?;
            anyhow::ensure!(!tag.split('/').any(|part| part == ".."), "更新tag路径无效");
            open_local_target(std::path::Path::new(url))
        })());
    }
    if command == "packages.readme" {
        return Some((|| {
            let id = payload
                .get("selection.packages")
                .and_then(|v| v.as_str())
                .context("请选择目录项目")?;
            let item = state
                .collections
                .get("packages")
                .and_then(|items| items.iter().find(|item| item.id == id))
                .context("所选项目已不在当前已读取目录中")?;
            let url = item
                .value
                .get("readme_url")
                .and_then(|v| v.as_str())
                .filter(|s| s.starts_with("https://"))
                .context("此项目未提供HTTPS说明文档")?;
            open_local_target(std::path::Path::new(url))
        })());
    }
    let target = match command {
        "tools.open_logs" => data_root.join("logs"),
        "tools.open_data" | "tools.open_folder" => data_root.to_path_buf(),
        "packages.open_directory" => PathBuf::from(
            payload
                .get("packages.directory")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        ),
        "tools.power_settings" => PathBuf::from("powercfg.cpl"),
        "tools.credits" => PathBuf::from("https://github.com/BartoszCichecki/LenovoLegionToolkit"),
        "tools.donate" => {
            PathBuf::from("https://github.com/BartoszCichecki/LenovoLegionToolkit#donate")
        }
        _ => return None,
    };
    Some((|| {
        if matches!(
            command,
            "tools.open_logs" | "tools.open_data" | "tools.open_folder" | "packages.open_directory"
        ) && (!target.is_absolute() || !target.is_dir())
        {
            bail!("目录当前不存在。请选择已存在的目录；日志目录仅在启用日志后创建。");
        }
        open_local_target(&target)
    })())
}
fn open_local_target(target: &std::path::Path) -> Result<String> {
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    let wide = |s: &std::ffi::OsStr| {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().chain(Some(0)).collect::<Vec<_>>()
    };
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            wide(target.as_os_str()).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result <= 32 {
        bail!("Windows未能打开目标（代码{result}）");
    }
    Ok("已交由Windows打开".into())
}
fn benchmark_record(
    snapshot: &Snapshot,
    read_only: bool,
    broker_pid: u32,
    start: Instant,
    window_ready_ms: Option<f64>,
    backend_ready_ms: Option<f64>,
    ui_closed: bool,
) -> serde_json::Value {
    json!({"application":"Jiks","read_only":read_only,"pid":std::process::id(),"broker_pid":broker_pid,
        "window_created_ms":window_ready_ms,"backend_ready_ms":backend_ready_ms,"elapsed_ms":start.elapsed().as_secs_f64()*1000.,
        "metadata_revision":snapshot.metadata_revision,"revision":snapshot.revision,"backend_ready":snapshot.backend_ready,"shutdown_ready":snapshot.shutdown_ready,"ui_closed":ui_closed,
        "metrics_count":snapshot.metrics.len(),"hardware_sensors_enabled":boolean(snapshot,"sensors.enabled",false),"thermal_active":boolean(snapshot,"thermal.active",false),"thermal_shadow":boolean(snapshot,"thermal.shadow",false),"configured_thermal_sampling_ms":snapshot.values.get("thermal.sampling_ms"),
        "power_modes":snapshot.options.get("power.mode").map(|o|o.iter().map(|c|json!({"label":c.label,"value":c.value})).collect::<Vec<_>>()),
        "initialization_failed":snapshot.values.contains_key("backend.terminal_error")})
}

pub fn apply_snapshot(model: &mut UiModel, state: &Snapshot) {
    let metadata_changed = model.metadata_revision != state.metadata_revision;
    model.revision = state.revision;
    model.version = state.version.clone();
    model.machine = state.machine.clone();
    model.notice = state.notice.clone();
    model.busy = state
        .values
        .get("backend.busy")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    model.values = state.values.clone();
    model
        .values
        .insert("backend.ready".into(), json!(state.backend_ready));
    model.shutdown_ready = state.shutdown_ready;
    model.metrics = state
        .metrics
        .iter()
        .map(|v| UiMetric {
            key: v.key.clone(),
            label: v.label.clone(),
            value: v.value.clone(),
            detail: v.detail.clone(),
        })
        .collect();
    model.results = state
        .results
        .iter()
        .map(|v| UiResult {
            request_id: v.request_id,
            ok: v.ok,
            message: v.message.clone(),
        })
        .collect();
    model.preferences = UiPreferences {
        minimize_to_tray: boolean(state, "app.minimize_to_tray", true),
        close_to_tray: boolean(state, "app.close_to_tray", false),
        always_on_top: boolean(state, "app.always_on_top", false),
        compact: boolean(state, "app.compact", false),
    };
    if metadata_changed {
        model.capabilities = state
            .capabilities
            .iter()
            .map(|(key, v)| {
                (
                    key.clone(),
                    UiCapability {
                        available: v.available,
                        reason: v.reason.clone(),
                    },
                )
            })
            .collect();
        model.options = state
            .options
            .iter()
            .map(|(key, v)| {
                (
                    key.clone(),
                    v.iter()
                        .map(|v| UiChoice {
                            label: v.label.clone(),
                            value: v.value.clone(),
                        })
                        .collect(),
                )
            })
            .collect();
        model.ranges = state
            .ranges
            .iter()
            .map(|(key, v)| {
                (
                    key.clone(),
                    UiRange {
                        minimum: v.minimum,
                        maximum: v.maximum,
                        step: v.step,
                        unit: v.unit.clone(),
                    },
                )
            })
            .collect();
        model.documents = state
            .documents
            .iter()
            .map(|(key, v)| {
                (
                    key.clone(),
                    UiDocument {
                        title: v.title.clone(),
                        value: v.value.0.as_ref().clone(),
                        writable: v.writable,
                        reason: v.reason.clone(),
                    },
                )
            })
            .collect();
        model.collections = state
            .collections
            .iter()
            .map(|(key, v)| {
                (
                    key.clone(),
                    v.iter()
                        .map(|v| UiItem {
                            id: v.id.clone(),
                            label: v.label.clone(),
                            detail: v.detail.clone(),
                            value: v.value.clone(),
                        })
                        .collect(),
                )
            })
            .collect();
        model.quick_actions = state
            .collections
            .get("quick_actions")
            .into_iter()
            .flatten()
            .map(|v| UiQuickAction {
                id: v.id.clone(),
                label: v.label.clone(),
                icon: v
                    .value
                    .get("icon")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .into(),
            })
            .collect();
        model.metadata_revision = state.metadata_revision;
    }
}
fn boolean(state: &Snapshot, key: &str, fallback: bool) -> bool {
    state
        .values
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn realtime_bridge_keeps_editable_static_documents_without_copying_them() {
        let mut model = UiModel::default();
        let mut state = Snapshot::default();
        state.metadata_revision = 7;
        state.documents = Arc::new(std::collections::BTreeMap::from([(
            "test".into(),
            jiks_runtime::service::Document {
                title: "test".into(),
                value: jiks_runtime::service::ArcValue(Arc::new(json!({"original":true}))),
                writable: true,
                reason: String::new(),
            },
        )]));
        apply_snapshot(&mut model, &state);
        model.documents.get_mut("test").unwrap().value = json!({"retained":true});
        state.revision += 1;
        apply_snapshot(&mut model, &state);
        assert_eq!(model.documents["test"].value, json!({"retained":true}));
        state.metadata_revision += 1;
        apply_snapshot(&mut model, &state);
        assert_eq!(model.documents["test"].value, json!({"original":true}));
    }
    #[test]
    fn unattended_quit_requires_read_only() {
        let root = std::env::temp_dir()
            .join(format!("JiksOptionsAbsent-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        assert!(
            Options::parse(&[
                "--data-dir".into(),
                root.clone(),
                "--quit-after-ms".into(),
                "10".into()
            ])
            .is_err()
        );
        assert!(
            Options::parse(&[
                "--data-dir".into(),
                root,
                "--read-only".into(),
                "--quit-after-ms".into(),
                "10".into()
            ])
            .is_ok()
        );
    }
    #[test]
    fn actual_startup_consumes_only_supported_saved_arguments_without_replaying_internal_lines() {
        let root = std::env::temp_dir().join(format!("JiksOptionsSaved-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("args.txt"),b"--minimized\r\n--read-only\r\nbroker\r\n--data-dir\r\nX:\\ignored\r\n--quit-after-ms\r\n1\r\n").unwrap();
        let options =
            Options::parse(&["--data-dir".into(), root.to_string_lossy().into_owned()]).unwrap();
        assert!(options.minimized && options.read_only);
        assert_eq!(options.data_root, root);
        assert!(options.close_after.is_none());
        assert!(options.benchmark_log.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
