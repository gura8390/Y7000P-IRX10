//! Ordered audio + optional Lenovo key-LED operation. Partial completion is data,
//! never a blanket claim that all endpoints or an unobservable LED changed.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointState {
    pub capture: bool,
    pub index: u32,
    pub muted: bool,
    pub volume_percent: f32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedFamily {
    Legion,
    ThinkBook,
    IdeaPad,
    Motorola,
    Yoga,
    Unknown,
}
pub fn led_code(family: LedFamily, capture: bool, muted: bool) -> Option<u32> {
    if !matches!(
        family,
        LedFamily::ThinkBook | LedFamily::IdeaPad | LedFamily::Motorola | LedFamily::Yoga
    ) {
        return None;
    }
    Some(match (capture, muted) {
        (true, true) => 1,
        (true, false) => 2,
        (false, true) => 4,
        (false, false) => 5,
    })
}
#[derive(Debug, Clone, Serialize)]
pub struct LedResult {
    pub applicable: bool,
    pub request_accepted: bool,
    pub physical_led_verified: bool,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize)]
pub struct AudioReport {
    pub capture: bool,
    pub endpoints: Vec<EndpointState>,
    pub attempted_indices: Vec<u32>,
    pub completed_indices: Vec<u32>,
    pub audio_verified: bool,
    pub led: LedResult,
    pub errors: Vec<String>,
}
pub trait AudioTransport {
    fn count(&self) -> usize;
    fn guard(&mut self) -> Result<()>;
    fn read(&mut self, index: usize) -> Result<EndpointState>;
    fn set_mute(&mut self, index: usize, value: bool) -> Result<()>;
    fn set_volume(&mut self, index: usize, value: u32) -> Result<()>;
    fn set_led(&mut self, code: u32) -> Result<()>;
}
pub fn execute(
    t: &mut impl AudioTransport,
    capture: bool,
    mute: Option<bool>,
    volume: Option<u32>,
    family: LedFamily,
) -> Result<AudioReport> {
    ensure!(t.count() > 0, "No active audio endpoint");
    ensure!(volume.is_none_or(|v| v <= 100), "Invalid audio volume");
    // Read all endpoints before the first mutation, rejecting an unreadable
    // later endpoint before partially applying a multi-endpoint request.
    let mut report = AudioReport {
        capture,
        endpoints: Vec::new(),
        attempted_indices: Vec::new(),
        completed_indices: Vec::new(),
        audio_verified: false,
        led: LedResult {
            applicable: false,
            request_accepted: false,
            physical_led_verified: false,
            reason: if mute.is_none() {
                "Volume does not request a mute LED change"
            } else {
                "Original Legion policy skips Utility special-key LED writes"
            }
            .into(),
        },
        errors: Vec::new(),
    };
    for i in 0..t.count() {
        let value = t.read(i)?;
        ensure!(
            value.capture == capture && value.index == i as u32 && value.volume_percent.is_finite(),
            "Audio endpoint identity/value changed during preflight"
        );
        report.endpoints.push(value);
    }
    let work = (|| -> Result<()> {
        for i in 0..t.count() {
            report.attempted_indices.push(i as u32);
            if let Some(value) = mute {
                t.guard()?;
                t.set_mute(i, value)?;
                ensure!(
                    t.read(i)?.muted == value,
                    "Audio mute readback mismatch at endpoint {i}"
                );
            }
            if let Some(value) = volume {
                t.guard()?;
                t.set_volume(i, value)?;
                ensure!(
                    (t.read(i)?.volume_percent - value as f32).abs() < 1.1,
                    "Audio volume readback mismatch at endpoint {i}"
                );
            }
            report.endpoints[i] = t.read(i)?;
            report.completed_indices.push(i as u32);
        }
        for i in 0..t.count() {
            let observed = t.read(i)?;
            ensure!(
                mute.is_none_or(|v| observed.muted == v)
                    && volume.is_none_or(|v| (observed.volume_percent - v as f32).abs() < 1.1),
                "Audio endpoint {i} changed before final verification"
            );
            report.endpoints[i] = observed;
        }
        report.audio_verified = true;
        if let Some(code) = mute.and_then(|m| led_code(family, capture, m)) {
            report.led.applicable = true;
            report.led.reason = "Utility.SetFeature acceptance has no LED-state getter".into();
            t.guard()?;
            t.set_led(code)?;
            report.led.request_accepted = true;
        }
        Ok(())
    })();
    if let Err(error) = work {
        report.errors.push(format!("{error:#}"));
        for i in 0..t.count() {
            match t.read(i) {
                Ok(v) => report.endpoints[i] = v,
                Err(e) => report.errors.push(format!("endpoint {i} readback: {e:#}")),
            }
        }
        anyhow::bail!(
            "Audio operation partially completed: {}",
            serde_json::to_string(&report)?
        );
    }
    Ok(report)
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        values: Vec<EndpointState>,
        writes: Vec<String>,
        fail_at: Option<usize>,
        fail_led: bool,
    }
    impl AudioTransport for Fake {
        fn count(&self) -> usize {
            self.values.len()
        }
        fn guard(&mut self) -> Result<()> {
            Ok(())
        }
        fn read(&mut self, i: usize) -> Result<EndpointState> {
            Ok(self.values[i].clone())
        }
        fn set_mute(&mut self, i: usize, v: bool) -> Result<()> {
            self.writes.push(format!("mute{i}"));
            ensure!(self.fail_at != Some(i), "fake endpoint failure");
            self.values[i].muted = v;
            Ok(())
        }
        fn set_volume(&mut self, i: usize, v: u32) -> Result<()> {
            self.writes.push(format!("volume{i}"));
            self.values[i].volume_percent = v as f32;
            Ok(())
        }
        fn set_led(&mut self, code: u32) -> Result<()> {
            self.writes.push(format!("led{code}"));
            ensure!(!self.fail_led, "fake LED failure");
            Ok(())
        }
    }
    fn fake() -> Fake {
        Fake {
            values: (0..2)
                .map(|i| EndpointState {
                    capture: true,
                    index: i,
                    muted: false,
                    volume_percent: 50.,
                })
                .collect(),
            writes: vec![],
            fail_at: None,
            fail_led: false,
        }
    }
    #[test]
    fn legion_skips_led_and_original_family_codes_are_exact() {
        for family in [LedFamily::Legion, LedFamily::Unknown] {
            assert_eq!(led_code(family, true, true), None);
        }
        for family in [
            LedFamily::ThinkBook,
            LedFamily::IdeaPad,
            LedFamily::Motorola,
            LedFamily::Yoga,
        ] {
            for (c, m, code) in [
                (true, true, 1),
                (true, false, 2),
                (false, true, 4),
                (false, false, 5),
            ] {
                assert_eq!(led_code(family, c, m), Some(code));
            }
        }
        let mut f = fake();
        let r = execute(&mut f, true, Some(true), None, LedFamily::Legion).unwrap();
        assert!(r.audio_verified);
        assert!(!r.led.applicable);
        assert_eq!(f.writes, vec!["mute0", "mute1"]);
    }
    #[test]
    fn later_audio_failure_reports_partial_and_does_not_sync_led() {
        let mut f = fake();
        f.fail_at = Some(1);
        let e = execute(&mut f, true, Some(true), None, LedFamily::Yoga)
            .unwrap_err()
            .to_string();
        assert!(e.contains("\"completed_indices\":[0]"));
        assert!(e.contains("\"audio_verified\":false"));
        assert_eq!(f.writes, vec!["mute0", "mute1"]);
        assert!(f.values[0].muted);
        assert!(!f.values[1].muted);
    }
    #[test]
    fn led_failure_preserves_verified_audio_and_invalid_volume_writes_nothing() {
        let mut f = fake();
        f.fail_led = true;
        let e = execute(&mut f, true, Some(true), None, LedFamily::ThinkBook)
            .unwrap_err()
            .to_string();
        assert!(e.contains("\"audio_verified\":true"));
        assert!(e.contains("\"physical_led_verified\":false"));
        assert_eq!(f.writes, vec!["mute0", "mute1", "led1"]);
        let mut f = fake();
        assert!(execute(&mut f, true, None, Some(101), LedFamily::Legion).is_err());
        assert!(f.writes.is_empty());
    }
}
