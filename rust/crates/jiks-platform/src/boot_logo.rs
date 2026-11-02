//! Explicit Lenovo boot-logo transaction. Constructors perform no firmware,
//! mount, process or settings I/O. Only native.rs supplies an owned native port.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
    path::{Component, Path, PathBuf},
};
pub(crate) mod native;
mod store;
pub(crate) use store::BackupStore;

pub const SHA256_VERSION: i32 = 0x0002_0003;
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_BACKUP_BYTES: usize = 64 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawVariable {
    pub bytes: Vec<u8>,
    pub attributes: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Firmware {
    pub info: RawVariable,
    pub checksum: RawVariable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    pub enabled: bool,
    pub width: u32,
    pub height: u32,
    pub formats: u32,
    pub raw_length: usize,
}
impl Info {
    pub fn decode(raw: &[u8]) -> Result<Self> {
        ensure!(
            [10, 13].contains(&raw.len()),
            "Unknown LBLDESP layout length {} (expected original Pack1 10 or reviewed extended 13)",
            raw.len()
        );
        ensure!(raw[0] <= 1, "Undefined boot-logo enabled value");
        let width = u32::from_le_bytes(raw[1..5].try_into()?);
        let height = u32::from_le_bytes(raw[5..9].try_into()?);
        ensure!(
            width > 0
                && height > 0
                && width <= 8192
                && height <= 8192
                && (width as u64) * (height as u64) <= 16_777_216,
            "Invalid firmware image dimensions"
        );
        let formats = if raw.len() == 10 {
            raw[9] as u32
        } else {
            u32::from_le_bytes(raw[9..13].try_into()?)
        };
        ensure!(
            formats & 0x39 != 0,
            "Firmware advertises no known image format"
        );
        Ok(Self {
            enabled: raw[0] == 1,
            width,
            height,
            formats,
            raw_length: raw.len(),
        })
    }
}
impl Firmware {
    pub fn validate(&self) -> Result<Info> {
        let info = Info::decode(&self.info.bytes)?;
        ensure!(
            self.checksum.bytes.len() == 40,
            "LBLDVC must be the original 40-byte Pack1 layout"
        );
        ensure!(
            self.info.attributes == 7 && self.checksum.attributes == 7,
            "Unexpected Lenovo UEFI variable attributes; raw attributes retained, no write authorized"
        );
        Ok(info)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnimationState {
    pub present: bool,
    pub disabled: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub firmware: Option<Firmware>,
    pub files: Option<BTreeMap<String, Vec<u8>>>,
    pub animation: Option<AnimationState>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LogoChange {
    Enable { source: PathBuf },
    Disable,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Apply {
        logo: Option<LogoChange>,
        animation_disabled: Option<bool>,
    },
    Restore {
        backup_id: String,
    },
    RetryRecovery,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedImage {
    pub bytes: Vec<u8>,
    pub extension: String,
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone)]
pub enum PreparedLogo {
    Enable(PreparedImage),
    Disable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mutation {
    Info(RawVariable),
    Checksum(RawVariable),
    WriteFile { name: String, bytes: Vec<u8> },
    DeleteFile { name: String },
    Animation(AnimationState),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransactionState {
    Prepared,
    Applying,
    Restoring,
    Restored,
    Committed,
    RecoveryRequired,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub backup_id: String,
    pub created_unix_ms: u64,
    pub baseline: Snapshot,
    pub target: Snapshot,
    pub attempted: Vec<Mutation>,
    pub state: TransactionState,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub backup_id: String,
    pub state: TransactionState,
    pub firmware_verified: bool,
    pub files_verified: bool,
    pub animation_verified: bool,
    pub mount_cleanup_verified: bool,
    pub permanent_user_change: bool,
}

pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}
pub fn safe_relative(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 1024
            && !name.contains([':', '\\', '\0'])
            && !name.ends_with(['.', ' ']),
        "Unsafe EFI relative filename"
    );
    for part in Path::new(name).components() {
        let Component::Normal(value) = part else {
            anyhow::bail!("EFI path escapes the Logo directory")
        };
        let value = value.to_string_lossy();
        ensure!(!value.ends_with(['.', ' ']), "Ambiguous Windows filename");
        let stem = value.split('.').next().unwrap_or("").to_ascii_uppercase();
        ensure!(
            !["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && stem.as_bytes()[3].is_ascii_digit()),
            "Reserved Windows filename"
        );
    }
    Ok(())
}
pub fn validate_files(files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    ensure!(files.len() <= 128, "Too many existing EFI logo files");
    let mut total = 0usize;
    let mut names = BTreeSet::new();
    for (name, bytes) in files {
        safe_relative(name)?;
        ensure!(
            names.insert(name.to_lowercase()),
            "Case-ambiguous EFI logo names"
        );
        total = total
            .checked_add(bytes.len())
            .context("EFI backup size overflow")?;
        ensure!(
            total <= MAX_BACKUP_BYTES,
            "EFI logo backup exceeds bounded size"
        );
    }
    Ok(())
}
pub fn checksum(before: &RawVariable, image: Option<&[u8]>) -> Result<RawVariable> {
    ensure!(before.bytes.len() == 40, "Checksum layout changed");
    let prior = i32::from_le_bytes(before.bytes[..4].try_into()?);
    let version = if image.is_none() || prior < 0 || prior >= SHA256_VERSION {
        SHA256_VERSION
    } else {
        prior
    };
    let mut raw = before.clone();
    raw.bytes[..4].copy_from_slice(&version.to_le_bytes());
    raw.bytes[4..36].fill(0);
    if let Some(bytes) = image {
        if version >= SHA256_VERSION {
            raw.bytes[4..36].copy_from_slice(&Sha256::digest(bytes));
        } else {
            raw.bytes[4..8].copy_from_slice(&adler32(bytes).to_le_bytes());
        }
    }
    Ok(raw)
}
pub fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in bytes.chunks(5552) {
        for value in chunk {
            a += *value as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}
pub fn fit(width: u32, height: u32, max_width: u32, max_height: u32) -> Result<(u32, u32)> {
    ensure!(
        width > 0 && height > 0 && max_width > 0 && max_height > 0,
        "Zero image size"
    );
    let ratio = (max_width as f64 / width as f64)
        .min(max_height as f64 / height as f64)
        .min(1.);
    Ok((
        ((width as f64 * ratio).round() as u32).max(1),
        ((height as f64 * ratio).round() as u32).max(1),
    ))
}
pub fn prepare_image(bytes: &[u8], info: &Info) -> Result<PreparedImage> {
    use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES,
        "Boot image is empty/too large"
    );
    let format = image::guess_format(bytes)?;
    let (flag, extension) = match format {
        ImageFormat::Jpeg => (1, "jpg"),
        ImageFormat::Gif => (8, "gif"),
        ImageFormat::Bmp => (0x10, "bmp"),
        ImageFormat::Png => (0x20, "png"),
        _ => anyhow::bail!("Firmware image codec is unsupported"),
    };
    ensure!(
        info.formats & flag != 0,
        "Firmware did not advertise this actual image format"
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    if format == ImageFormat::Gif {
        let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))?;
        decoder.set_limits(limits)?;
        let (width, height) = decoder.dimensions();
        ensure!(
            width > 0 && height > 0 && width <= info.width && height <= info.height,
            "Animated GIF exceeds firmware dimensions"
        );
        let mut decoded = 0u64;
        let mut count = 0u32;
        for frame in decoder.into_frames() {
            let frame = frame?;
            count += 1;
            ensure!(count <= 512, "GIF has too many frames");
            decoded = decoded
                .checked_add(frame.buffer().as_raw().len() as u64)
                .context("GIF decoded size overflow")?;
            ensure!(
                decoded <= 128 * 1024 * 1024,
                "GIF decoded frames exceed memory bound"
            );
        }
        ensure!(count > 0, "GIF has no frames");
        return Ok(PreparedImage {
            bytes: bytes.to_vec(),
            extension: extension.into(),
            width,
            height,
        });
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let image = reader.decode()?;
    let (width, height) = fit(image.width(), image.height(), info.width, info.height)?;
    let image = if (width, height) == (image.width(), image.height()) {
        image
    } else {
        image.resize_exact(width, height, image::imageops::FilterType::CatmullRom)
    };
    let rgba = image.to_rgba8();
    let rgb = image::RgbImage::from_fn(width, height, |x, y| {
        let p = rgba.get_pixel(x, y);
        let a = p[3] as u16;
        image::Rgb([
            ((p[0] as u16 * a + 127) / 255) as u8,
            ((p[1] as u16 * a + 127) / 255) as u8,
            ((p[2] as u16 * a + 127) / 255) as u8,
        ])
    });
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(rgb).write_to(&mut out, format)?;
    let bytes = out.into_inner();
    ensure!(
        bytes.len() <= MAX_IMAGE_BYTES,
        "Encoded logo exceeds bounded size"
    );
    Ok(PreparedImage {
        bytes,
        extension: extension.into(),
        width,
        height,
    })
}

pub trait BootPort {
    fn begin_recovery(&mut self) -> Result<()> {
        Ok(())
    }
    fn validate_target(&mut self, _target: &Snapshot) -> Result<()> {
        Ok(())
    }
    fn guard(&mut self, cleanup: bool) -> Result<()>;
    fn firmware(&mut self) -> Result<Firmware>;
    fn files(&mut self) -> Result<BTreeMap<String, Vec<u8>>>;
    fn animation(&mut self) -> Result<AnimationState>;
    fn prepare_backup(&mut self, record: &mut Record) -> Result<()>;
    /// Durable intent is required before every physical mutation, including rollback.
    fn persist(&mut self, record: &Record) -> Result<()>;
    fn mutate(&mut self, change: &Mutation, cleanup: bool) -> Result<()>;
    fn cleanup(&mut self) -> Result<()>;
}
pub struct Session<P: BootPort> {
    pub(crate) port: P,
    pub(crate) record: Option<Record>,
}
impl<P: BootPort> Session<P> {
    pub fn new(port: P) -> Self {
        Self { port, record: None }
    }
    pub fn from_record(port: P, record: Record) -> Result<Self> {
        validate_snapshot(&record.baseline)?;
        validate_snapshot(&record.target)?;
        Ok(Self {
            port,
            record: Some(record),
        })
    }
    pub fn pending(&self) -> bool {
        self.record.is_some()
    }
    fn capture(&mut self, like: &Snapshot) -> Result<Snapshot> {
        Ok(Snapshot {
            firmware: if like.firmware.is_some() {
                Some(self.port.firmware()?)
            } else {
                None
            },
            files: if like.files.is_some() {
                Some(self.port.files()?)
            } else {
                None
            },
            animation: if like.animation.is_some() {
                Some(self.port.animation()?)
            } else {
                None
            },
        })
    }
    fn capture_owned(&mut self) -> Result<Snapshot> {
        let r = self.record.as_ref().context("Boot transaction missing")?;
        let (fw, files, animation) = (
            r.baseline.firmware.is_some(),
            r.baseline.files.is_some(),
            r.baseline.animation.is_some(),
        );
        Ok(Snapshot {
            firmware: if fw {
                Some(self.port.firmware()?)
            } else {
                None
            },
            files: if files {
                Some(self.port.files()?)
            } else {
                None
            },
            animation: if animation {
                Some(self.port.animation()?)
            } else {
                None
            },
        })
    }
    pub fn apply(
        &mut self,
        logo: Option<PreparedLogo>,
        animation: Option<bool>,
        created_unix_ms: u64,
    ) -> Result<Report> {
        ensure!(
            self.record.is_none(),
            "Recover the pending boot transaction before another request"
        );
        ensure!(logo.is_some() || animation.is_some(), "Empty boot request");
        self.port.guard(false)?;
        let firmware = if logo.is_some() {
            let f = self.port.firmware()?;
            f.validate()?;
            Some(f)
        } else {
            None
        };
        let files = if logo.is_some() {
            let files = self.port.files()?;
            validate_files(&files)?;
            Some(files)
        } else {
            None
        };
        let baseline = Snapshot {
            firmware,
            files,
            animation: if animation.is_some() {
                Some(self.port.animation()?)
            } else {
                None
            },
        };
        let mut target = baseline.clone();
        if let Some(logo) = logo {
            let f = target.firmware.as_mut().unwrap();
            let info = f.validate()?;
            f.info.bytes[0] = u8::from(matches!(logo, PreparedLogo::Enable(_)));
            target.files = Some(BTreeMap::new());
            match logo {
                PreparedLogo::Enable(image) => {
                    let required = match image.extension.as_str() {
                        "png" => 0x20,
                        "jpg" => 1,
                        "gif" => 8,
                        "bmp" => 0x10,
                        _ => anyhow::bail!("Unreviewed prepared logo format"),
                    };
                    ensure!(
                        info.formats & required != 0,
                        "Firmware format capability changed before backup"
                    );
                    ensure!(
                        image.width <= info.width && image.height <= info.height,
                        "Prepared logo dimensions changed"
                    );
                    let filename =
                        format!("mylogo_{}x{}.{}", info.width, info.height, image.extension);
                    safe_relative(&filename)?;
                    f.checksum = checksum(&f.checksum, Some(&image.bytes))?;
                    target.files.as_mut().unwrap().insert(filename, image.bytes);
                }
                PreparedLogo::Disable => {
                    f.checksum = checksum(&f.checksum, None)?;
                }
            }
        }
        if let Some(disabled) = animation {
            target.animation = Some(AnimationState {
                present: true,
                disabled,
            });
        }
        self.apply_target(baseline, target, created_unix_ms)
    }
    pub fn restore_as_new_change(
        &mut self,
        target: Snapshot,
        created_unix_ms: u64,
    ) -> Result<Report> {
        ensure!(
            self.record.is_none(),
            "Pending recovery has priority over another backup restore"
        );
        validate_snapshot(&target)?;
        let baseline = self.capture(&target)?;
        if let (Some(old), Some(current)) = (&target.firmware, &baseline.firmware) {
            ensure!(
                old.info.attributes == current.info.attributes
                    && old.info.bytes[1..] == current.info.bytes[1..],
                "Backup firmware capabilities/layout differ from the current machine; only the enabled byte may be restored"
            );
        }
        self.apply_target(baseline, target, created_unix_ms)
    }
    fn apply_target(
        &mut self,
        baseline: Snapshot,
        target: Snapshot,
        created_unix_ms: u64,
    ) -> Result<Report> {
        validate_snapshot(&baseline)?;
        validate_snapshot(&target)?;
        self.port.validate_target(&target)?;
        let mut record = Record {
            backup_id: String::new(),
            created_unix_ms,
            baseline,
            target,
            attempted: vec![],
            state: TransactionState::Prepared,
            error: None,
        };
        self.port.prepare_backup(&mut record)?;
        self.record = Some(record);
        let result = self.execute(false);
        match result {
            Ok(report) => Ok(report),
            Err(error) => {
                if let Some(record) = self.record.as_mut() {
                    record.error = Some(format!("{error:#}"));
                }
                let restored = self.recover();
                match restored {
                    Ok(report) => anyhow::bail!(
                        "Boot change failed and original state was restored; backup={}: {error:#}",
                        report.backup_id
                    ),
                    Err(restore) => anyhow::bail!(
                        "Boot change failed; recovery is still REQUIRED and owner must remain held: primary={error:#}; recovery={restore:#}"
                    ),
                }
            }
        }
    }
    fn issue(&mut self, mutation: Mutation, cleanup: bool) -> Result<()> {
        self.port.guard(cleanup)?;
        let current = self.capture_owned()?;
        ensure_owned(&current, self.record.as_ref().unwrap())?;
        let record = self.record.as_mut().unwrap();
        record.state = if cleanup {
            TransactionState::Restoring
        } else {
            TransactionState::Applying
        };
        record.attempted.push(mutation.clone());
        self.port.persist(record)?;
        self.port.guard(cleanup)?;
        self.port.mutate(&mutation, cleanup)
    }
    fn execute(&mut self, cleanup: bool) -> Result<Report> {
        let record = self.record.as_ref().context("Boot transaction missing")?;
        let desired = if cleanup {
            record.baseline.clone()
        } else {
            record.target.clone()
        };
        let observed = self.capture(&desired)?;
        ensure_owned(&observed, self.record.as_ref().unwrap())?;
        if observed != desired {
            // Disable the logo before changing its files/hash. Preserve every other
            // unknown byte/attribute; on restore write the exact original bytes last.
            if let Some(current) = observed.firmware.as_ref() {
                if current.info.bytes[0] != 0 {
                    let mut disabled = current.info.clone();
                    disabled.bytes[0] = 0;
                    self.issue(Mutation::Info(disabled), cleanup)?;
                }
            }
            if let Some(target) = desired.files.as_ref() {
                let current = self.port.files()?;
                for name in current.keys() {
                    if !target.contains_key(name) {
                        self.issue(Mutation::DeleteFile { name: name.clone() }, cleanup)?;
                    }
                }
                for (name, bytes) in target {
                    if current.get(name) != Some(bytes) {
                        self.issue(
                            Mutation::WriteFile {
                                name: name.clone(),
                                bytes: bytes.clone(),
                            },
                            cleanup,
                        )?;
                    }
                }
            }
            if let Some(target) = desired.firmware.as_ref() {
                let current = self.port.firmware()?;
                if current.checksum != target.checksum {
                    self.issue(Mutation::Checksum(target.checksum.clone()), cleanup)?;
                }
                let current = self.port.firmware()?;
                if current.info != target.info {
                    self.issue(Mutation::Info(target.info.clone()), cleanup)?;
                }
            }
            if let Some(target) = desired.animation {
                if self.port.animation()? != target {
                    self.issue(Mutation::Animation(target), cleanup)?;
                }
            }
        }
        ensure!(
            self.capture(&desired)? == desired,
            "Boot transaction final physical readback mismatch"
        );
        self.port.cleanup()?;
        let record = self.record.as_mut().unwrap();
        record.state = if cleanup {
            TransactionState::Restored
        } else {
            TransactionState::Committed
        };
        self.port.persist(record)?;
        let report = Report {
            backup_id: record.backup_id.clone(),
            state: record.state.clone(),
            firmware_verified: desired.firmware.is_some(),
            files_verified: desired.files.is_some(),
            animation_verified: desired.animation.is_some(),
            mount_cleanup_verified: true,
            permanent_user_change: !cleanup,
        };
        self.record = None;
        Ok(report)
    }
    pub fn recover(&mut self) -> Result<Report> {
        ensure!(self.record.is_some(), "No pending boot transaction");
        self.port.begin_recovery()?;
        let result = self.execute(true);
        if let Err(error) = &result {
            if let Some(record) = self.record.as_mut() {
                record.state = TransactionState::RecoveryRequired;
                record.error = Some(format!("{error:#}"));
                let _ = self.port.persist(record);
            }
        }
        result
    }
    pub fn shutdown(&mut self) -> Result<()> {
        if self.pending() {
            self.recover()?;
        }
        self.port.cleanup()
    }
}
fn validate_snapshot(s: &Snapshot) -> Result<()> {
    ensure!(
        s.firmware.is_some() == s.files.is_some(),
        "Boot snapshot has mismatched firmware/files"
    );
    if let Some(f) = &s.firmware {
        f.validate()?;
    }
    if let Some(files) = &s.files {
        validate_files(files)?;
    }
    ensure!(
        s.firmware.is_some() || s.animation.is_some(),
        "Empty boot snapshot"
    );
    Ok(())
}
fn ensure_owned(current: &Snapshot, r: &Record) -> Result<()> {
    if let (Some(c), Some(b)) = (&current.firmware, &r.baseline.firmware) {
        ensure!(
            c.info == b.info
                || r.attempted
                    .iter()
                    .any(|m| matches!(m,Mutation::Info(v)if *v==c.info)),
            "External LBLDESP change; refusing automatic overwrite"
        );
        ensure!(
            c.checksum == b.checksum
                || r.attempted
                    .iter()
                    .any(|m| matches!(m,Mutation::Checksum(v)if *v==c.checksum)),
            "External LBLDVC change; refusing automatic overwrite"
        );
    }
    if let (Some(c), Some(b)) = (&current.files, &r.baseline.files) {
        validate_files(c)?;
        let names: BTreeSet<_> = b
            .keys()
            .chain(c.keys())
            .chain(r.attempted.iter().filter_map(|m| match m {
                Mutation::WriteFile { name, .. } | Mutation::DeleteFile { name } => Some(name),
                _ => None,
            }))
            .collect();
        for name in names {
            let value = c.get(name);
            let known = value == b.get(name)
                || r.attempted.iter().any(|m| match m {
                    Mutation::WriteFile { name: n, bytes } if n == name => value == Some(bytes),
                    Mutation::DeleteFile { name: n } if n == name => value.is_none(),
                    _ => false,
                });
            ensure!(
                known,
                "External EFI logo file change at {name}; refusing automatic overwrite"
            );
        }
    }
    if let (Some(c), Some(b)) = (current.animation, r.baseline.animation) {
        ensure!(
            c == b
                || r.attempted
                    .iter()
                    .any(|m| matches!(m,Mutation::Animation(v)if *v==c)),
            "External BCD boot animation change; refusing automatic overwrite"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn baseline() -> Snapshot {
        let mut info = vec![1];
        info.extend_from_slice(&640u32.to_le_bytes());
        info.extend_from_slice(&480u32.to_le_bytes());
        info.push(0x20);
        let mut hash = vec![0u8; 40];
        hash[..4].copy_from_slice(&SHA256_VERSION.to_le_bytes());
        Snapshot {
            firmware: Some(Firmware {
                info: RawVariable {
                    bytes: info,
                    attributes: 7,
                },
                checksum: RawVariable {
                    bytes: hash,
                    attributes: 7,
                },
            }),
            files: Some(BTreeMap::from([(
                "original.png".into(),
                b"original".to_vec(),
            )])),
            animation: Some(AnimationState {
                present: false,
                disabled: false,
            }),
        }
    }
    struct Fake {
        state: Snapshot,
        backup: bool,
        intents: usize,
        writes: usize,
        fail_at: Option<usize>,
        fail_recovery: bool,
        external: bool,
        expired: bool,
        fail_backup: bool,
        fail_persist: bool,
        cleanup_verified: bool,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                state: baseline(),
                backup: false,
                intents: 0,
                writes: 0,
                fail_at: None,
                fail_recovery: false,
                external: false,
                expired: false,
                fail_backup: false,
                fail_persist: false,
                cleanup_verified: false,
            }
        }
    }
    impl BootPort for Fake {
        fn begin_recovery(&mut self) -> Result<()> {
            Ok(())
        }
        fn guard(&mut self, cleanup: bool) -> Result<()> {
            ensure!(
                cleanup || !self.expired || self.writes == 0,
                "original command expired"
            );
            ensure!(!cleanup || !self.fail_recovery, "fake restore failure");
            Ok(())
        }
        fn firmware(&mut self) -> Result<Firmware> {
            Ok(self.state.firmware.clone().unwrap())
        }
        fn files(&mut self) -> Result<BTreeMap<String, Vec<u8>>> {
            Ok(self.state.files.clone().unwrap())
        }
        fn animation(&mut self) -> Result<AnimationState> {
            Ok(self.state.animation.unwrap())
        }
        fn prepare_backup(&mut self, r: &mut Record) -> Result<()> {
            ensure!(!self.fail_backup, "backup flush failed");
            r.backup_id = "boot-logo-fixture".into();
            self.backup = true;
            Ok(())
        }
        fn persist(&mut self, _r: &Record) -> Result<()> {
            self.intents += 1;
            ensure!(!self.fail_persist, "durable intent failure");
            Ok(())
        }
        fn mutate(&mut self, m: &Mutation, cleanup: bool) -> Result<()> {
            assert!(self.backup && self.intents > self.writes);
            self.guard(cleanup)?;
            self.writes += 1;
            match m {
                Mutation::Info(v) => self.state.firmware.as_mut().unwrap().info = v.clone(),
                Mutation::Checksum(v) => self.state.firmware.as_mut().unwrap().checksum = v.clone(),
                Mutation::WriteFile { name, bytes } => {
                    self.state
                        .files
                        .as_mut()
                        .unwrap()
                        .insert(name.clone(), bytes.clone());
                }
                Mutation::DeleteFile { name } => {
                    self.state.files.as_mut().unwrap().remove(name);
                }
                Mutation::Animation(v) => self.state.animation = Some(*v),
            }
            if self.fail_at == Some(self.writes) {
                if self.external {
                    self.state
                        .files
                        .as_mut()
                        .unwrap()
                        .insert("external.png".into(), b"external owner".to_vec());
                }
                anyhow::bail!("setter applied but returned an error");
            }
            Ok(())
        }
        fn cleanup(&mut self) -> Result<()> {
            self.cleanup_verified = true;
            Ok(())
        }
    }
    fn image() -> PreparedLogo {
        PreparedLogo::Enable(PreparedImage {
            bytes: b"new logo".to_vec(),
            extension: "png".into(),
            width: 100,
            height: 50,
        })
    }
    #[test]
    fn pack1_layout_and_checksum_preserve_unknown_bytes_attributes_and_padding() {
        let f = baseline().firmware.unwrap();
        assert_eq!(Info::decode(&f.info.bytes).unwrap().raw_length, 10);
        let mut extended = f.info.bytes.clone();
        extended.extend_from_slice(&[0xa0, 0xb0, 0xc0]);
        assert_eq!(Info::decode(&extended).unwrap().formats, 0xc0b0a020);
        let mut raw = f.checksum;
        raw.bytes[..4].copy_from_slice(&0x10001i32.to_le_bytes());
        raw.bytes[36..].copy_from_slice(&[3, 4, 5, 6]);
        let r = checksum(&raw, Some(b"Wikipedia")).unwrap();
        assert_eq!(&r.bytes[4..8], &0x11e60398u32.to_le_bytes());
        assert_eq!(&r.bytes[36..], &[3, 4, 5, 6]);
        assert_eq!(r.attributes, 7);
        let mut unknown = baseline().firmware.unwrap();
        unknown.info.attributes = 0x47;
        assert!(unknown.validate().is_err());
        assert_eq!(fit(1920, 1080, 640, 480).unwrap(), (640, 360));
        assert_eq!(fit(100, 50, 640, 480).unwrap(), (100, 50));
    }
    #[test]
    fn successful_change_is_permanent_and_backup_precedes_every_write() {
        let mut s = Session::new(Fake::new());
        let r = s.apply(Some(image()), Some(true), 1).unwrap();
        assert!(r.permanent_user_change && r.mount_cleanup_verified);
        assert!(!s.pending());
        assert!(
            s.port
                .state
                .files
                .as_ref()
                .unwrap()
                .contains_key("mylogo_640x480.png")
        );
        assert_eq!(
            s.port.state.animation,
            Some(AnimationState {
                present: true,
                disabled: true
            })
        );
        let after = s.port.state.clone();
        s.shutdown().unwrap();
        assert_eq!(s.port.state, after);
    }
    #[test]
    fn backup_failure_means_zero_firmware_file_or_bcd_writes() {
        let mut f = Fake::new();
        f.fail_backup = true;
        let mut s = Session::new(f);
        assert!(s.apply(Some(image()), Some(true), 1).is_err());
        assert_eq!(s.port.writes, 0);
        assert_eq!(s.port.state, baseline());
        s.shutdown().unwrap();
        assert!(s.port.cleanup_verified);
    }
    #[test]
    fn applied_error_restores_exact_original_and_absent_bcd() {
        let mut f = Fake::new();
        f.fail_at = Some(3);
        let mut s = Session::new(f);
        let e = s
            .apply(Some(image()), Some(true), 1)
            .unwrap_err()
            .to_string();
        assert!(e.contains("original state was restored"));
        assert_eq!(s.port.state, baseline());
        assert!(!s.pending());
    }
    #[test]
    fn failed_restore_retains_pending_and_shutdown_cannot_claim_success() {
        let mut f = Fake::new();
        f.fail_at = Some(3);
        f.fail_recovery = true;
        let mut s = Session::new(f);
        assert!(s.apply(Some(image()), None, 1).is_err());
        assert!(s.pending());
        assert!(s.shutdown().is_err());
        assert!(!s.port.cleanup_verified);
        s.port.fail_recovery = false;
        s.recover().unwrap();
        assert_eq!(s.port.state, baseline());
        assert!(!s.pending());
    }
    #[test]
    fn late_original_deadline_does_not_expire_independent_rollback() {
        let mut f = Fake::new();
        f.expired = true;
        let mut s = Session::new(f);
        assert!(s.apply(Some(image()), None, 1).is_err());
        assert_eq!(s.port.state.firmware, baseline().firmware);
        assert_eq!(s.port.state.files, baseline().files);
        assert!(!s.pending());
    }
    #[test]
    fn external_file_is_not_overwritten_by_automatic_recovery() {
        let mut f = Fake::new();
        f.fail_at = Some(3);
        f.external = true;
        let mut s = Session::new(f);
        assert!(s.apply(Some(image()), None, 1).is_err());
        assert!(s.pending());
        assert_eq!(s.port.writes, 3);
        assert!(
            s.port
                .state
                .files
                .as_ref()
                .unwrap()
                .contains_key("external.png")
        );
        assert!(s.shutdown().is_err());
    }
    #[test]
    fn durable_intent_failure_never_writes_and_retains_journal_obligation() {
        let mut f = Fake::new();
        f.fail_persist = true;
        let mut s = Session::new(f);
        assert!(s.apply(Some(image()), None, 1).is_err());
        assert_eq!(s.port.writes, 0);
        assert!(s.pending());
        assert!(s.shutdown().is_err());
        s.port.fail_persist = false;
        s.recover().unwrap();
        assert_eq!(s.port.writes, 0);
        assert!(!s.pending());
    }
    #[test]
    fn restoring_equal_state_does_not_toggle_firmware_and_different_caps_are_rejected() {
        let mut s = Session::new(Fake::new());
        s.restore_as_new_change(baseline(), 1).unwrap();
        assert_eq!(s.port.writes, 0);
        let mut target = baseline();
        target.firmware.as_mut().unwrap().info.bytes[1..5].copy_from_slice(&641u32.to_le_bytes());
        assert!(s.restore_as_new_change(target, 2).is_err());
        assert_eq!(s.port.writes, 0);
    }
    #[test]
    fn image_codec_flattens_alpha_and_uses_real_magic_not_filename() {
        let info = Info::decode(&baseline().firmware.unwrap().info.bytes).unwrap();
        let rgba = image::RgbaImage::from_pixel(2, 1, image::Rgba([200, 100, 50, 128]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let result = prepare_image(bytes.get_ref(), &info).unwrap();
        let rgb = image::load_from_memory(&result.bytes).unwrap().to_rgb8();
        assert_eq!(*rgb.get_pixel(0, 0), image::Rgb([100, 50, 25]));
        let mut info = info;
        info.formats = 1;
        assert!(prepare_image(bytes.get_ref(), &info).is_err());
        for path in [
            "../outside",
            "/root",
            "x:stream",
            "sub\\path",
            "CON.txt",
            "trailing. ",
        ] {
            assert!(safe_relative(path).is_err(), "{path}");
        }
    }
    #[test]
    fn bcd_parser_preserves_presence_and_rejects_substring_guesses() {
        use native::parse_animation;
        assert_eq!(
            parse_animation("identifier {current}\n").unwrap(),
            AnimationState {
                present: false,
                disabled: false
            }
        );
        assert_eq!(
            parse_animation("bootuxdisabled Yes").unwrap(),
            AnimationState {
                present: true,
                disabled: true
            }
        );
        assert_eq!(
            parse_animation("bootuxdisabled 否").unwrap(),
            AnimationState {
                present: true,
                disabled: false
            }
        );
        assert!(parse_animation("bootuxdisabled unknown1value").is_err());
    }
    #[test]
    fn cleanup_retries_applied_unmount_without_detaching_replacement_volume() {
        use native::cleanup_requires_unmount as decide;
        assert!(!decide(Some("owned"), false, None).unwrap());
        assert!(!decide(None, false, None).unwrap());
        assert!(!decide(Some("owned"), true, Some("external")).unwrap());
        assert!(decide(Some("owned"), true, Some("OWNED")).unwrap());
        assert!(decide(None, true, Some("unknown")).is_err());
        assert!(decide(Some("owned"), true, None).is_err());
    }
}
