//! Parse explicitly supplied public metadata captures. No networking, hardware,
//! user settings, package execution or installation is performed by this tool.
use anyhow::{Context, Result, ensure};
use jiks_runtime::packages::{self, Channel, DetectionFacts, WindowsVersion};
use serde_json::json;
use sha2::{Digest, Sha256};
fn main() -> Result<()> {
    let paths = std::env::args().skip(1).collect::<Vec<_>>();
    ensure!(
        paths.len() == 4,
        "Supply PCSupport JSON, Vantage catalog XML, one Vantage package XML, GitHub releases JSON"
    );
    let captures = paths
        .iter()
        .map(std::fs::read_to_string)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let pc = packages::parse_pc_support(&captures[0], WindowsVersion::Windows11)?;
    let definitions = packages::parse_vantage_catalog(&captures[1])?;
    let package = packages::parse_vantage_package(
        &captures[2],
        definitions.first().context("No Vantage definitions")?,
        &DetectionFacts {
            os: Some(WindowsVersion::Windows11),
            ..Default::default()
        },
    )?;
    let release = packages::parse_updates(&captures[3], "0.1.0", Channel::Stable)?;
    let raw: serde_json::Value = serde_json::from_str(&captures[3])?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"model":"83NN","pc_support":{"packages":pc.packages.len(),"warnings":pc.warnings,"first_id":pc.packages.first().map(|p|&p.id)},"vantage":{"definitions":definitions.len(),"parsed_package_id":package.id,"availability":package.availability,"installer_sha256_present":package.declared_checksum.as_deref().is_some_and(|hash|hash.len()==64)},"github":{"public_releases":raw.as_array().map(Vec::len),"eligible_jiks_release":release},"capture_sha256":captures.iter().map(|text|format!("{:x}",Sha256::digest(text.as_bytes()))).collect::<Vec<_>>(),"networking_by_this_tool":false,"hardware_calls":false,"installer_execution":false})
        )?
    );
    Ok(())
}
