//! Explicit, cancellable Lenovo catalogs and verified downloads. Construction
//! performs no I/O. Packages are never installed, executed or externally detected.
use anyhow::{Context, Result, bail, ensure};
use jiks_core::settings::{JsonNode, SettingsDocument};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
mod metadata;
mod native_http;
pub use metadata::{collect_detection_facts, collect_registry_rule_facts};
pub use native_http::NativeHttp;

const TEXT_LIMIT: usize = 16 * 1024 * 1024;
const DOWNLOAD_LIMIT: u64 = 8 * 1024 * 1024 * 1024;
pub const REPOSITORY: &str = "gura8390/Y7000P-IRX10";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[serde(alias = "PCSupport", alias = "pcsupport")]
    PcSupport,
    #[serde(alias = "Vantage")]
    Vantage,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowsVersion {
    #[serde(alias = "win11", alias = "windows11", alias = "Windows 11")]
    Windows11,
    #[serde(alias = "win10", alias = "windows10", alias = "Windows 10")]
    Windows10,
}
impl WindowsVersion {
    fn catalog(self) -> &'static str {
        match self {
            Self::Windows11 => "win11",
            Self::Windows10 => "win10",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Windows11 => "Windows 11",
            Self::Windows10 => "Windows 10",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Stable,
    #[serde(alias = "preview")]
    Beta,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum Availability {
    Update,
    NotUpdate,
    Unknown(String),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub id: String,
    pub source: Source,
    pub title: String,
    pub description: String,
    pub version: String,
    pub category: String,
    pub file_name: String,
    pub file_size: String,
    pub file_size_bytes: Option<u64>,
    pub declared_checksum: Option<String>,
    pub release_date: String,
    pub readme_url: Option<String>,
    pub file_url: String,
    pub availability: Availability,
    pub reboot: u32,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub packages: Vec<Package>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Default)]
pub struct DetectionFacts {
    pub os: Option<WindowsVersion>,
    pub windows_build: Option<u32>,
    pub cpu_address_width: Option<u32>,
    pub bios_version: Option<String>,
    /// None means inventory was not collected; an empty known list is distinct.
    pub drivers: Option<Vec<DriverFact>>,
    pub registry_keys: BTreeMap<String, bool>,
    /// A known key with None means its value is absent; missing map key is unknown.
    pub registry_versions: BTreeMap<(String, String), Option<String>>,
    pub errors: BTreeMap<String, String>,
}
#[derive(Debug, Clone)]
pub struct DriverFact {
    pub device_id: String,
    pub hardware_id: String,
    pub version: Option<String>,
    pub date: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRelease {
    pub version: String,
    pub tag: String,
    pub url: String,
    pub notes: String,
    pub assets: Vec<UpdateAsset>,
    pub newer: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
    pub sha256: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Progress {
        phase: String,
        completed: u64,
        total: Option<u64>,
    },
    Catalog {
        catalog: Catalog,
    },
    Downloaded {
        path: PathBuf,
        sha256: String,
        bytes: u64,
    },
    UpdateChecked {
        release: Option<UpdateRelease>,
    },
    Cancelled,
    Failed {
        message: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub request_id: u64,
    pub event: Event,
}

#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Package request cancelled")
    }
}
impl std::error::Error for Cancelled {}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Err(Cancelled.into());
    }
    Ok(())
}
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub url: String,
    pub referer: Option<String>,
}
#[derive(Debug, Clone)]
pub struct HttpHead {
    pub status: u32,
    pub content_length: Option<u64>,
    pub location: Option<String>,
}
/// The callback is streaming and may return cancellation/size errors. Implementors
/// must propagate it and must not silently retry an already-streamed body.
pub trait HttpTransport: Send {
    fn get(
        &mut self,
        request: &HttpRequest,
        cancel: &AtomicBool,
        body: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<HttpHead>;
}
type TransportFactory = Arc<dyn Fn() -> Box<dyn HttpTransport> + Send + Sync>;
struct Worker {
    join: JoinHandle<()>,
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Message>,
}
pub struct PackageService {
    factory: TransportFactory,
    worker: Option<Worker>,
}
impl Default for PackageService {
    fn default() -> Self {
        Self::new()
    }
}
impl PackageService {
    pub fn new() -> Self {
        Self::with_transport(Arc::new(|| Box::new(NativeHttp::new())))
    }
    pub fn with_transport(factory: TransportFactory) -> Self {
        Self {
            factory,
            worker: None,
        }
    }
    pub fn is_busy(&self) -> bool {
        self.worker.is_some()
    }
    pub fn cancel(&self) {
        if let Some(worker) = &self.worker {
            worker.cancel.store(true, Ordering::Release);
        }
    }
    /// A terminal event does not release the worker until its thread really exits.
    pub fn poll(&mut self) -> Option<Message> {
        let worker = self.worker.as_mut()?;
        let message = worker.receiver.try_recv().ok();
        if message.is_none() && worker.join.is_finished() {
            let worker = self.worker.take().unwrap();
            let _ = worker.join.join();
        }
        message
    }
    fn start(
        &mut self,
        id: u64,
        run: impl FnOnce(&mut dyn HttpTransport, &AtomicBool, &mut dyn FnMut(Event)) -> Result<Event>
        + Send
        + 'static,
    ) -> Result<()> {
        ensure!(
            self.worker.is_none(),
            "Previous package request still owns its worker"
        );
        let (send, receiver) = mpsc::sync_channel(32);
        let cancel = Arc::new(AtomicBool::new(false));
        let requested = cancel.clone();
        let factory = self.factory.clone();
        let join = thread::Builder::new()
            .name("jiks-explicit-package".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut http = factory();
                    let mut progress = |event| {
                        let _ = send.try_send(Message {
                            request_id: id,
                            event,
                        });
                    };
                    check_cancel(&requested)?;
                    run(http.as_mut(), &requested, &mut progress)
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Package worker panicked")));
                let event = match outcome {
                    Ok(event) => event,
                    Err(error) if error.downcast_ref::<Cancelled>().is_some() => Event::Cancelled,
                    Err(error) => Event::Failed {
                        message: format!("{error:#}"),
                    },
                };
                // Terminal admission is reliable; Drop of the receiver unblocks it.
                let _ = send.send(Message {
                    request_id: id,
                    event,
                });
            })?;
        self.worker = Some(Worker {
            join,
            cancel,
            receiver,
        });
        Ok(())
    }
    pub fn start_catalog(
        &mut self,
        id: u64,
        source: Source,
        os: WindowsVersion,
        facts: DetectionFacts,
    ) -> Result<()> {
        self.start(id, move |http, cancel, progress| {
            Ok(Event::Catalog {
                catalog: fetch_catalog(http, source, os, &facts, cancel, progress)?,
            })
        })
    }
    /// Collect actual local metadata inside the explicit catalog worker. The OS
    /// selected for catalog browsing never becomes a claim about this machine.
    pub fn start_catalog_local(
        &mut self,
        id: u64,
        source: Source,
        catalog_os: WindowsVersion,
    ) -> Result<()> {
        self.start(id, move |http, cancel, progress| {
            let mut facts = if source == Source::Vantage {
                collect_detection_facts()
            } else {
                DetectionFacts::default()
            };
            let mut catalog =
                fetch_catalog_local(http, source, catalog_os, &mut facts, cancel, progress, true)?;
            catalog.warnings.extend(
                facts
                    .errors
                    .iter()
                    .map(|(key, error)| format!("Detection metadata {key}: {error}")),
            );
            Ok(Event::Catalog { catalog })
        })
    }
    pub fn start_download(&mut self, id: u64, package: Package, directory: PathBuf) -> Result<()> {
        validate_package(&package)?;
        ensure!(
            directory.is_absolute(),
            "Choose an absolute download directory"
        );
        self.start(id, move |http, cancel, progress| {
            download(http, &package, &directory, cancel, progress)
        })
    }
    pub fn start_update_check(&mut self, id: u64, current: String, channel: Channel) -> Result<()> {
        release_version(&current).context("Invalid current Jiks version")?;
        self.start(id, move |http, cancel, _| {
            let data = text(
                http,
                &format!("https://api.github.com/repos/{REPOSITORY}/releases?per_page=100"),
                None,
                cancel,
            )?;
            Ok(Event::UpdateChecked {
                release: parse_updates(&data, &current, channel)?,
            })
        })
    }
}
impl Drop for PackageService {
    fn drop(&mut self) {
        self.cancel(); /* Never block the UI while native HTTP unwinds. */
    }
}

