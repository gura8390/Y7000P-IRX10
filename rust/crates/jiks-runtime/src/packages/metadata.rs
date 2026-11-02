//! Read-only detection inputs. No constructor performs collection and no
//! external detector, process execution, firmware or user configuration is used.
use super::*;
use jiks_platform::wmi_native::WmiFacade;
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND},
    System::Registry::*,
};
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .as_object()?
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value)
}
fn string(value: Option<&Value>) -> Option<String> {
    json_text(value).filter(|v| !v.is_empty())
}
fn number(value: Option<&Value>) -> Option<u32> {
    value.and_then(|v| {
        v.as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .or_else(|| v.as_str()?.parse().ok())
    })
}
fn capture_rows(
    facade: &WmiFacade,
    key: &str,
    query: &str,
    facts: &mut DetectionFacts,
) -> Option<Vec<Value>> {
    match facade.query::<Value>(query) {
        Ok(rows) if rows.len() <= 8192 => Some(rows),
        Ok(_) => {
            facts
                .errors
                .insert(key.into(), "Metadata row budget exceeded".into());
            None
        }
        Err(error) => {
            facts.errors.insert(key.into(), format!("{error:#}"));
            None
        }
    }
}
pub fn collect_detection_facts() -> DetectionFacts {
    let mut facts = DetectionFacts::default();
    let facade = match WmiFacade::connect("ROOT\\CIMV2") {
        Ok(facade) => facade,
        Err(error) => {
            facts.errors.insert("WMI".into(), format!("{error:#}"));
            return facts;
        }
    };
    if let Some(rows) = capture_rows(
        &facade,
        "os",
        "SELECT Version,BuildNumber,ProductType FROM Win32_OperatingSystem",
        &mut facts,
    ) {
        apply_os(&rows, &mut facts);
    }
    if let Some(rows) = capture_rows(
        &facade,
        "cpu",
        "SELECT AddressWidth FROM Win32_Processor",
        &mut facts,
    ) {
        let values = rows
            .iter()
            .filter_map(|row| number(field(row, "AddressWidth")))
            .collect::<Vec<_>>();
        if let Some(width) = values.first().copied().filter(|width| {
            matches!(width, 32 | 64)
                && values.len() == rows.len()
                && !rows.is_empty()
                && values.iter().all(|v| Some(v) == values.first())
        }) {
            facts.cpu_address_width = Some(width);
        } else {
            facts.errors.insert(
                "cpu".into(),
                "CPU address width missing or inconsistent".into(),
            );
        }
    }
    if let Some(rows) = capture_rows(
        &facade,
        "bios",
        "SELECT SMBIOSBIOSVersion FROM Win32_BIOS",
        &mut facts,
    ) {
        facts.bios_version = rows
            .first()
            .and_then(|row| string(field(row, "SMBIOSBIOSVersion")));
        if facts.bios_version.is_none() {
            facts
                .errors
                .insert("bios".into(), "BIOS version absent".into());
        }
    }
    if let Some(rows) = capture_rows(
        &facade,
        "drivers",
        "SELECT DeviceID,HardWareID,DriverVersion,DriverDate FROM Win32_PnPSignedDriver",
        &mut facts,
    ) {
        facts.drivers = Some(rows.iter().map(driver).collect());
    }
    facts
}
fn apply_os(rows: &[Value], facts: &mut DetectionFacts) {
    facts.os = None;
    facts.windows_build = None;
    if rows.len() != 1 {
        facts
            .errors
            .insert("os".into(), "Expected one local operating system".into());
        return;
    }
    let row = &rows[0];
    facts.windows_build = number(field(row, "BuildNumber"));
    let version = string(field(row, "Version"));
    let desktop = number(field(row, "ProductType")) == Some(1);
    facts.os = match (desktop, version.as_deref(), facts.windows_build) {
        (true, Some(version), Some(build)) if version.starts_with("10.") => {
            Some(if build >= 22000 {
                WindowsVersion::Windows11
            } else {
                WindowsVersion::Windows10
            })
        }
        _ => None,
    };
    if facts.os.is_none() {
        facts.errors.insert(
            "os".into(),
            "Actual client OS version is unknown or outside the supported Windows 10/11 catalog"
                .into(),
        );
    }
}
fn driver(row: &Value) -> DriverFact {
    let date = string(field(row, "DriverDate")).map(|date| {
        if date.len() >= 8 && date.as_bytes()[..8].iter().all(u8::is_ascii_digit) {
            format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..8])
        } else {
            date
        }
    });
    DriverFact {
        device_id: string(field(row, "DeviceID")).unwrap_or_default(),
        hardware_id: string(field(row, "HardWareID")).unwrap_or_default(),
        version: string(field(row, "DriverVersion")),
        date,
    }
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn registry_path(value: &str) -> Result<(HKEY, &str)> {
    ensure!(
        value.len() <= 2048 && !value.contains('\0'),
        "Invalid registry detection path"
    );
    let (hive, path) = value
        .split_once('\\')
        .context("Registry detection path has no hive")?;
    let hive = match hive.to_ascii_uppercase().as_str() {
        "HKEY_LOCAL_MACHINE" | "HKLM" => HKEY_LOCAL_MACHINE,
        "HKEY_CURRENT_USER" | "HKCU" => HKEY_CURRENT_USER,
        _ => bail!("Registry detector hive is outside metadata scope"),
    };
    let first = path.split('\\').next().unwrap_or("");
    ensure!(
        first.eq_ignore_ascii_case("SOFTWARE") || first.eq_ignore_ascii_case("SYSTEM"),
        "Registry detector is outside software/driver metadata scope"
    );
    Ok((hive, path))
}
fn open(value: &str) -> Result<Option<Key>> {
    let (hive, path) = registry_path(value)?;
    let mut handle = std::ptr::null_mut();
    let code = unsafe { RegOpenKeyExW(hive, wide(path).as_ptr(), 0, KEY_QUERY_VALUE, &mut handle) };
    if code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    ensure!(code == 0, "Read-only registry detection failed ({code})");
    Ok(Some(Key(handle)))
}
fn version_value(key: &Key, name: &str) -> Result<Option<String>> {
    ensure!(
        name.len() <= 256
            && !name.contains('\0')
            && (name.is_empty()
                || name.to_ascii_lowercase().contains("version")
                || name.to_ascii_lowercase().contains("release")),
        "Registry value is outside version metadata scope"
    );
    let name = wide(name);
    let (mut kind, mut length) = (0, 0);
    let code = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            std::ptr::null_mut(),
            &mut length,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    ensure!(
        code == 0 && length <= 4096 && length % 2 == 0,
        "Registry version length/type unavailable ({code})"
    );
    ensure!(
        kind == REG_SZ || kind == REG_EXPAND_SZ,
        "Registry version is not a string"
    );
    let mut value = vec![0u16; length as usize / 2];
    let code = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            value.as_mut_ptr().cast(),
            &mut length,
        )
    };
    ensure!(
        code == 0 && length as usize <= value.len() * 2 && length % 2 == 0,
        "Registry version changed during read ({code})"
    );
    let length = value
        .iter()
        .position(|v| *v == 0)
        .unwrap_or(length as usize / 2);
    Ok(Some(String::from_utf16(&value[..length])?))
}
fn registry_requests(node: &Xml, keys: &mut Vec<String>, versions: &mut Vec<(String, String)>) {
    if node.name == "_RegistryKey" {
        if let Some(key) = node.text_at("Key") {
            keys.push(key.into());
        }
    }
    if node.name == "_RegistryKeyValue" {
        if let (Some(key), Some(name)) = (node.text_at("Key"), node.child("KeyName")) {
            versions.push((key.into(), name.text.trim().into()));
        }
    }
    for child in &node.children {
        registry_requests(child, keys, versions);
    }
}
pub fn collect_registry_rule_facts(input: &str, facts: &mut DetectionFacts) -> Result<()> {
    let root = xml(input)?;
    let (mut keys, mut versions) = (Vec::new(), Vec::new());
    registry_requests(&root, &mut keys, &mut versions);
    ensure!(
        keys.len() + versions.len() <= 256,
        "Too many registry detection rules"
    );
    for key in keys {
        let error_key = format!("registry.key:{key}");
        if facts.registry_keys.contains_key(&key) || facts.errors.contains_key(&error_key) {
            continue;
        }
        match open(&key) {
            Ok(handle) => {
                facts.registry_keys.insert(key, handle.is_some());
            }
            Err(error) => {
                facts.errors.insert(error_key, format!("{error:#}"));
            }
        }
    }
    for (key, name) in versions {
        let error_key = format!("registry.version:{key}/{name}");
        if facts
            .registry_versions
            .contains_key(&(key.clone(), name.clone()))
            || facts.errors.contains_key(&error_key)
        {
            continue;
        }
        let result = open(&key).and_then(|handle| {
            handle
                .map(|handle| version_value(&handle, &name))
                .transpose()
                .map(Option::flatten)
        });
        match result {
            Ok(value) => {
                facts.registry_versions.insert((key, name), value);
            }
            Err(error) => {
                facts.errors.insert(error_key, format!("{error:#}"));
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn selected_windows10_catalog_does_not_replace_actual_windows11_facts() {
        struct Mock;
        impl HttpTransport for Mock {
            fn get(
                &mut self,
                request: &HttpRequest,
                _: &AtomicBool,
                body: &mut dyn FnMut(&[u8]) -> Result<()>,
            ) -> Result<HttpHead> {
                let data = if request.url.contains("/catalog/") {
                    r#"<packages><package><location>https://download.lenovo.com/fixture.xml</location><category>Fixture</category></package></packages>"#
                } else {
                    r#"<Package id="fixture" version="1.0"><Title><Desc>Fixture</Desc></Title><ReleaseDate>2026-11-03</ReleaseDate><Files><Installer><File><Name>fixture.exe</Name><Size>1</Size></File></Installer></Files><Dependencies><_OS><OS>win10</OS></_OS></Dependencies></Package>"#
                };
                body(data.as_bytes())?;
                Ok(HttpHead {
                    status: 200,
                    content_length: Some(data.len() as u64),
                    location: None,
                })
            }
        }
        let mut service = PackageService::with_transport(Arc::new(|| Box::new(Mock)));
        service
            .start_catalog(
                7,
                Source::Vantage,
                WindowsVersion::Windows10,
                DetectionFacts {
                    os: Some(WindowsVersion::Windows11),
                    ..Default::default()
                },
            )
            .unwrap();
        let started = Instant::now();
        let mut result = None;
        while service.is_busy() {
            if let Some(message) = service.poll() {
                if let Event::Catalog { catalog } = message.event {
                    result = Some(catalog);
                }
            }
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::yield_now();
        }
        assert!(matches!(
            result.unwrap().packages[0].availability,
            Availability::NotUpdate
        ));
    }
    #[test]
    fn actual_os_build_and_product_type_never_follow_catalog_selection() {
        let mut facts = DetectionFacts::default();
        apply_os(
            &[json!({"Version":"10.0.26100","BuildNumber":"26100","ProductType":1})],
            &mut facts,
        );
        assert_eq!(facts.os, Some(WindowsVersion::Windows11));
        apply_os(
            &[json!({"Version":"10.0.19045","BuildNumber":"19045","ProductType":1})],
            &mut facts,
        );
        assert_eq!(facts.os, Some(WindowsVersion::Windows10));
        apply_os(
            &[json!({"Version":"10.0.26100","BuildNumber":"26100","ProductType":3})],
            &mut facts,
        );
        assert_eq!(facts.os, None);
    }
    #[test]
    fn driver_reducer_preserves_missing_fields_and_converts_wmi_dates() {
        let row = driver(
            &json!({"DeviceID":"PCI\\VEN_FIXTURE","HardWareID":"PCI\\VEN_FIXTURE&DEV_1","DriverVersion":"1.2.3.4","DriverDate":"20251102000000.000000+000"}),
        );
        assert_eq!(row.date.as_deref(), Some("2025-11-02"));
        assert_eq!(driver(&json!({})).version, None);
    }
    #[test]
    fn registry_requests_are_finite_and_do_not_collect_external_commands() {
        let root=xml("<Package><Dependencies><_RegistryKey><Key>HKEY_LOCAL_MACHINE\\SOFTWARE\\Fixture</Key></_RegistryKey><_ExternalDetection rc='0'>secret.exe</_ExternalDetection></Dependencies><DetectInstall><_RegistryKeyValue><Key>HKCU\\SOFTWARE\\Fixture</Key><KeyName>DisplayVersion</KeyName><Version>1.0</Version></_RegistryKeyValue></DetectInstall></Package>").unwrap();
        let (mut keys, mut versions) = (vec![], vec![]);
        registry_requests(&root, &mut keys, &mut versions);
        assert_eq!(keys.len(), 1);
        assert_eq!(versions.len(), 1);
        assert!(registry_path("HKLM\\SAM\\fixture").is_err());
        assert!(registry_path("HKLM\\SOFTWARE\\Fixture").is_ok());
    }
}
