use super::*;
use serde_json::json;
use std::{collections::VecDeque, sync::atomic::AtomicU64};
struct FakeHttp {
    responses: VecDeque<(u32, Vec<u8>, Option<String>)>,
    requests: Vec<String>,
    cancel_after_first: bool,
}
impl FakeHttp {
    fn new(responses: Vec<Vec<u8>>) -> Self {
        Self {
            responses: responses
                .into_iter()
                .map(|body| (200, body, None))
                .collect(),
            requests: Vec::new(),
            cancel_after_first: false,
        }
    }
}
impl HttpTransport for FakeHttp {
    fn get(
        &mut self,
        request: &HttpRequest,
        cancel: &AtomicBool,
        body: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<HttpHead> {
        self.requests.push(request.url.clone());
        let (status, data, location) = self
            .responses
            .pop_front()
            .context("Unexpected fake HTTP request")?;
        if status == 200 {
            for (index, chunk) in data.chunks(4).enumerate() {
                body(chunk)?;
                if self.cancel_after_first && index == 0 {
                    cancel.store(true, Ordering::Release);
                }
            }
        }
        Ok(HttpHead {
            status,
            content_length: Some(data.len() as u64),
            location,
        })
    }
}
static NEXT: AtomicU64 = AtomicU64::new(1);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "JiksPackageFake-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        assert!(
            self.0.starts_with(std::env::temp_dir())
                && self
                    .0
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("JiksPackageFake-")
        );
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn package(bytes: &[u8]) -> Package {
    Package {
        id: "fixture".into(),
        source: Source::PcSupport,
        title: "Test".into(),
        description: String::new(),
        version: "1.0".into(),
        category: "Fixture".into(),
        file_name: "installer.exe".into(),
        file_size: bytes.len().to_string(),
        file_size_bytes: Some(bytes.len() as u64),
        declared_checksum: Some(format!("{:x}", Sha256::digest(bytes))),
        release_date: String::new(),
        readme_url: None,
        file_url: "https://download.lenovo.com/fixture/installer.exe".into(),
        availability: Availability::Unknown("fixture".into()),
        reboot: 0,
    }
}
fn definition() -> PackageDefinition {
    PackageDefinition {
        location: "https://download.lenovo.com/fixture/package.xml".into(),
        category: "Fixture".into(),
    }
}
fn vantage(rules: &str, detect: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><Package id="fixture" version="2.0"><Title><Desc><![CDATA[Driver & display]]></Desc></Title><ReleaseDate>2026-11-02</ReleaseDate><Files><Installer><File><Name>installer.exe</Name><Size>12</Size><CRC>not-a-sha256</CRC></File></Installer><Readme><File><Name>readme.txt</Name></File></Readme></Files><Reboot type="3"/><Dependencies>{rules}</Dependencies><DetectInstall>{detect}</DetectInstall></Package>"#
    )
}
#[test]
fn constructors_and_cancel_without_request_do_not_create_native_work() {
    let service = PackageService::new();
    assert!(!service.is_busy());
    service.cancel();
    let mut service = service;
    assert!(service.poll().is_none());
}
#[test]
fn pc_support_preserves_legacy_quoted_id_filters_os_and_prefers_exe() {
    let data=json!({"body":{"DownloadItems":[{"ID":"DS-FIXTURE","Title":"Driver","Summary":"Driver","SummaryInfo":{"Version":"2.0"},"Category":{"Name":"Audio"},"OperatingSystemKeys":["Windows 11 (64-bit)"],"Files":[{"TypeString":"zip","URL":"https://download.lenovo.com/f.zip","Size":"20"},{"TypeString":"EXE","URL":"https://download.lenovo.com/f.exe","Size":"12","SHA256":"a".repeat(64),"Date":{"Unix":123}},{"TypeString":"txt readme","URL":"https://download.lenovo.com/readme.txt"}]},{"ID":3,"OperatingSystemKeys":["Windows 10"]}]}}).to_string();
    let catalog = parse_pc_support(&data, WindowsVersion::Windows11).unwrap();
    assert_eq!(catalog.packages.len(), 1);
    let package = &catalog.packages[0];
    assert_eq!(package.id, "\"DS-FIXTURE\"");
    assert_eq!(package.file_name, "f.exe");
    assert_eq!(package.file_size_bytes, Some(12));
    assert!(package.description.is_empty());
    assert!(catalog.warnings.is_empty());
    assert!(matches!(package.availability, Availability::Unknown(_)));
}
#[test]
fn vantage_catalog_and_package_decode_xml_entities_and_keep_unknown_external_detection() {
    let cancel = AtomicBool::new(false);
    let mut utf16 = vec![0xff, 0xfe];
    utf16.extend("<packages/>".encode_utf16().flat_map(u16::to_le_bytes));
    let mut http = FakeHttp::new(vec![utf16]);
    assert!(
        parse_vantage_catalog(
            &text(
                &mut http,
                "https://download.lenovo.com/fixture/catalog.xml",
                None,
                &cancel
            )
            .unwrap()
        )
        .unwrap()
        .is_empty()
    );
    let definitions=parse_vantage_catalog("<packages><package><location>https://download.lenovo.com/fixture/package.xml</location><category>Audio &amp; video</category></package></packages>").unwrap();
    assert_eq!(definitions[0].category, "Audio & video");
    let facts = DetectionFacts {
        os: Some(WindowsVersion::Windows11),
        cpu_address_width: Some(64),
        ..Default::default()
    };
    let data = vantage(
        "<And><_OS><OS>win11</OS></_OS><_CPUAddressWidth><AddressWidth>64</AddressWidth></_CPUAddressWidth></And>",
        "<_ExternalDetection rc=\"0\">detector.exe</_ExternalDetection>",
    );
    let package = parse_vantage_package(&data, &definitions[0], &facts).unwrap();
    assert_eq!(package.title, "Driver & display");
    assert_eq!(package.reboot, 3);
    assert!(matches!(package.availability, Availability::Unknown(_)));
    assert_eq!(
        package.file_url,
        "https://download.lenovo.com/fixture/installer.exe"
    );
}
#[test]
fn known_driver_rules_distinguish_missing_inventory_and_update_from_not_update() {
    let data = vantage(
        "<_OS><OS>win11</OS></_OS>",
        "<_Driver><HardwareID>PCI\\VEN_FIXTURE</HardwareID><Version>2.0</Version></_Driver>",
    );
    let mut facts = DetectionFacts {
        os: Some(WindowsVersion::Windows11),
        ..Default::default()
    };
    assert!(matches!(
        parse_vantage_package(&data, &definition(), &facts)
            .unwrap()
            .availability,
        Availability::Unknown(_)
    ));
    facts.drivers = Some(vec![DriverFact {
        device_id: "pci\\ven_fixture\\0".into(),
        hardware_id: "fixture".into(),
        version: Some("1.0".into()),
        date: None,
    }]);
    assert!(matches!(
        parse_vantage_package(&data, &definition(), &facts)
            .unwrap()
            .availability,
        Availability::Update
    ));
    facts.drivers.as_mut().unwrap()[0].version = Some("3.0".into());
    assert!(matches!(
        parse_vantage_package(&data, &definition(), &facts)
            .unwrap()
            .availability,
        Availability::NotUpdate
    ));
}
#[test]
fn xml_dtd_depth_and_untrusted_catalog_locations_fail_before_fetch() {
    assert!(
        parse_vantage_catalog(
            "<!DOCTYPE packages [<!ENTITY e SYSTEM 'file:///fixture'>]><packages/>"
        )
        .is_err()
    );
    let nested = format!("{}{}", "<packages>".repeat(65), "</packages>".repeat(65));
    assert!(parse_vantage_catalog(&nested).is_err());
    assert!(parse_vantage_catalog("<packages><package><location>http://download.lenovo.com/file.xml</location><category>Test</category></package></packages>").is_err());
    for url in [
        "https://download.lenovo.com.evil.test/file",
        "https://user@download.lenovo.com/file",
        "https://download.lenovo.com:80/file",
        "https://download.lenovo.com/\r\nfile",
    ] {
        assert!(trusted_url(url).is_err());
    }
}
#[test]
fn verified_download_is_atomic_preserves_existing_file_and_never_executes_it() {
    let temp = Temp::new();
    std::fs::write(temp.0.join("Test - installer.exe"), b"old").unwrap();
    let bytes = b"fixture-download";
    let mut http = FakeHttp::new(vec![bytes.to_vec()]);
    let outcome = download(
        &mut http,
        &package(bytes),
        &temp.0,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    let Event::Downloaded {
        path,
        sha256,
        bytes: count,
    } = outcome
    else {
        panic!("wrong outcome")
    };
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    assert_eq!(
        std::fs::read(temp.0.join("Test - installer.exe")).unwrap(),
        b"old"
    );
    assert_eq!(sha256, format!("{:x}", Sha256::digest(bytes)));
    assert_eq!(count, bytes.len() as u64);
    assert_eq!(http.requests.len(), 1);
    assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 2);
}
#[test]
fn checksum_failure_and_cancellation_remove_all_pending_files() {
    let temp = Temp::new();
    let bytes = b"fixture-download";
    let mut bad = package(bytes);
    bad.declared_checksum = None;
    let mut http = FakeHttp::new(vec![bytes.to_vec(), "0".repeat(64).into_bytes()]);
    assert!(
        download(
            &mut http,
            &bad,
            &temp.0,
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .is_err()
    );
    assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 0);
    let cancel = AtomicBool::new(false);
    let mut http = FakeHttp::new(vec![bytes.to_vec()]);
    http.cancel_after_first = true;
    assert!(
        download(&mut http, &package(bytes), &temp.0, &cancel, &mut |_| {})
            .unwrap_err()
            .downcast_ref::<Cancelled>()
            .is_some()
    );
    assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 0);
}
#[test]
fn legacy_crc_falls_back_to_sha256_sidecar_but_length_mismatch_never_publishes() {
    let temp = Temp::new();
    let bytes = b"fixture-download";
    let mut package = package(bytes);
    package.declared_checksum = Some("DEADBEEF".into());
    let sha = format!("{:x}  installer.exe\n", Sha256::digest(bytes));
    let mut http = FakeHttp::new(vec![bytes.to_vec(), sha.into_bytes()]);
    assert!(matches!(
        download(
            &mut http,
            &package,
            &temp.0,
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .unwrap(),
        Event::Downloaded { .. }
    ));
    assert_eq!(http.requests.len(), 2);
    package.file_size_bytes = Some(99);
    let mut http = FakeHttp::new(vec![bytes.to_vec()]);
    assert!(
        download(
            &mut http,
            &package,
            &temp.0,
            &AtomicBool::new(false),
            &mut |_| {}
        )
        .is_err()
    );
    assert_eq!(std::fs::read_dir(&temp.0).unwrap().count(), 1);
}
#[test]
fn redirects_are_validated_and_do_not_leak_to_untrusted_hosts() {
    let cancel = AtomicBool::new(false);
    let mut http = FakeHttp {
        responses: vec![(302, vec![], Some("https://evil.test/file".into()))].into(),
        requests: vec![],
        cancel_after_first: false,
    };
    assert!(
        text(
            &mut http,
            "https://download.lenovo.com/fixture",
            None,
            &cancel
        )
        .is_err()
    );
    assert_eq!(http.requests.len(), 1);
}
#[test]
fn github_updates_select_jiks_assets_and_never_offer_the_csharp_release() {
    assert!(release_version("0.3.0").unwrap() > release_version("0.3.0-beta.10").unwrap());
    assert!(release_version("0.3.0-beta.10").unwrap() > release_version("0.3.0-beta.2").unwrap());
    let release = |tag: &str, name: &str, pre: bool| json!({"tag_name":tag,"draft":false,"prerelease":pre,"html_url":format!("https://github.com/{REPOSITORY}/releases/tag/{tag}"),"body":"fixture notes","assets":[{"name":name,"browser_download_url":format!("https://github.com/{REPOSITORY}/releases/download/{tag}/{name}"),"size":12,"digest":format!("sha256:{}","a".repeat(64))}]});
    let data = json!([
        release("v2.34.0.3", "LenovoLegionToolkit.zip", false),
        release("jiks-v0.2.0", "Jiks-win-x64.zip", false),
        release("jiks-v0.3.0-beta", "Jiks-win-x64.zip", true)
    ])
    .to_string();
    let stable = parse_updates(&data, "0.1.0", Channel::Stable)
        .unwrap()
        .unwrap();
    assert_eq!(stable.version, "0.2.0");
    assert!(stable.newer);
    assert_eq!(
        parse_updates(&data, "0.3.0", Channel::Beta)
            .unwrap()
            .unwrap()
            .version,
        "0.3.0-beta"
    );
}
#[test]
fn package_preferences_preserve_unknown_numeric_lexemes_and_keep_unknown_updates_visible() {
    let original =
        SettingsDocument::parse("{\"Future\":1e9999,\"Minus\":-0,\"HiddenPackages\":[]}").unwrap();
    let payload =
        JsonNode::parse("{\"OnlyShowUpdates\":true,\"HiddenPackages\":[\"hidden\"]}").unwrap();
    let changed = preferences_change(&original, &payload).unwrap();
    assert!(changed.to_json().contains("1e9999") && changed.to_json().contains("-0"));
    let mut unknown = package(b"fixture");
    let mut hidden = unknown.clone();
    hidden.id = "hidden".into();
    let mut not = unknown.clone();
    not.id = "not".into();
    not.availability = Availability::NotUpdate;
    unknown.id = "unknown".into();
    let catalog = Catalog {
        packages: vec![unknown, hidden, not],
        warnings: vec![],
    };
    let visible = visible_packages(&catalog, &changed);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, "unknown");
    assert!(
        preferences_change(
            &original,
            &JsonNode::parse("{\"EnableCpuThermalControl\":true}").unwrap()
        )
        .is_err()
    );
}
