use super::*;
use crate::cli::{Endpoint, Request, Response};
use std::ptr::{null, null_mut};
use windows_sys::Win32::{Foundation::*, System::Registry::*};
const CUSTOM: &str = r"Software\HWiNFO64\Sensors\Custom\Jiks";
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
fn set_string(path: &str, name: &str, value: &str) -> Result<()> {
    unsafe {
        let mut key = null_mut();
        let mut disposition = 0;
        let result = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(path).as_ptr(),
            0,
            null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            null(),
            &mut key,
            &mut disposition,
        );
        ensure!(result == 0, "注册表键未打开：{result}");
        let key = Key(key);
        let bytes = wide(value);
        let result = RegSetValueExW(
            key.0,
            wide(name).as_ptr(),
            0,
            REG_SZ,
            bytes.as_ptr().cast(),
            (bytes.len() * 2) as u32,
        );
        ensure!(result == 0, "注册表值未写入：{result}");
        let mut actual = vec![0u16; bytes.len() + 1];
        let mut size = (actual.len() * 2) as u32;
        let mut kind = 0;
        let result = RegQueryValueExW(
            key.0,
            wide(name).as_ptr(),
            null(),
            &mut kind,
            actual.as_mut_ptr().cast(),
            &mut size,
        );
        ensure!(
            result == 0 && kind == REG_SZ && actual[..bytes.len()] == bytes,
            "注册表写后读回不一致"
        );
        Ok(())
    }
}
fn clear_sensor(path:&str)->Result<()> {let code=unsafe{RegDeleteTreeW(HKEY_CURRENT_USER,wide(path).as_ptr())};ensure!([0,ERROR_FILE_NOT_FOUND,ERROR_PATH_NOT_FOUND].contains(&code),"HWiNFO失效值未清理：{code}");Ok(())}
fn set_dword(path:&str,name:&str,value:u32)->Result<()> {
    unsafe {
        let mut key=null_mut();let mut disposition=0;
        ensure!(RegCreateKeyExW(HKEY_CURRENT_USER,wide(path).as_ptr(),0,null(),0,KEY_SET_VALUE|KEY_QUERY_VALUE,null(),&mut key,&mut disposition)==0,"HWiNFO键未打开");let key=Key(key);
        ensure!(RegSetValueExW(key.0,wide(name).as_ptr(),0,REG_DWORD,(&value as *const u32).cast(),4)==0,"HWiNFO数值未写入");
        let mut actual=0u32;let mut kind=0;let mut size=4;
        ensure!(RegQueryValueExW(key.0,wide(name).as_ptr(),null(),&mut kind,(&mut actual as *mut u32).cast(),&mut size)==0&&kind==REG_DWORD&&size==4&&actual==value,"HWiNFO数值读回不一致");Ok(())
    }
}
fn clear_hwinfo() -> Result<()> {
    let code = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(CUSTOM).as_ptr()) };
    ensure!(
        code == 0 || code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND,
        "HWiNFO清理未完成：{code}"
    );
    Ok(())
}
fn user_path(enabled: bool) -> Result<()> {
    unsafe {
        let directory = std::env::current_exe()?
            .parent()
            .context("应用目录缺失")?
            .to_string_lossy()
            .into_owned();
        let mut key = null_mut();
        let code = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide("Environment").as_ptr(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut key,
        );
        ensure!(code == 0, "用户环境变量键未打开：{code}");
        let key = Key(key);
        let mut kind = 0;
        let mut bytes = 0;
        let code = RegQueryValueExW(
            key.0,
            wide("Path").as_ptr(),
            null(),
            &mut kind,
            null_mut(),
            &mut bytes,
        );
        ensure!(
            code == 0 || code == ERROR_FILE_NOT_FOUND,
            "用户PATH未读出：{code}"
        );
        ensure!(bytes <= 128 * 1024, "用户PATH过长");
        if code == ERROR_FILE_NOT_FOUND {
            kind = REG_EXPAND_SZ;
        }
        ensure!(
            kind == REG_SZ || kind == REG_EXPAND_SZ,
            "用户PATH格式异常，未改写"
        );
        let mut units = vec![0u16; (bytes as usize + 1) / 2 + 1];
        if bytes > 0 {
            ensure!(
                RegQueryValueExW(
                    key.0,
                    wide("Path").as_ptr(),
                    null(),
                    &mut kind,
                    units.as_mut_ptr().cast(),
                    &mut bytes
                ) == 0,
                "PATH读取失败"
            );
        }
        let old = String::from_utf16(
            &units[..units.iter().position(|v| *v == 0).unwrap_or(units.len())],
        )?;
        let mut paths: Vec<_> = if old.is_empty() {
            vec![]
        } else {
            old.split(';').map(str::to_owned).collect()
        };
        let matches = |s: &str| {
            s.trim_matches('"')
                .trim_end_matches('\\')
                .eq_ignore_ascii_case(directory.trim_end_matches('\\'))
        };
        if enabled {
            if !paths.iter().any(|p| matches(p)) {
                paths.push(directory);
            }
        } else {
            paths.retain(|p| !matches(p));
        }
        let value = wide(&paths.join(";"));
        ensure!(
            RegSetValueExW(
                key.0,
                wide("Path").as_ptr(),
                0,
                kind,
                value.as_ptr().cast(),
                (value.len() * 2) as u32
            ) == 0,
            "用户PATH写入失败"
        );
        let mut actual = vec![0u16; value.len() + 1];
        let mut actual_bytes = (actual.len() * 2) as u32;
        ensure!(
            RegQueryValueExW(
                key.0,
                wide("Path").as_ptr(),
                null(),
                &mut kind,
                actual.as_mut_ptr().cast(),
                &mut actual_bytes
            ) == 0
                && actual[..value.len()] == value,
            "用户PATH读回不一致"
        );
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        let mut ignored = 0;
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            wide("Environment").as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            500,
            &mut ignored,
        );
        Ok(())
    }
}
#[derive(Default)]
pub(super) struct IntegrationHost {
    cli: Option<Endpoint>,
    request: Option<u64>,
    hwinfo_owned: bool,
    next: u64,
    cli_attempted: bool,
}
impl IntegrationHost {
    pub(super) fn is_busy(&self) -> bool {
        self.request.is_some()
    }
    pub(super) fn needs_cleanup(&self) -> bool {
        self.hwinfo_owned
    }
    pub(super) fn is_active(&self) -> bool {
        self.cli.is_some() || self.hwinfo_owned
    }
}
impl Engine {
    /// Coordinator-owned physical ticket only, not telemetry/auxiliary readers.
    /// Fixed labels deliberately exclude the command payload and configuration.
    fn physical_status_at(&self, now: u64) -> Value {
        match &self.pending {
            Some(pending) => json!({
                "busy": true,
                "operation": match &pending.action {
                    A::ReadDriverKeyEvents => "keys.driver.read",
                    A::StartDriverKeyEvents => "keys.driver.start",
                    A::StopDriverKeyEvents => "keys.driver.stop",
                    A::SetAutorun(_) => "app.autorun.apply",
                    A::ShutdownWorker => "backend.shutdown",
                    _ => "device-operation",
                },
                "request_id": pending.work.id,
                "elapsed_ms": now.saturating_sub(pending.started),
                "deadline_remaining_ms": pending.work.deadline_tick_ms.saturating_sub(now),
                "late": pending.work.late,
                "cancel_requested": pending.cancel.load(std::sync::atomic::Ordering::Acquire),
            }),
            None => json!({"busy":false,"operation":null,"request_id":null,
                "elapsed_ms":null,"deadline_remaining_ms":null,"late":false,
                "cancel_requested":false}),
        }
    }
    pub(super) fn request_integration(&mut self, id: u64, payload: &Value) -> Result<()> {
        self.writable()?;
        let mut root = self.repository.document("integrations")?.root().clone();
        for (ui, key) in [("integration.hwinfo", "HWiNFO"), ("integration.cli", "CLI")] {
            if let Some(value) = payload.get(ui) {
                root.set_known(
                    key,
                    JsonNode::Bool(value.as_bool().context("集成开关必须为布尔值")?),
                )?;
            }
        }
        if let Some(value) = payload.get("integration.path") {
            root.set_known(
                "JiksPathAdded",
                JsonNode::Bool(value.as_bool().context("PATH开关必须为布尔值")?),
            )?;
        }
        let new = SettingsDocument::from_root(root)?;
        self.repository
            .validate_changes(&[("integrations".into(), new.clone())])?;
        let cli = new.root().get_ci("CLI").and_then(JsonNode::as_bool) == Some(true);
        // Bind before saving an enabled switch. Failure retains the previous file.
        if cli && self.integration.cli.is_none() {
            self.integration.cli = Some(Endpoint::open(self.repository.root())?);
        }
        if new.root().get_ci("HWiNFO").and_then(JsonNode::as_bool) != Some(true)
            && self.integration.hwinfo_owned
        {
            clear_hwinfo()?;
            self.integration.hwinfo_owned = false;
        }
        if let Some(enabled) = payload.get("integration.path").and_then(Value::as_bool) {
            let previous = self
                .repository
                .document("integrations")?
                .root()
                .get_ci("JiksPathAdded")
                .and_then(JsonNode::as_bool)
                .unwrap_or(false);
            if enabled || previous {
                user_path(enabled)?;
            }
        }
        self.repository.save("integrations", new)?;
        self.publish_configuration()?;
        self.finish(
            id,
            true,
            "集成设置已保存；CLI启用时端点已实际注册，HWiNFO共享状态请查看实时说明",
        );
        Ok(())
    }
    pub(super) fn poll_integration(&mut self) -> Result<()> {
        let mut host = std::mem::take(&mut self.integration);
        let result = (|| -> Result<()> {
            let root = self.repository.document("integrations")?.root();
            let cli = !self.read_only
                && self.shutdown.is_none()
                && root.get_ci("CLI").and_then(JsonNode::as_bool) == Some(true);
            let hwinfo = !self.read_only
                && self.shutdown.is_none()
                && root.get_ci("HWiNFO").and_then(JsonNode::as_bool) == Some(true);
            if !cli && host.request.is_none() {
                host.cli = None;
                host.cli_attempted = false;
            }
            if cli && host.cli.is_none() && !host.cli_attempted {
                host.cli_attempted = true;
                match Endpoint::open(self.repository.root()) {
                    Ok(endpoint) => host.cli = Some(endpoint),
                    Err(error) => self.value(
                        "integration.cli.status",
                        json!(format!("CLI端点未注册：{error:#}")),
                    ),
                };
            }
            if let Some(id) = host.request {
                if let Some(result) = self.take_internal_result(id) {
                    host.request = None;
                    if let Some(endpoint) = host.cli.as_mut() {
                        let (ok, message) = match result {
                            Ok(()) => (true, "操作已实际完成".to_owned()),
                            Err(error) => (false, error),
                        };
                        endpoint.respond(Response {
                            ok,
                            message,
                            value: Value::Null,
                        })?;
                    }
                }
            }
            if let Some(endpoint) = host.cli.as_mut() {
                if let Some(request) = endpoint.poll()? {
                    match request {
                        Request::Status => {
                            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                            endpoint.respond(Response{ok:true,message:"已运行协调器的当前状态".into(),value:json!({"version":state.version,"machine":state.machine,"backend_ready":state.backend_ready,"thermal_active":state.values.get("thermal.active"),"metrics":state.metrics,"notice":state.notice,"physical":self.physical_status_at(monotonic_ms())})})?;
                        }
                        Request::Execute { command, payload } => {
                            if !cli
                                || command.len() > 128
                                || !payload.is_object()
                                || command == "integration.apply"
                                || command.ends_with(".effect.complete")
                                || command == "ui.visibility"
                                || command == "osd.visibility"
                            {
                                endpoint.respond(Response {
                                    ok: false,
                                    message: "CLI已关闭、请求格式错误，或该集成动作须在界面完成"
                                        .into(),
                                    value: Value::Null,
                                })?;
                            } else {
                                let id = self.new_internal_request()?;
                                host.request = Some(id);
                                if let Err(error) = self.process_foreground(Command::Execute {
                                    request_id: id,
                                    command,
                                    payload,
                                }) {
                                    self.finish(id, false, format!("{error:#}"));
                                }
                            }
                        }
                    }
                }
            }
            if !hwinfo && host.hwinfo_owned {
                clear_hwinfo()?;
                host.hwinfo_owned = false;
            }
            if hwinfo && monotonic_ms() >= host.next {
                host.next = monotonic_ms().saturating_add(1000);
                if let Some(firmware) = self.cache.get("firmware") {
                    for (key, index, name) in [
                        ("cpu_fan_rpm", 0, "CPU Fan"),
                        ("gpu_fan_rpm", 1, "GPU Fan"),
                        ("pch_fan_rpm", 2, "PCH Fan"),
                    ] {
                        if let Some(value) = firmware
                            .pointer(&format!("/values/{key}"))
                            .filter(|v| v.get("available").and_then(Value::as_bool) == Some(true))
                            .and_then(|v| v.get("value"))
                            .and_then(Value::as_u64)
                        {
                            let path = format!(r"{CUSTOM}\Fan{index}");
                            host.hwinfo_owned = true;
                            set_string(&path, "Name", name)?;
                            set_dword(&path, "Value", u32::try_from(value)?)?;
                        } else {
                            clear_sensor(&format!(r"{CUSTOM}\Fan{index}"))?;
                        }
                    }
                }
                host.hwinfo_owned = true;
                set_string(&format!(r"{CUSTOM}\Temp0"),"Name","Battery Temperature")?;
                let temperature=self.cache.get("battery.thermal").filter(|value|value.get("completed_at_tick_ms").and_then(Value::as_u64).is_some_and(|tick|monotonic_ms().saturating_sub(tick)<2500)).and_then(|value|value.get("celsius")).and_then(Value::as_f64).filter(|value|value.is_finite()&&(-20.0..100.0).contains(value));
                set_string(&format!(r"{CUSTOM}\Temp0"),"Value",&temperature.map(|value|value.to_string()).unwrap_or_default())?;
                self.value(
                    "integration.hwinfo.status",
                    json!(if host.hwinfo_owned {
                        "已共享原始风扇与实际电池温度；未知温度为空，不制造数值"
                    } else {
                        "等待有效传感器数据；未制造温度或风扇值"
                    }),
                );
                if !self.read_queue.iter().any(|item| item.0 == "firmware")
                    && self.reading.as_ref().is_none_or(|r| r.key != "firmware")
                {
                    self.enqueue_read("firmware".into(), A::ReadFirmwareSensors, false);
                }
            }
            Ok(())
        })();
        self.integration = host;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_status_reports_owned_ticket_age_and_lateness_without_payloads() {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        assert_eq!(f.engine.physical_status_at(100)["busy"], false);
        assert!(f.engine.physical_status_at(100)["operation"].is_null());
        let _answer = f.answer();
        f.engine.start_work(515,"private payload must never be disclosed",vec![A::ReadDriverKeyEvents],After::Hardware).unwrap();
        let pending = f.engine.pending.as_mut().unwrap();
        pending.started = 100;
        pending.work.deadline_tick_ms = 200;
        pending.work.late = true;
        pending.cancel.store(true,std::sync::atomic::Ordering::Release);
        let actual = f.engine.physical_status_at(250);
        assert_eq!(actual["busy"],true);
        assert_eq!(actual["operation"],"keys.driver.read");
        assert_eq!(actual["request_id"],515);
        assert_eq!(actual["elapsed_ms"],150);
        assert_eq!(actual["deadline_remaining_ms"],0);
        assert_eq!(actual["late"],true);
        assert_eq!(actual["cancel_requested"],true);
        assert!(!actual.to_string().contains("private payload"));
        assert!(f.engine.pending.is_some(),"status cleared or consumed the ticket");
    }
}