#[derive(Debug, Clone)]
pub(super) struct Url {
    pub host: String,
    pub path: String,
}
pub(super) fn parse_url(url: &str) -> Result<Url> {
    ensure!(
        url.len() <= 8192 && !url.chars().any(|c| c.is_control() || c == '\\'),
        "Invalid HTTPS URL"
    );
    let rest = url
        .strip_prefix("https://")
        .context("Only HTTPS downloads are allowed")?;
    let (authority, path) = rest
        .split_once('/')
        .map_or((rest, "/"), |(a, _)| (a, &rest[a.len()..]));
    ensure!(
        !authority.is_empty()
            && authority
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
        "Invalid HTTPS authority"
    );
    ensure!(
        !path.contains('#'),
        "URL fragments are not sent to package endpoints"
    );
    Ok(Url {
        host: authority.to_ascii_lowercase(),
        path: path.into(),
    })
}
fn trusted_url(url: &str) -> Result<Url> {
    let parsed = parse_url(url)?;
    ensure!(
        parsed.host == "lenovo.com"
            || parsed.host.ends_with(".lenovo.com")
            || parsed.host == "api.github.com",
        "URL is outside the configured catalog services"
    );
    Ok(parsed)
}
fn request(
    http: &mut dyn HttpTransport,
    url: &str,
    referer: Option<&str>,
    cancel: &AtomicBool,
    body: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<HttpHead> {
    let mut url = url.to_owned();
    for _ in 0..6 {
        check_cancel(cancel)?;
        trusted_url(&url)?;
        let head = http.get(
            &HttpRequest {
                url: url.clone(),
                referer: referer.map(str::to_owned),
            },
            cancel,
            body,
        )?;
        if [301, 302, 303, 307, 308].contains(&head.status) {
            let location = head.location.context("HTTPS redirect omitted Location")?;
            url = if location.starts_with('/') && !location.starts_with("//") {
                format!("https://{}{}", parse_url(&url)?.host, location)
            } else {
                location
            };
            continue;
        }
        ensure!(head.status == 200, "Catalog HTTP status {}", head.status);
        return Ok(head);
    }
    bail!("Too many catalog redirects")
}
fn text(
    http: &mut dyn HttpTransport,
    url: &str,
    referer: Option<&str>,
    cancel: &AtomicBool,
) -> Result<String> {
    let mut bytes = Vec::new();
    let started = Instant::now();
    request(http, url, referer, cancel, &mut |chunk| {
        check_cancel(cancel)?;
        ensure!(
            started.elapsed() < Duration::from_secs(120),
            "Catalog request deadline exceeded"
        );
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= TEXT_LIMIT,
            "Catalog exceeds size budget"
        );
        bytes.extend_from_slice(chunk);
        Ok(())
    })?;
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        ensure!(bytes.len() % 2 == 0, "Truncated UTF16 catalog");
        let little = bytes[0] == 0xff;
        let units = bytes[2..]
            .chunks_exact(2)
            .map(|pair| {
                if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect::<Vec<_>>();
        return Ok(String::from_utf16(&units)?);
    }
    Ok(String::from_utf8(bytes)?
        .trim_start_matches('\u{feff}')
        .to_owned())
}
fn json_text(value: Option<&Value>) -> Option<String> {
    value.and_then(|v| match v {
        Value::String(v) => Some(v.clone()),
        Value::Number(v) => Some(v.to_string()),
        _ => None,
    })
}
fn required_json(value: Option<&Value>, name: &str) -> Result<String> {
    json_text(value)
        .filter(|v| !v.trim().is_empty())
        .with_context(|| format!("Package {name} absent"))
}
fn checksum(value: &str) -> Option<String> {
    let value = value.trim();
    (value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}
fn file_name(url: &str) -> Result<String> {
    let parsed = parse_url(url)?;
    let name = parsed
        .path
        .split('?')
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");
    ensure!(!name.is_empty(), "Download URL has no filename");
    safe_name(name)
}
fn safe_name(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && name.len() <= 220
            && !name
                .chars()
                .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
            && !name.ends_with(['.', ' '])
            && name != "."
            && name != "..",
        "Invalid package filename"
    );
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    ensure!(
        ![
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"
        ]
        .contains(&stem.as_str()),
        "Reserved package filename"
    );
    Ok(name.into())
}
fn validate_package(package: &Package) -> Result<()> {
    let url = trusted_url(&package.file_url)?;
    ensure!(
        url.host.ends_with(".lenovo.com") || url.host == "lenovo.com",
        "Package download must remain on Lenovo HTTPS"
    );
    safe_name(&package.file_name)?;
    ensure!(
        package.file_size_bytes.is_none_or(|v| v <= DOWNLOAD_LIMIT),
        "Package exceeds download budget"
    );
    Ok(())
}

pub fn parse_pc_support(input: &str, os: WindowsVersion) -> Result<Catalog> {
    ensure!(input.len() <= TEXT_LIMIT, "Catalog exceeds size budget");
    let root: Value = serde_json::from_str(input)?;
    let items = root
        .pointer("/body/DownloadItems")
        .and_then(Value::as_array)
        .context("PCSupport response has no DownloadItems catalog")?;
    ensure!(items.len() <= 5000, "Too many packages");
    let mut out = Catalog::default();
    for (index, item) in items.iter().enumerate() {
        let compatible = item
            .get("OperatingSystemKeys")
            .and_then(Value::as_array)
            .is_none_or(|keys| {
                keys.is_empty()
                    || keys.iter().filter_map(Value::as_str).any(|v| {
                        v.to_ascii_lowercase()
                            .starts_with(&os.label().to_ascii_lowercase())
                    })
            });
        if !compatible {
            continue;
        }
        let package = (|| -> Result<Package> {
            let files = item
                .get("Files")
                .and_then(Value::as_array)
                .context("Files absent")?;
            let typed = |kind: &str| {
                files.iter().find(|f| {
                    f.get("TypeString")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.eq_ignore_ascii_case(kind))
                })
            };
            let main = typed("exe")
                .or_else(|| typed("zip"))
                .or_else(|| files.first())
                .context("Installer file absent")?;
            let title = required_json(item.get("Title"), "title")?;
            let description = json_text(item.get("Summary")).unwrap_or_default();
            let url = required_json(main.get("URL"), "URL")?;
            let size = required_json(main.get("Size"), "size")?;
            let package = Package {
                id: item.get("ID").context("Package ID absent")?.to_string(),
                source: Source::PcSupport,
                title: title.clone(),
                description: if description == title {
                    String::new()
                } else {
                    description
                },
                version: required_json(item.pointer("/SummaryInfo/Version"), "version")?,
                category: required_json(item.pointer("/Category/Name"), "category")?,
                file_name: file_name(&url)?,
                file_size_bytes: size.parse().ok(),
                file_size: size,
                declared_checksum: json_text(main.get("SHA256")),
                release_date: json_text(main.pointer("/Date/Unix")).unwrap_or_default(),
                readme_url: typed("txt readme")
                    .or_else(|| typed("html"))
                    .and_then(|f| json_text(f.get("URL"))),
                file_url: url,
                availability: Availability::Unknown(
                    "PCSupport catalog does not include installed-version detection".into(),
                ),
                reboot: 0,
            };
            validate_package(&package)?;
            Ok(package)
        })();
        match package {
            Ok(package) => out.packages.push(package),
            Err(error) => out
                .warnings
                .push(format!("PCSupport item {}: {error:#}", index + 1)),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
struct Xml {
    name: String,
    attributes: BTreeMap<String, String>,
    text: String,
    children: Vec<Xml>,
}
impl Xml {
    fn child(&self, name: &str) -> Option<&Xml> {
        self.children.iter().find(|node| node.name == name)
    }
    fn at(&self, path: &str) -> Option<&Xml> {
        path.split('/')
            .try_fold(self, |node, name| node.child(name))
    }
    fn text_at(&self, path: &str) -> Option<&str> {
        self.at(path)
            .map(|node| node.text.trim())
            .filter(|v| !v.is_empty())
    }
    fn required(&self, path: &str) -> Result<String> {
        self.text_at(path)
            .map(str::to_owned)
            .with_context(|| format!("Package XML {path} absent"))
    }
}
fn xml(input: &str) -> Result<Xml> {
    use quick_xml::{Reader, events::Event as E};
    ensure!(input.len() <= TEXT_LIMIT, "XML exceeds size budget");
    let mut reader = Reader::from_str(input);
    let mut stack: Vec<Xml> = Vec::new();
    let mut root = None;
    let mut count = 0;
    loop {
        match reader.read_event()? {
            E::Start(element) | E::Empty(element) => {
                count += 1;
                ensure!(
                    count <= 50000 && stack.len() < 64,
                    "XML structure exceeds budget"
                );
                let mut node = Xml {
                    name: String::from_utf8(element.name().as_ref().to_vec())?,
                    ..Default::default()
                };
                for attribute in element.attributes() {
                    let attribute = attribute?;
                    let key = String::from_utf8(attribute.key.as_ref().to_vec())?;
                    ensure!(
                        !node.attributes.contains_key(&key),
                        "Duplicate XML attribute"
                    );
                    node.attributes.insert(
                        key,
                        attribute
                            .decode_and_unescape_value(reader.decoder())?
                            .into_owned(),
                    );
                }
                // Reader reports empty tags independently; inspect the just-consumed
                // closing delimiter to avoid allocating a second parser pass.
                let end = reader.buffer_position() as usize;
                let empty = input.as_bytes().get(end.saturating_sub(2)..end) == Some(b"/>");
                if empty {
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(node);
                    } else {
                        ensure!(root.is_none(), "Multiple XML roots");
                        root = Some(node);
                    }
                } else {
                    stack.push(node);
                }
            }
            E::End(_) => {
                let node = stack.pop().context("Unexpected XML closing tag")?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    ensure!(root.is_none(), "Multiple XML roots");
                    root = Some(node);
                }
            }
            E::Text(text) => {
                let decoded = text.decode()?;
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&quick_xml::escape::unescape(&decoded)?);
                } else {
                    ensure!(decoded.trim().is_empty(), "Text outside XML root");
                }
            }
            E::CData(text) => {
                stack
                    .last_mut()
                    .context("CDATA outside XML root")?
                    .text
                    .push_str(&text.decode()?);
            }
            E::GeneralRef(reference) => {
                let reference = reference.decode()?;
                let escaped = format!("&{reference};");
                stack
                    .last_mut()
                    .context("Reference outside XML root")?
                    .text
                    .push_str(&quick_xml::escape::unescape(&escaped)?);
            }
            E::DocType(_) => bail!("External entities and DTDs are not accepted"),
            E::Eof => break,
            _ => {}
        }
    }
    ensure!(stack.is_empty(), "Unclosed XML elements");
    root.context("XML root absent")
}
#[derive(Debug, Clone)]
pub struct PackageDefinition {
    pub location: String,
    pub category: String,
}
pub fn parse_vantage_catalog(input: &str) -> Result<Vec<PackageDefinition>> {
    let root = xml(input)?;
    ensure!(
        root.name == "packages",
        "Vantage catalog root is not packages"
    );
    let mut out = Vec::new();
    for node in root.children.iter().filter(|node| node.name == "package") {
        if let (Some(location), Some(category)) =
            (node.text_at("location"), node.text_at("category"))
        {
            trusted_url(location)?;
            out.push(PackageDefinition {
                location: location.into(),
                category: category.into(),
            });
        }
    }
    ensure!(out.len() <= 2000, "Too many Vantage definitions");
    Ok(out)
}
fn joined(base: &str, name: &str) -> Result<String> {
    safe_name(name)?;
    let base = base
        .rsplit_once('/')
        .context("Invalid package definition URL")?
        .0;
    let value = format!("{base}/{name}");
    trusted_url(&value)?;
    Ok(value)
}
pub fn parse_vantage_package(
    input: &str,
    definition: &PackageDefinition,
    facts: &DetectionFacts,
) -> Result<Package> {
    let root = xml(input)?;
    ensure!(root.name == "Package", "Unexpected Vantage package root");
    let name = root.required("Files/Installer/File/Name")?;
    let size = root.required("Files/Installer/File/Size")?;
    let size_bytes: u64 = size.parse()?;
    let dependencies = root
        .child("Dependencies")
        .map(|node| evaluate_children(node, facts, false))
        .unwrap_or(Truth::Known(false));
    let detect = root
        .child("DetectInstall")
        .filter(|node| !node.children.is_empty())
        .map(|node| evaluate_children(node, facts, true))
        .unwrap_or(Truth::Known(true));
    let availability=match dependencies.and(detect) {Truth::Known(true)=>Availability::Update,Truth::Known(false)=>Availability::NotUpdate,Truth::Unknown=>Availability::Unknown("Installed-state rule requires unavailable metadata or an external detector; no detector was executed".into())};
    let package = Package {
        id: root
            .attributes
            .get("id")
            .cloned()
            .context("Package id absent")?,
        source: Source::Vantage,
        title: root.required("Title/Desc")?,
        description: String::new(),
        version: root
            .attributes
            .get("version")
            .cloned()
            .context("Package version absent")?,
        category: definition.category.clone(),
        file_name: safe_name(&name)?,
        file_size: format!("{:.2} MB", size_bytes as f64 / 1048576.0),
        file_size_bytes: Some(size_bytes),
        declared_checksum: root.text_at("Files/Installer/File/CRC").map(str::to_owned),
        release_date: root.required("ReleaseDate")?,
        readme_url: root
            .text_at("Files/Readme/File/Name")
            .map(|name| joined(&definition.location, name))
            .transpose()?,
        file_url: joined(&definition.location, &name)?,
        availability,
        reboot: root
            .child("Reboot")
            .and_then(|node| node.attributes.get("type"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    };
    validate_package(&package)?;
    Ok(package)
}

#[derive(Clone, Copy)]
enum Truth {
    Known(bool),
    Unknown,
}
impl Truth {
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Known(false), _) | (_, Self::Known(false)) => Self::Known(false),
            (Self::Known(true), Self::Known(true)) => Self::Known(true),
            _ => Self::Unknown,
        }
    }
    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Known(true), _) | (_, Self::Known(true)) => Self::Known(true),
            (Self::Known(false), Self::Known(false)) => Self::Known(false),
            _ => Self::Unknown,
        }
    }
    fn not(self) -> Self {
        match self {
            Self::Known(value) => Self::Known(!value),
            Self::Unknown => Self::Unknown,
        }
    }
}
fn evaluate_children(node: &Xml, facts: &DetectionFacts, install: bool) -> Truth {
    node.children.iter().fold(Truth::Known(true), |all, child| {
        all.and(evaluate(child, facts, install))
    })
}
fn version(raw: &str) -> Option<Vec<u32>> {
    let raw = raw
        .trim()
        .trim_start_matches(['v', 'V'])
        .split('-')
        .next()?;
    let values = raw
        .split('.')
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    (values.len() >= 2 && values.len() <= 4).then(|| {
        let mut values = values;
        values.resize(4, 0);
        values
    })
}
fn numeric_version(raw: &str) -> Option<Vec<u32>> {
    version(
        &raw.chars()
            .filter(|c| c.is_ascii_digit() || *c == '.')
            .collect::<String>(),
    )
}
fn evaluate(node: &Xml, facts: &DetectionFacts, install: bool) -> Truth {
    let known = Truth::Known;
    match node.name.as_str() {
        "And" => evaluate_children(node, facts, install),
        "Or" => node.children.iter().fold(known(false), |all, child| {
            all.or(evaluate(child, facts, install))
        }),
        "Not" if node.children.len() == 1 => evaluate(&node.children[0], facts, install).not(),
        "_OS" => facts
            .os
            .map(|os| {
                known(
                    node.children
                        .iter()
                        .filter(|c| c.name == "OS")
                        .any(|c| c.text.to_ascii_lowercase().starts_with(os.catalog())),
                )
            })
            .unwrap_or(Truth::Unknown),
        "_CPUAddressWidth" => node
            .text_at("AddressWidth")
            .and_then(|v| v.parse::<u32>().ok())
            .zip(facts.cpu_address_width)
            .map(|(wanted, actual)| known(wanted == actual))
            .unwrap_or(Truth::Unknown),
        "_WindowsBuildVersion" => node
            .text_at("AddressWidth")
            .or_else(|| node.text_at("Version"))
            .and_then(|v| v.parse::<u32>().ok())
            .zip(facts.windows_build)
            .map(|(wanted, actual)| known(wanted <= actual))
            .unwrap_or(Truth::Unknown),
        "_PnPID" => facts
            .drivers
            .as_ref()
            .map(|drivers| {
                known(drivers.iter().any(|d| {
                    starts_ci(&d.device_id, node.text.trim())
                        || starts_ci(&d.hardware_id, node.text.trim())
                }))
            })
            .unwrap_or(Truth::Unknown),
        "_Driver" => {
            let Some(drivers) = &facts.drivers else {
                return Truth::Unknown;
            };
            let matching = drivers.iter().find(|driver| {
                node.children
                    .iter()
                    .filter(|c| c.name == "HardwareID")
                    .any(|id| {
                        starts_ci(&driver.device_id, id.text.trim())
                            || starts_ci(&driver.hardware_id, id.text.trim())
                    })
            });
            let Some(driver) = matching else {
                return known(false);
            };
            let newer = node
                .text_at("Version")
                .and_then(numeric_version)
                .zip(driver.version.as_deref().and_then(numeric_version))
                .map(|(target, actual)| target > actual);
            let Some(mut newer) = newer else {
                return Truth::Unknown;
            };
            if let (Some(target), Some(actual)) = (node.text_at("Date"), driver.date.as_deref()) {
                if date_key(target)
                    .zip(date_key(actual))
                    .is_some_and(|(target, actual)| target < actual)
                {
                    newer = false;
                }
            }
            known(if install { newer } else { !newer })
        }
        "_Bios" => {
            let Some(current) = facts.bios_version.as_deref().and_then(bios) else {
                return Truth::Unknown;
            };
            let levels = node
                .children
                .iter()
                .filter(|n| n.name == "Level")
                .map(|n| bios(n.text.trim()))
                .collect::<Option<Vec<_>>>();
            let Some(levels) = levels.filter(|v| !v.is_empty()) else {
                return Truth::Unknown;
            };
            known(if install {
                levels
                    .iter()
                    .all(|level| level.0 == current.0 && level.1 > current.1)
            } else {
                levels.contains(&current)
            })
        }
        "_RegistryKey" => node
            .text_at("Key")
            .and_then(|key| facts.registry_keys.get(key))
            .copied()
            .map(known)
            .unwrap_or(Truth::Unknown),
        "_RegistryKeyValue" => {
            let (Some(key), Some(name), Some(wanted)) = (
                node.text_at("Key"),
                node.text_at("KeyName"),
                node.text_at("Version").and_then(numeric_version),
            ) else {
                return Truth::Unknown;
            };
            match facts.registry_versions.get(&(key.into(), name.into())) {
                None => Truth::Unknown,
                Some(None) => known(!install),
                Some(Some(actual)) => {
                    numeric_version(actual.split_whitespace().next().unwrap_or(""))
                        .map(|actual| {
                            known(if install {
                                wanted > actual
                            } else {
                                wanted <= actual
                            })
                        })
                        .unwrap_or(Truth::Unknown)
                }
            }
        }
        _ => Truth::Unknown,
    }
}
fn starts_ci(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| jiks_core::automation::ordinal_ignore_case(head, prefix))
}
fn bios(value: &str) -> Option<(String, u32)> {
    let prefix = value.get(..4)?.to_owned();
    if !prefix
        .bytes()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return None;
    }
    let digits = value
        .as_bytes()
        .windows(2)
        .find(|pair| pair.iter().all(u8::is_ascii_digit))?;
    Some((prefix, std::str::from_utf8(digits).ok()?.parse().ok()?))
}
fn date_key(raw: &str) -> Option<(u32, u32, u32)> {
    let split = raw.split(['-', '/']).collect::<Vec<_>>();
    if split.len() != 3 {
        return None;
    }
    let a = split[0].parse().ok()?;
    let b = split[1].parse().ok()?;
    let c = split[2].parse().ok()?;
    Some(if a > 31 { (a, b, c) } else { (c, a, b) })
}
fn fetch_catalog(
    http: &mut dyn HttpTransport,
    source: Source,
    os: WindowsVersion,
    facts: &DetectionFacts,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Event),
) -> Result<Catalog> {
    fetch_catalog_local(
        http,
        source,
        os,
        &mut facts.clone(),
        cancel,
        progress,
        false,
    )
}
fn fetch_catalog_local(
    http: &mut dyn HttpTransport,
    source: Source,
    os: WindowsVersion,
    facts: &mut DetectionFacts,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Event),
    local_metadata: bool,
) -> Result<Catalog> {
    match source {
        Source::PcSupport => parse_pc_support(
            &text(
                http,
                "https://pcsupport.lenovo.com/us/en/api/v4/downloads/drivers?productId=83NN",
                Some("https://pcsupport.lenovo.com/"),
                cancel,
            )?,
            os,
        ),
        Source::Vantage => {
            let definitions = parse_vantage_catalog(&text(
                http,
                &format!(
                    "https://download.lenovo.com/catalog/83NN_{}.xml",
                    os.catalog()
                ),
                None,
                cancel,
            )?)?;
            let total = definitions.len() as u64;
            let mut catalog = Catalog::default();
            for (i, definition) in definitions.into_iter().enumerate() {
                check_cancel(cancel)?;
                match text(http, &definition.location, None, cancel).and_then(|data| {
                    if local_metadata {
                        collect_registry_rule_facts(&data, facts)?;
                    }
                    parse_vantage_package(&data, &definition, facts)
                }) {
                    Ok(package) => catalog.packages.push(package),
                    Err(error) if error.downcast_ref::<Cancelled>().is_some() => return Err(error),
                    Err(error) => catalog
                        .warnings
                        .push(format!("Vantage definition {}: {error:#}", i + 1)),
                }
                progress(Event::Progress {
                    phase: "catalog".into(),
                    completed: i as u64 + 1,
                    total: Some(total),
                });
            }
            Ok(catalog)
        }
    }
}

