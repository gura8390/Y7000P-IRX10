use super::*;
use crate::packages::{
    self, Availability, Catalog, Channel, Event, Message, PackageService, Source, WindowsVersion,
};
#[derive(Default)]
pub(super) struct PackageHost {
    service: PackageService,
    catalog: Catalog,
    filter: String,
    sort: String,
    source: Option<Source>,
    os: Option<WindowsVersion>,
    terminal: Option<Message>,
    cancel_waiters: Vec<u64>,
    auto_request: Option<u64>,
    auto_signature: Option<String>,
    auto_retry_after: u64,
}
impl PackageHost {
    fn visible<'a>(&'a self, document: &SettingsDocument) -> Vec<&'a packages::Package> {
        let filter = self.filter.to_lowercase();
        let mut items = packages::visible_packages(&self.catalog, document)
            .into_iter()
            .filter(|package| {
                filter.is_empty()
                    || [&package.title, &package.category, &package.description]
                        .iter()
                        .any(|text| text.to_lowercase().contains(&filter))
            })
            .collect::<Vec<_>>();
        items.sort_by(|left, right| {
            let order = match self.sort.as_str() {
                "category" => left
                    .category
                    .to_lowercase()
                    .cmp(&right.category.to_lowercase()),
                "date" => {
                    package_date_key(&right.release_date).cmp(&package_date_key(&left.release_date))
                }
                _ => left.title.to_lowercase().cmp(&right.title.to_lowercase()),
            };
            order
                .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
                .then_with(|| left.id.cmp(&right.id))
        });
        items
    }
    pub(super) fn is_busy(&self) -> bool {
        self.service.is_busy() || self.terminal.is_some()
    }
    pub(super) fn cancel(&self) {
        self.service.cancel();
    }
}
fn package_date_key(raw: &str) -> u64 {
    let trimmed = raw.trim();
    if trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return trimmed.parse().unwrap_or(0);
    }
    let parts = trimmed
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .take(3)
        .filter_map(|part| part.parse::<u64>().ok())
        .collect::<Vec<_>>();
    match parts.as_slice() {
        [year, month, day] if *year >= 1900 && *month <= 12 && *day <= 31 => {
            year * 10000 + month * 100 + day
        }
        _ => 0,
    }
}
impl Engine {
    pub(super) fn publish_packages(&mut self) -> Result<()> {
        let document = self.repository.document("packages")?;
        let items = self
            .packages
            .visible(document)
            .into_iter()
            .map(|package| {
                Ok(Item {
                    id: package.id.clone(),
                    label: package.title.clone(),
                    detail: format!(
                        "{} · {} · {}",
                        package.version,
                        package.file_size,
                        match &package.availability {
                            Availability::Update => "可更新".into(),
                            Availability::NotUpdate => "未检测到更新".into(),
                            Availability::Unknown(reason) => format!("更新状态未确定：{reason}"),
                        }
                    ),
                    value: serde_json::to_value(package)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let directory = document
            .root()
            .get_ci("DownloadPath")
            .and_then(JsonNode::as_str)
            .unwrap_or("")
            .to_owned();
        let only = document
            .root()
            .get_ci("OnlyShowUpdates")
            .and_then(JsonNode::as_bool)
            .unwrap_or(false);
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut options = state.options.as_ref().clone();
        options.insert(
            "packages.sort".into(),
            vec![
                Choice {
                    label: "名称".into(),
                    value: json!("name"),
                },
                Choice {
                    label: "分类".into(),
                    value: json!("category"),
                },
                Choice {
                    label: "发布日期（最新在前）".into(),
                    value: json!("date"),
                },
            ],
        );
        options.insert(
            "packages.source".into(),
            vec![
                Choice {
                    label: "Lenovo PC Support".into(),
                    value: json!("pc_support"),
                },
                Choice {
                    label: "Lenovo Vantage".into(),
                    value: json!("vantage"),
                },
            ],
        );
        options.insert(
            "packages.os".into(),
            vec![
                Choice {
                    label: "Windows 11".into(),
                    value: json!("Windows11"),
                },
                Choice {
                    label: "Windows 10".into(),
                    value: json!("Windows10"),
                },
            ],
        );
        state.options = Arc::new(options);
        for (key, value) in [
            ("packages.machine_type", json!("83NN")),
            (
                "packages.source",
                json!(if self.packages.source == Some(Source::Vantage) {
                    "vantage"
                } else {
                    "pc_support"
                }),
            ),
            (
                "packages.os",
                json!(if self.packages.os == Some(WindowsVersion::Windows10) {
                    "Windows10"
                } else {
                    "Windows11"
                }),
            ),
            ("packages.directory", json!(directory)),
            ("packages.filter", json!(self.packages.filter)),
            (
                "packages.sort",
                json!(if self.packages.sort.is_empty() {
                    "name"
                } else {
                    &self.packages.sort
                }),
            ),
            ("packages.only_updates", json!(only)),
        ] {
            state.values.insert(key.into(), value);
        }
        let mut collections = state.collections.as_ref().clone();
        collections.insert("packages".into(), items);
        state.collections = Arc::new(collections);
        let mut capabilities = state.capabilities.as_ref().clone();
        for command in [
            "packages.refresh",
            "packages.filter",
            "packages.cancel",
            "packages.readme",
            "packages.copy.info",
            "packages.copy.url",
            "packages.copy.title",
            "packages.open_directory",
            "update.check",
        ] {
            capabilities.insert(
                command.into(),
                Capability {
                    available: true,
                    reason: String::new(),
                },
            );
        }
        for command in [
            "packages.download",
            "packages.hide",
            "packages.hide_all",
            "packages.unhide_all",
        ] {
            capabilities.insert(
                command.into(),
                Capability {
                    available: !self.read_only,
                    reason: if self.read_only {
                        "只读运行".into()
                    } else {
                        String::new()
                    },
                },
            );
        }
        state.capabilities = Arc::new(capabilities);
        state.metadata_revision += 1;
        state.revision += 1;
        Ok(())
    }
    pub(super) fn request_packages(
        &mut self,
        id: u64,
        command: &str,
        payload: &Value,
    ) -> Result<()> {
        match command {
            "packages.filter" => {
                let filter = payload
                    .get("packages.filter")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let sort = payload
                    .get("packages.sort")
                    .and_then(Value::as_str)
                    .unwrap_or("name");
                ensure!(
                    ["name", "category", "date"].contains(&sort),
                    "驱动列表排序无效"
                );
                self.packages.filter = filter.chars().take(256).collect();
                self.packages.sort = sort.into();
                self.publish_packages()?;
                self.finish(id, true, "当前目录筛选与排序已更新；未重新联网");
            }
            "packages.cancel" => {
                if self.packages.service.is_busy() {
                    self.packages.service.cancel();
                    ensure!(self.packages.cancel_waiters.len() < 16, "取消请求过多");
                    self.packages.cancel_waiters.push(id);
                } else {
                    self.finish(id, true, "没有正在进行的下载或目录读取");
                }
            }
            "packages.refresh" => {
                if let Some(machine) = payload.get("packages.machine_type").and_then(Value::as_str)
                {
                    ensure!(
                        machine.eq_ignore_ascii_case("83NN"),
                        "此版本仅为本机83NN提供目录，不猜测其它机型"
                    );
                }
                let source = match payload
                    .get("packages.source")
                    .and_then(Value::as_str)
                    .unwrap_or("pc_support")
                {
                    "pc_support" | "PCSupport" => Source::PcSupport,
                    "vantage" | "Vantage" => Source::Vantage,
                    _ => bail!("未知Lenovo目录来源"),
                };
                let os = match payload
                    .get("packages.os")
                    .and_then(Value::as_str)
                    .unwrap_or("Windows11")
                {
                    "Windows11" | "win11" => WindowsVersion::Windows11,
                    "Windows10" | "win10" => WindowsVersion::Windows10,
                    _ => bail!("请选择支持的Windows目录"),
                };
                ensure!(
                    !self.packages.service.is_busy(),
                    "上一个目录/下载请求尚未实际结束"
                );
                let mut preferences = serde_json::Map::new();
                if let Some(value) = payload.get("packages.only_updates") {
                    preferences.insert("OnlyShowUpdates".into(), value.clone());
                }
                if let Some(path) = payload
                    .get("packages.directory")
                    .and_then(Value::as_str)
                    .filter(|path| !path.trim().is_empty())
                {
                    preferences.insert("DownloadPath".into(), json!(path));
                }
                if !self.read_only && !preferences.is_empty() {
                    let document = packages::preferences_change(
                        self.repository.document("packages")?,
                        &JsonNode::parse(&Value::Object(preferences).to_string())?,
                    )?;
                    self.repository.save("packages", document)?;
                }
                self.packages.filter = payload
                    .get("packages.filter")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(256)
                    .collect();
                let sort = payload
                    .get("packages.sort")
                    .and_then(Value::as_str)
                    .unwrap_or("name");
                ensure!(
                    ["name", "category", "date"].contains(&sort),
                    "驱动列表排序无效"
                );
                self.packages.sort = sort.into();
                self.packages.source = Some(source);
                self.packages.os = Some(os);
                self.packages.service.start_catalog_local(id, source, os)?;
                self.value(
                    "packages.progress",
                    json!("正在读取本机Lenovo驱动目录；不会安装或运行下载文件"),
                );
                self.publish_packages()?;
            }
            "packages.download" => {
                self.writable()?;
                let selected = payload
                    .get("selection.packages")
                    .and_then(Value::as_str)
                    .context("请选择下载项目")?;
                let package = self
                    .packages
                    .catalog
                    .packages
                    .iter()
                    .find(|p| p.id == selected)
                    .context("所选项目已不在当前目录中")?
                    .clone();
                let directory = payload
                    .get("packages.directory")
                    .or_else(|| payload.get("directory"))
                    .and_then(Value::as_str)
                    .context("请填写下载目录")?;
                self.packages
                    .service
                    .start_download(id, package, PathBuf::from(directory))?;
            }
            "packages.hide" | "packages.hide_all" | "packages.unhide_all" => {
                self.writable()?;
                let mut hidden = self
                    .repository
                    .document("packages")?
                    .root()
                    .get_ci("HiddenPackages")
                    .and_then(JsonNode::as_array)
                    .map(|items| items.to_vec())
                    .unwrap_or_default();
                if command == "packages.hide" {
                    let id = payload
                        .get("selection.packages")
                        .and_then(Value::as_str)
                        .context("请选择要隐藏的项目")?;
                    ensure!(
                        self.packages.catalog.packages.iter().any(|p| p.id == id),
                        "项目不在当前已读取目录中"
                    );
                    if !hidden.iter().any(|value| value.as_str() == Some(id)) {
                        hidden.push(JsonNode::String(id.into()));
                    }
                } else if command == "packages.hide_all" {
                    let ids = self
                        .packages
                        .visible(self.repository.document("packages")?)
                        .into_iter()
                        .map(|package| package.id.clone())
                        .collect::<Vec<_>>();
                    for id in ids {
                        if !hidden
                            .iter()
                            .any(|value| value.as_str() == Some(id.as_str()))
                        {
                            hidden.push(JsonNode::String(id));
                        }
                    }
                } else {
                    hidden.clear();
                }
                let document = packages::preferences_change(
                    self.repository.document("packages")?,
                    &JsonNode::Object(vec![("HiddenPackages".into(), JsonNode::Array(hidden))]),
                )?;
                self.repository.save("packages", document)?;
                self.publish_configuration()?;
                self.finish(id, true, "下载列表偏好已保存");
            }
            "update.check" => {
                ensure!(
                    self.repository
                        .document("update")?
                        .root()
                        .get_ci("UpdateMethod")
                        .is_none_or(|method| method.as_i64() == Some(0)
                            || method
                                .as_str()
                                .is_some_and(|name| name.eq_ignore_ascii_case("GitHub"))),
                    "旧版Server更新接口不发布Rust机控；请在设置选择GitHub"
                );
                let channel = payload
                    .get("channel")
                    .or_else(|| payload.get("update.channel"))
                    .and_then(Value::as_str)
                    .map(|name| {
                        if name.eq_ignore_ascii_case("beta") || name.eq_ignore_ascii_case("preview")
                        {
                            Channel::Beta
                        } else {
                            Channel::Stable
                        }
                    })
                    .unwrap_or_else(|| {
                        if self
                            .repository
                            .document("update")
                            .ok()
                            .and_then(|d| d.root().get_ci("UpdateChannel"))
                            .is_some_and(|s| {
                                s.as_str().is_some_and(|s| s.eq_ignore_ascii_case("beta"))
                                    || s.as_i64() == Some(1)
                            })
                        {
                            Channel::Beta
                        } else {
                            Channel::Stable
                        }
                    });
                self.packages.service.start_update_check(
                    id,
                    env!("CARGO_PKG_VERSION").into(),
                    channel,
                )?;
            }
            _ => bail!("未知目录操作"),
        }
        Ok(())
    }
    pub(super) fn poll_packages(&mut self) -> Result<()> {
        if let Some(id) = self.packages.auto_request {
            if self.take_internal_result(id).is_some() {
                self.packages.auto_request = None;
            }
        }
        for _ in 0..64 {
            let Some(message) = self.packages.service.poll() else {
                break;
            };
            match &message.event {
                Event::Progress {
                    phase,
                    completed,
                    total,
                } => self.value(
                    "packages.progress",
                    json!(format!(
                        "{phase} · {completed} / {}",
                        total.map(|n| n.to_string()).unwrap_or_else(|| "—".into())
                    )),
                ),
                _ => self.packages.terminal = Some(message),
            }
        }
        if !self.packages.service.is_busy() {
            if let Some(message) = self.packages.terminal.take() {
                match message.event {
                    Event::Catalog { catalog } => {
                        let count = catalog.packages.len();
                        let warnings = catalog.warnings.join("；");
                        self.packages.catalog = catalog;
                        self.publish_packages()?;
                        self.value("packages.progress", json!(format!("目录已读取：{count}项")));
                        self.finish(
                            message.request_id,
                            true,
                            format!("已读取{count}个项目。{warnings}"),
                        );
                    }
                    Event::Downloaded {
                        path,
                        sha256,
                        bytes,
                    } => {
                        self.value("packages.progress", json!("下载与校验完成，未执行安装"));
                        self.finish(
                            message.request_id,
                            true,
                            format!("已下载{bytes}字节并核对SHA256 {sha256}：{}", path.display()),
                        );
                    }
                    Event::UpdateChecked { release } => {
                        if !self.read_only {
                            let stamp = crate::time_values::now_ms();
                            if let Err(error) = self.repository.update_known(
                                "update",
                                &[(
                                    &["LastUpdateCheckDateTime"],
                                    JsonNode::String(crate::time_values::iso(stamp)?),
                                )],
                            ) {
                                self.finish(
                                    message.request_id,
                                    false,
                                    format!("更新检查已返回，但检查时间未能保存：{error:#}"),
                                );
                                return Ok(());
                            }
                        }
                        if let Some(release) = release {
                            self.value("update.available", json!(release.newer));
                            self.value("update.release_url", json!(release.url));
                            {
                                let mut state =
                                    self.state.lock().unwrap_or_else(|p| p.into_inner());
                                let mut caps = state.capabilities.as_ref().clone();
                                caps.insert(
                                    "update.open_release".into(),
                                    Capability {
                                        available: true,
                                        reason: String::new(),
                                    },
                                );
                                state.capabilities = Arc::new(caps);
                                state.metadata_revision += 1;
                                state.revision += 1;
                            }
                            if release.newer {
                                self.queue_notification(json!({"type":"UpdateAvailable","title":"机控更新","text":format!("Jiks {} 可供下载；点击通知查看更新设置",release.version)}))?;
                            }
                            self.finish(
                                message.request_id,
                                true,
                                format!(
                                    "机控版本{}{}",
                                    release.version,
                                    if release.newer {
                                        "可供下载"
                                    } else {
                                        "；当前版本无需更新"
                                    }
                                ),
                            );
                        } else {
                            self.finish(
                                message.request_id,
                                true,
                                "尚无符合本机控渠道的公开Rust版本；未将C#版本当成Rust升级",
                            );
                        }
                    }
                    Event::Cancelled => self.finish(
                        message.request_id,
                        false,
                        "目录或下载已实际取消，未执行安装",
                    ),
                    Event::Failed { message: error } => self.finish(
                        message.request_id,
                        false,
                        format!("目录/下载未完成：{error}"),
                    ),
                    Event::Progress { .. } => unreachable!(),
                }
            }
            for id in std::mem::take(&mut self.packages.cancel_waiters) {
                self.finish(id, true, "后台网络请求与临时文件清理已返回");
            }
        }
        if !self.read_only
            && !self.initializing
            && !self.faulted
            && self.shutdown.is_none()
            && !self.packages.service.is_busy()
            && self.packages.auto_request.is_none()
        {
            let ready = self
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .values
                .get("automation.startup_ready")
                .and_then(Value::as_bool)
                == Some(true);
            if ready {
                let root = self.repository.document("update")?.root();
                let frequency = root
                    .get_ci("UpdateCheckFrequency")
                    .map(JsonNode::to_json)
                    .unwrap_or_else(|| "\"PerDay\"".into());
                let hours = match update_hours(root.get_ci("UpdateCheckFrequency")) {
                    Ok(hours) => hours,
                    Err(error) => {
                        self.value(
                            "update.status",
                            json!(format!("未知更新设置保持惰性：{error:#}")),
                        );
                        return Ok(());
                    }
                };
                let last = root
                    .get_ci("LastUpdateCheckDateTime")
                    .and_then(JsonNode::as_str)
                    .and_then(crate::time_values::parse)
                    .unwrap_or(0);
                let now = crate::time_values::now_ms();
                let signature = format!(
                    "{frequency}/{last}/{}/{}",
                    root.get_ci("UpdateChannel")
                        .map(JsonNode::to_json)
                        .unwrap_or_default(),
                    root.get_ci("UpdateMethod")
                        .map(JsonNode::to_json)
                        .unwrap_or_default()
                );
                if hours > 0
                    && now.saturating_sub(last) >= hours * 3_600_000
                    && (self.packages.auto_signature.as_ref() != Some(&signature)
                        || monotonic_ms() >= self.packages.auto_retry_after)
                {
                    self.packages.auto_signature = Some(signature);
                    self.packages.auto_retry_after =
                        monotonic_ms().saturating_add(hours * 3_600_000);
                    let id = self.new_internal_request()?;
                    match self.request_packages(id, "update.check", &json!({})) {
                        Ok(()) => self.packages.auto_request = Some(id),
                        Err(error) => {
                            self.internal_requests.remove(&id);
                            self.value(
                                "update.status",
                                json!(format!("自动更新检查未开始：{error:#}")),
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
fn update_hours(value: Option<&JsonNode>) -> Result<u64> {
    let names = [
        "Never",
        "PerHour",
        "PerThreeHours",
        "PerTwelveHours",
        "PerDay",
        "PerWeek",
        "PerMonth",
    ];
    let index = match value {
        None | Some(JsonNode::Null) => 4,
        Some(v) => v
            .as_i64()
            .and_then(|n| usize::try_from(n).ok())
            .or_else(|| {
                v.as_str()
                    .and_then(|s| names.iter().position(|n| n.eq_ignore_ascii_case(s)))
            })
            .context("未知更新频率；未自动联网")?,
    };
    [0, 1, 3, 12, 24, 168, 720]
        .get(index)
        .copied()
        .context("未知更新频率；未自动联网")
}
#[cfg(test)]
mod tests {
    use super::*;
    fn package(id: &str, title: &str, category: &str, date: &str) -> packages::Package {
        packages::Package {
            id: id.into(),
            source: Source::PcSupport,
            title: title.into(),
            description: "metadata".into(),
            version: "1".into(),
            category: category.into(),
            file_name: "fixture.exe".into(),
            file_size: "1 MB".into(),
            file_size_bytes: Some(1),
            declared_checksum: None,
            release_date: date.into(),
            readme_url: None,
            file_url: "https://download.lenovo.com/fixture.exe".into(),
            availability: Availability::Unknown("fixture".into()),
            reboot: 0,
        }
    }
    #[test]
    fn local_sort_filter_hide_all_operates_on_current_visible_opaque_ids_without_network_or_hardware()
     {
        let mut f = super::super::lifecycle_tests::Fixture::new();
        f.engine.packages.catalog.packages = vec![
            package("\"quoted-A\"", "Zeta", "Audio", "2026-01-02"),
            package("id-B", "Alpha", "Video", "2025-12-31"),
            package("id-C", "Beta", "Audio", "2026-11-03"),
        ];
        f.engine
            .request_packages(
                1,
                "packages.filter",
                &json!({"packages.filter":"audio","packages.sort":"date"}),
            )
            .unwrap();
        let state = f.engine.state.lock().unwrap();
        assert_eq!(
            state.collections["packages"]
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["id-C", "\"quoted-A\""]
        );
        drop(state);
        f.engine
            .request_packages(2, "packages.hide_all", &json!({}))
            .unwrap();
        let hidden = f
            .engine
            .repository
            .document("packages")
            .unwrap()
            .root()
            .get_ci("HiddenPackages")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(hidden.len(), 2);
        assert!(hidden.iter().any(|id| id.as_str() == Some("\"quoted-A\"")));
        assert!(!hidden.iter().any(|id| id.as_str() == Some("id-B")));
        assert!(!f.engine.packages.service.is_busy());
        assert!(f.log.lock().unwrap().is_empty());
    }
    #[test]
    fn update_frequency_preserves_known_schedule_and_unknown_remains_inert() {
        for (index, hours) in [0, 1, 3, 12, 24, 168, 720].into_iter().enumerate() {
            assert_eq!(
                update_hours(Some(&JsonNode::Number(index.to_string()))).unwrap(),
                hours
            );
        }
        assert_eq!(update_hours(None).unwrap(), 24);
        assert!(update_hours(Some(&JsonNode::String("FutureFrequency".into()))).is_err());
    }
}
