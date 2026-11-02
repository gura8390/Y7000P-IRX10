//! Metadata-only System/Kernel-Power event 105 history, matching the original
//! on-battery timer. This neither reads temperature nor subscribes to events.
use anyhow::{Context, Result, ensure};
use quick_xml::{Reader, events::Event};
use serde::Serialize;
use std::{mem::size_of, ptr};
use windows_sys::Win32::{Foundation::*, System::EventLog::*};
#[derive(Debug, Clone, Serialize)]
pub struct History {
    pub since_utc: Option<String>,
    pub events_examined: u32,
    pub reset_on_reboot: bool,
    pub source: &'static str,
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
struct Handle(isize);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            EvtClose(self.0);
        }
    }
}
pub fn parse_event(xml: &str) -> Result<(String, bool)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let (mut time, mut ac, mut read_ac) = (None, None, false);
    loop {
        match reader.read_event()? {
            Event::Start(event) | Event::Empty(event) => {
                let tag = event.name();
                let name = tag.as_ref();
                if name == b"TimeCreated" {
                    for attr in event.attributes() {
                        let attr = attr?;
                        if attr.key.as_ref() == b"SystemTime" {
                            time = Some(
                                attr.decode_and_unescape_value(reader.decoder())?
                                    .into_owned(),
                            );
                        }
                    }
                }
                if name == b"Data" {
                    read_ac = false;
                    for attr in event.attributes() {
                        let attr = attr?;
                        if attr.key.as_ref() == b"Name"
                            && attr.decode_and_unescape_value(reader.decoder())?.as_ref()
                                == "AcOnline"
                        {
                            read_ac = true;
                        }
                    }
                }
            }
            Event::Text(text) if read_ac => {
                let value = text.decode()?;
                ac = match value.as_ref() {
                    "true" | "1" => Some(true),
                    "false" | "0" => Some(false),
                    _ => None,
                };
            }
            Event::End(event) if event.name().as_ref() == b"Data" => read_ac = false,
            Event::Eof => break,
            _ => {}
        }
    }
    let time = time.context("Kernel-Power event lacks UTC time")?;
    ensure!(
        (20..=36).contains(&time.len())
            && time.ends_with('Z')
            && time.as_bytes().get(10) == Some(&b'T'),
        "Invalid event UTC timestamp"
    );
    Ok((
        time,
        ac.context("Kernel-Power event lacks AcOnline boolean")?,
    ))
}
/// Reverse chronological events: the oldest consecutive DC event is the start
/// of this interval. A bounded scan without an AC boundary is unknown.
pub fn reduce(events: &[(String, bool)], exhausted: bool) -> Option<String> {
    let mut since = None;
    for (time, ac) in events {
        if *ac {
            return since;
        }
        since = Some(time.clone());
    }
    if exhausted { since } else { None }
}
pub fn read(reset_on_reboot: bool) -> Result<History> {
    unsafe {
        let started = crate::monotonic_ms();
        let filter = if reset_on_reboot {
            format!(
                "*[System[Provider[@Name='Microsoft-Windows-Kernel-Power'] and EventID=105 and TimeCreated[timediff(@SystemTime) <= {}]]]",
                started
            )
        } else {
            "*[System[Provider[@Name='Microsoft-Windows-Kernel-Power'] and EventID=105]]".into()
        };
        let query = EvtQuery(
            0,
            wide("System").as_ptr(),
            wide(&filter).as_ptr(),
            EvtQueryChannelPath | EvtQueryReverseDirection,
        );
        ensure!(
            query != 0,
            "Read on-battery event history failed ({})",
            GetLastError()
        );
        let query = Handle(query);
        let mut events = Vec::new();
        let mut exhausted = false;
        for _ in 0..128 {
            ensure!(
                crate::monotonic_ms().saturating_sub(started) < 1500,
                "Battery history query exceeded its metadata budget"
            );
            let mut event = 0isize;
            let mut count = 0;
            if EvtNext(query.0, 1, &mut event, 0, 0, &mut count) == 0 {
                let error = GetLastError();
                ensure!(
                    error == ERROR_NO_MORE_ITEMS,
                    "Read battery history event failed ({error})"
                );
                exhausted = true;
                break;
            }
            ensure!(
                count == 1 && event != 0,
                "Battery history returned an invalid event"
            );
            let event = Handle(event);
            let (mut needed, mut properties) = (0, 0);
            let probe = EvtRender(
                0,
                event.0,
                EvtRenderEventXml,
                0,
                ptr::null_mut(),
                &mut needed,
                &mut properties,
            );
            ensure!(
                probe != 0 || GetLastError() == ERROR_INSUFFICIENT_BUFFER,
                "Battery event XML size query failed"
            );
            ensure!(
                needed > 0 && needed <= 64 * 1024 && needed as usize % size_of::<u16>() == 0,
                "Battery event XML exceeds metadata bounds"
            );
            let mut buffer = vec![0u16; needed as usize / 2];
            let bytes = needed;
            ensure!(
                EvtRender(
                    0,
                    event.0,
                    EvtRenderEventXml,
                    bytes,
                    buffer.as_mut_ptr().cast(),
                    &mut needed,
                    &mut properties
                ) != 0
                    && needed <= bytes,
                "Battery event XML read failed"
            );
            let text = String::from_utf16(
                &buffer[..buffer.iter().position(|v| *v == 0).unwrap_or(buffer.len())],
            )?;
            let parsed = parse_event(&text)?;
            let ac = parsed.1;
            events.push(parsed);
            if ac {
                exhausted = true;
                break;
            }
        }
        ensure!(
            exhausted,
            "Battery history scan reached its bound before a trustworthy AC/DC boundary"
        );
        Ok(History {
            since_utc: reduce(&events, exhausted),
            events_examined: events.len() as u32,
            reset_on_reboot,
            source: "System/Microsoft-Windows-Kernel-Power event 105 AcOnline",
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_named_metadata_not_other_data_order() {
        assert_eq!(parse_event("<Event><System><TimeCreated SystemTime='2026-11-03T00:01:02.123Z'/></System><EventData><Data Name='Other'>true</Data><Data Name='AcOnline'>false</Data></EventData></Event>").unwrap(),("2026-11-03T00:01:02.123Z".into(),false));
    }
    #[test]
    fn reverse_history_uses_oldest_consecutive_dc_and_never_prior_interval() {
        let events = vec![
            ("new".into(), false),
            ("old".into(), false),
            ("ac".into(), true),
            ("olderDC".into(), false),
        ];
        assert_eq!(reduce(&events, true), Some("old".into()));
        assert_eq!(reduce(&[("ac".into(), true)], true), None);
        assert_eq!(reduce(&events[..2], false), None);
    }
    #[test]
    fn missing_or_invalid_values_never_manufacture_dc() {
        assert!(parse_event("<Event><TimeCreated SystemTime='2026-11-03T00:00:00Z'/><Data Name='AcOnline'>unknown</Data></Event>").is_err());
        assert!(parse_event("<Event/>").is_err());
    }
}