struct PendingFile(PathBuf);
impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
fn download(
    http: &mut dyn HttpTransport,
    package: &Package,
    directory: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Event),
) -> Result<Event> {
    validate_package(package)?;
    check_cancel(cancel)?;
    ensure!(
        directory.is_absolute(),
        "Choose an absolute download directory"
    );
    std::fs::create_dir_all(directory)?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let pending = PendingFile(directory.join(format!(
        ".jiks-download-{}-{nonce}.pending",
        std::process::id()
    )));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending.0)?;
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let started = Instant::now();
    request(http, &package.file_url, None, cancel, &mut |chunk| {
        check_cancel(cancel)?;
        ensure!(
            started.elapsed() < Duration::from_secs(3600),
            "Download deadline exceeded"
        );
        count = count
            .checked_add(chunk.len() as u64)
            .context("Download byte count overflow")?;
        ensure!(count <= DOWNLOAD_LIMIT, "Download exceeds byte budget");
        file.write_all(chunk)?;
        digest.update(chunk);
        progress(Event::Progress {
            phase: "download".into(),
            completed: count,
            total: package.file_size_bytes,
        });
        Ok(())
    })?;
    if let Some(size) = package.file_size_bytes {
        ensure!(count == size, "Downloaded length differs from catalog");
    }
    let hash = format!("{:x}", digest.finalize());
    let catalog_match = package
        .declared_checksum
        .as_deref()
        .and_then(checksum)
        .is_some_and(|value| value == hash);
    if !catalog_match {
        let external = text(http, &format!("{}.sha256", package.file_url), None, cancel)?;
        let expected = external
            .split_whitespace()
            .next()
            .and_then(checksum)
            .context("External SHA256 checksum absent or malformed")?;
        ensure!(expected == hash, "Downloaded SHA256 checksum mismatch");
    }
    file.flush()?;
    file.sync_all()?;
    drop(file);
    check_cancel(cancel)?;
    let stem = package
        .title
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(80)
        .collect::<String>()
        .trim()
        .trim_end_matches('.')
        .to_owned();
    let stem = if stem.is_empty() { "Lenovo" } else { &stem };
    let mut published = None;
    for index in 0..1000 {
        let suffix = if index == 0 {
            String::new()
        } else {
            format!(" ({index})")
        };
        let name = format!("{stem}{suffix} - {}", package.file_name);
        safe_name(&name)?;
        let path = directory.join(name);
        match native_http::publish_file(&pending.0, &path) {
            Ok(()) => {
                published = Some(path);
                break;
            }
            Err(error) if error.raw_os_error() == Some(183) || error.raw_os_error() == Some(80) => {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let path = published.context("No available download filename")?;
    Ok(Event::Downloaded {
        path,
        sha256: hash,
        bytes: count,
    })
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ReleaseVersion {
    numbers: Vec<u32>,
    pre: Option<String>,
}
fn release_version(raw: &str) -> Option<ReleaseVersion> {
    let raw = raw
        .trim()
        .trim_start_matches(['v', 'V'])
        .split('+')
        .next()?;
    let (number, pre) = raw
        .split_once('-')
        .map_or((raw, None), |(number, pre)| (number, Some(pre)));
    if pre.is_some_and(|pre| {
        pre.split('.').any(|part| {
            part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    }) {
        return None;
    }
    Some(ReleaseVersion {
        numbers: version(number)?,
        pre: pre.map(str::to_owned),
    })
}
impl Ord for ReleaseVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        let numeric = self.numbers.cmp(&other.numbers);
        if numeric != Equal {
            return numeric;
        }
        match (&self.pre, &other.pre) {
            (None, None) => Equal,
            (None, Some(_)) => Greater,
            (Some(_), None) => Less,
            (Some(left), Some(right)) => {
                let left = left.split('.').collect::<Vec<_>>();
                let right = right.split('.').collect::<Vec<_>>();
                for (left, right) in left.iter().zip(&right) {
                    let left_number = left.bytes().all(|b| b.is_ascii_digit());
                    let right_number = right.bytes().all(|b| b.is_ascii_digit());
                    let comparison = match (left_number, right_number) {
                        (true, false) => Less,
                        (false, true) => Greater,
                        (true, true) => {
                            let left = left.trim_start_matches('0');
                            let right = right.trim_start_matches('0');
                            left.len().cmp(&right.len()).then_with(|| left.cmp(right))
                        }
                        _ => left.cmp(right),
                    };
                    if comparison != Equal {
                        return comparison;
                    }
                }
                left.len().cmp(&right.len())
            }
        }
    }
}
impl PartialOrd for ReleaseVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
pub fn parse_updates(
    input: &str,
    current: &str,
    channel: Channel,
) -> Result<Option<UpdateRelease>> {
    ensure!(input.len() <= TEXT_LIMIT, "Release response exceeds budget");
    let current = release_version(current).context("Invalid current version")?;
    let releases: Vec<Value> = serde_json::from_str(input)?;
    ensure!(releases.len() <= 100, "Too many releases");
    let mut found: Option<(ReleaseVersion, UpdateRelease)> = None;
    for release in releases {
        if release.get("draft").and_then(Value::as_bool) != Some(false)
            || channel == Channel::Stable
                && release.get("prerelease").and_then(Value::as_bool) != Some(false)
        {
            continue;
        }
        let tag = required_json(release.get("tag_name"), "release tag")?;
        let normalized = tag
            .strip_prefix("jiks-v")
            .or_else(|| tag.strip_prefix("jiks-"))
            .unwrap_or(&tag);
        let Some(numbers) = release_version(normalized) else {
            continue;
        };
        if channel == Channel::Stable && numbers.pre.is_some() {
            continue;
        }
        let mut assets = Vec::new();
        for asset in release
            .get("assets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = asset.get("name").and_then(Value::as_str) else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            if !(lower.starts_with("jiks-") || lower.starts_with("jiks."))
                || !(lower.ends_with(".zip") || lower.ends_with(".exe"))
            {
                continue;
            }
            let url = required_json(asset.get("browser_download_url"), "release asset URL")?;
            ensure!(
                url.starts_with(&format!(
                    "https://github.com/{REPOSITORY}/releases/download/"
                )),
                "Unexpected release asset repository"
            );
            parse_url(&url)?;
            assets.push(UpdateAsset {
                name: name.into(),
                url,
                size: asset.get("size").and_then(Value::as_u64).unwrap_or(0),
                sha256: asset
                    .get("digest")
                    .and_then(Value::as_str)
                    .and_then(|value| value.strip_prefix("sha256:"))
                    .and_then(checksum),
            });
        }
        if assets.is_empty() {
            continue;
        }
        let url = required_json(release.get("html_url"), "release URL")?;
        ensure!(
            url.starts_with(&format!("https://github.com/{REPOSITORY}/releases/tag/")),
            "Unexpected release page repository"
        );
        let candidate = UpdateRelease {
            version: normalized.into(),
            tag: tag.clone(),
            url,
            notes: release
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            assets,
            newer: numbers > current,
        };
        if found.as_ref().is_none_or(|(best, _)| numbers > *best) {
            found = Some((numbers, candidate));
        }
    }
    Ok(found.map(|(_, release)| release))
}

/// Only these legacy preferences change; every other token remains lossless.
pub fn preferences_change(
    document: &SettingsDocument,
    payload: &JsonNode,
) -> Result<SettingsDocument> {
    let fields = payload
        .as_object()
        .context("Package preferences require an object")?;
    let mut root = document.root().clone();
    for (key, value) in fields {
        match key.as_str() {
            "DownloadPath" => ensure!(
                matches!(value, JsonNode::Null)
                    || value
                        .as_str()
                        .is_some_and(|p| Path::new(p).is_absolute() && !p.contains('\0')),
                "DownloadPath must be absolute or null"
            ),
            "OnlyShowUpdates" => {
                ensure!(value.as_bool().is_some(), "OnlyShowUpdates must be boolean")
            }
            "HiddenPackages" => {
                let values = value
                    .as_array()
                    .context("HiddenPackages must be an array")?;
                ensure!(
                    values.len() <= 10000
                        && values
                            .iter()
                            .all(|v| v.as_str().is_some_and(|v| v.len() <= 512)),
                    "Invalid hidden package IDs"
                );
            }
            _ => bail!("Unknown package preference {key}"),
        }
        root.set_known(key, value.clone())?;
    }
    Ok(SettingsDocument::from_root(root)?)
}
pub fn visible_packages<'a>(catalog: &'a Catalog, document: &SettingsDocument) -> Vec<&'a Package> {
    let hidden = document
        .root()
        .get_ci("HiddenPackages")
        .and_then(JsonNode::as_array);
    let only = document
        .root()
        .get_ci("OnlyShowUpdates")
        .and_then(JsonNode::as_bool)
        == Some(true);
    catalog
        .packages
        .iter()
        .filter(|p| {
            !hidden.is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(&p.id)))
                && (!only
                    || matches!(
                        p.availability,
                        Availability::Update | Availability::Unknown(_)
                    ))
        })
        .collect()
}

#[cfg(test)]
mod tests;
