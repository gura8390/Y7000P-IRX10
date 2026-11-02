use jiks_core::settings::*;
use std::collections::BTreeMap;

#[test]
fn opaque_numeric_tokens_survive_three_newline_conventions_and_unicode() {
    let numbers = [
        "1.2345678901234567890123456789",
        "1e9999",
        "1e-9999",
        "-0",
        "-0.0",
        "1234567890123456789012345678901234567890",
        "2.5000e+0003",
    ];
    for newline in ["\r\n", "\n", "\r"] {
        let text = [
            "{",
            "// 汉字😀é numeric positions",
            "'FutureOption':1, 'futureoption':2,",
            "'前缀😀.数组[0]': [",
            &numbers.join(","),
            "], 'GameDetection':{'UseDiscreteGPU':false, '未来.策略[0]':1e9999,},",
            "'FutureNull':null,}",
        ]
        .join(newline);
        let mut node = JsonNode::parse(&text).unwrap();
        for _ in 0..3 {
            let output = node.to_json();
            let strict: BTreeMap<String, Box<serde_json::value::RawValue>> =
                serde_json::from_str(&output).unwrap();
            let raw_array: Vec<Box<serde_json::value::RawValue>> =
                serde_json::from_str(strict["前缀😀.数组[0]"].get()).unwrap();
            for (raw, expected) in raw_array.iter().zip(numbers) {
                assert_eq!(raw.get(), expected);
            }
            assert_eq!(node.get("FutureOption").unwrap().as_i64(), Some(1));
            assert_eq!(node.get("futureoption").unwrap().as_i64(), Some(2));
            assert_eq!(
                node.get_path(&["GameDetection", "未来.策略[0]"]),
                Some(&JsonNode::Number("1e9999".into()))
            );
            assert_eq!(node.get("FutureNull"), Some(&JsonNode::Null));
            node = JsonNode::parse(&output).unwrap();
        }
    }
}

#[test]
fn loose_numbers_normalize_semantics_without_hex_suffix_mistakes() {
    let node =
        JsonNode::parse("{/*legacy*/ 'hex':0x10,'octal':017,'fraction':.5,'array':[0x10,017,.5],}")
            .unwrap();
    let strict: serde_json::Value = serde_json::from_str(&node.to_json()).unwrap();
    assert_eq!(strict["hex"], 16);
    assert_eq!(strict["octal"], 15);
    assert_eq!(strict["fraction"], 0.5);
    assert_eq!(strict["array"], serde_json::json!([16, 15, 0.5]));
    let unquoted = JsonNode::parse("{plain:1,未来:1e9999,$metadata:null,_name:'value'}").unwrap();
    assert_eq!(
        unquoted.get("未来"),
        Some(&JsonNode::Number("1e9999".into()))
    );
    assert!(serde_json::from_str::<serde_json::Value>(&unquoted.to_json()).is_ok());
    for invalid in [
        "{\"n\":+.5}",
        "{\"n\":08}",
        "{\"n\":1e}",
        "{} {}",
        "{",
        "/*",
    ] {
        assert!(JsonNode::parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn escaped_strings_and_long_token_positions_are_utf8_safe() {
    let prefix = "汉😀é".repeat(1400);
    let text = format!(
        "{{'prefix':'{prefix}','quote':'a\\\'b\\\"c','emoji':'\\uD83D\\uDE00','number':1.2345678901234567890123456789/* after number */}} "
    );
    let node = JsonNode::parse(&text).unwrap();
    assert_eq!(node.get("prefix").unwrap().as_str(), Some(prefix.as_str()));
    assert_eq!(node.get("quote").unwrap().as_str(), Some("a'b\"c"));
    assert_eq!(node.get("emoji").unwrap().as_str(), Some("😀"));
    assert!(node.to_json().contains("1.2345678901234567890123456789"));
    assert!(serde_json::from_str::<serde_json::Value>(&node.to_json()).is_ok());
}

#[test]
fn known_fields_update_but_unknown_case_variants_and_nested_extensions_remain() {
    let original = JsonNode::parse(r#"
        {"temperatureunit":"C","FutureOption":1,"futureoption":2,"SmartKeySinglePressActionId":"old-action",
         "Future":{"numeric":1.2345678901234567890123456789,"array":[1,null,{"type":"future"}]},
         "GameDetection":{"usediscretegpu":false,"FutureOption":1,"futureoption":2},
         "CpuThermalControl":{"PredictionMilliseconds":500,"FutureControl":null,"Policy":{"TargetC":88,"FuturePolicy":1e9999}},
         "WindowSize":{"Width":900,"Height":600,"FutureBounds":{"unit":"DIP"}},
         "PowerPlans":{"Quiet":"11111111-1111-1111-1111-111111111111"},"IncludedProcesses":["delete.exe"]}
        "#).unwrap();
    let current = JsonNode::parse(
        r#"
        {"TemperatureUnit":"F","GameDetection":{"UseDiscreteGPU":true},
         "CpuThermalControl":{"PredictionMilliseconds":700,"Policy":{"TargetC":89}},
         "WindowSize":{"Width":1000,"Height":650},"PowerPlans":{},"IncludedProcesses":[]}
        "#,
    )
    .unwrap();
    let merged = preserve_unknown(&original, &current, &application_schema());
    assert!(merged.get("temperatureunit").is_none());
    assert_eq!(
        merged.get("TemperatureUnit"),
        Some(&JsonNode::String("F".into()))
    );
    for path in [
        vec!["FutureOption"],
        vec!["futureoption"],
        vec!["SmartKeySinglePressActionId"],
        vec!["Future"],
        vec!["GameDetection", "FutureOption"],
        vec!["GameDetection", "futureoption"],
        vec!["CpuThermalControl", "FutureControl"],
        vec!["CpuThermalControl", "Policy", "FuturePolicy"],
        vec!["WindowSize", "FutureBounds"],
    ] {
        assert_eq!(merged.get_path(&path), original.get_path(&path));
    }
    assert!(merged.get_path(&["PowerPlans", "Quiet"]).is_none());
    assert_eq!(
        merged
            .get("IncludedProcesses")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let mut null_current = current;
    null_current
        .set("CpuThermalControl", JsonNode::Null)
        .unwrap();
    assert_eq!(
        preserve_unknown(&original, &null_current, &application_schema()).get("CpuThermalControl"),
        Some(&JsonNode::Null)
    );
}

#[test]
fn unknown_metadata_stays_data_and_cannot_override_typed_thermal_defaults() {
    let doc = SettingsDocument::parse("{\"$type\":\"Untrusted.Missing, Assembly\",\"$ref\":\"opaque\",\"Future\":{\"$values\":[1e9999,null]},\"CpuThermalControl\":null}").unwrap();
    let thermal = ApplicationThermalSettings::from_document(&doc).unwrap();
    assert!(!thermal.enabled);
    assert!(!thermal.shadow_enabled);
    assert_eq!(thermal.sampling_ms, 250);
    assert_eq!(thermal.firmware_limit_c, 88);
    assert!(thermal.validate_control().is_ok());
    assert_eq!(doc.root().get("$ref").unwrap().as_str(), Some("opaque"));
    assert_eq!(
        SettingsDocument::parse(&doc.to_json()).unwrap().root(),
        doc.root()
    );
}

#[test]
fn thermal_settings_parse_legacy_fields_and_reject_unsafe_runtime_values() {
    let doc = SettingsDocument::parse(r#"
        {"enablecputhermalcontrol":true,"CpuThermalSamplingMilliseconds":100,"CpuThermalFirmwareLimitC":88,
         "CpuThermalControl":{"CommandTimeout":"00:00:01.5000000","PredictionMilliseconds":500,
         "Policy":{"TargetC":88,"WarmC":85,"RecoverC":82}}}
        "#).unwrap();
    let view = ApplicationThermalSettings::from_document(&doc).unwrap();
    assert!(view.enabled);
    assert_eq!(view.options.command_timeout_ms, 1500);
    assert!(view.validate_control().is_ok());
    for text in [
        "{\"CpuThermalSamplingMilliseconds\":2147483648}",
        "{\"CpuThermalSamplingMilliseconds\":-1}",
        "{\"EnableCpuThermalControl\":\"true\"}",
    ] {
        assert!(
            ApplicationThermalSettings::from_document(&SettingsDocument::parse(text).unwrap())
                .is_err()
        );
    }
    let invalid = ApplicationThermalSettings::from_document(
        &SettingsDocument::parse("{\"CpuThermalControl\":{\"Policy\":{\"TargetC\":\"NaN\"}}}")
            .unwrap(),
    )
    .unwrap();
    assert!(invalid.validate_control().is_err());
}

#[test]
fn timespan_ticks_are_exact_and_extreme_inputs_never_panic() {
    assert_eq!(
        parse_timespan_ticks("00:00:01.5000000").unwrap(),
        15_000_000
    );
    assert_eq!(
        parse_timespan_ticks("1.00:00:00.0000001").unwrap(),
        864_000_000_001
    );
    assert_eq!(parse_timespan_ticks("-00:00:00.0000001").unwrap(), -1);
    for invalid in [
        "99:99:99",
        "NaN",
        "170141183460469231731687303715884105727.00:00:00",
        "106751991167300.00:00:00",
        "00:00:00.00000001",
    ] {
        assert!(parse_timespan_ticks(invalid).is_err());
    }
}

#[test]
fn argument_contract_preserves_values_first_wins_and_redacts_debug() {
    let startup = vec![
        "--proxy-url".into(),
        "--proxy-url=http://fixture".into(),
        "--proxy-password=  fixture=秘密  ".into(),
        "--trace".into(),
    ];
    let external = vec![
        "--proxy-url=later".into(),
        "--trace=false".into(),
        "--future=keep".into(),
    ];
    let flags = ArgumentFlags::parse(&startup, &external);
    assert_eq!(flags.string("--proxy-url"), Some("http://fixture"));
    assert_eq!(flags.string("--proxy-password"), Some("  fixture=秘密  "));
    assert!(flags.boolean("--trace"));
    assert!(!flags.boolean("--TRACE"));
    assert!(flags.string("--proxy").is_none());
    assert!(!format!("{flags:?}").contains("秘密"));
    assert_eq!(flags.entries().last().unwrap(), "--future=keep");
    let empty = ArgumentFlags::parse(&["--proxy-url=".into()], &[]);
    assert_eq!(empty.string("--proxy-url"), Some(""));
}

#[test]
fn every_store_filename_is_explicit_and_null_normalization_keeps_opaque_fields() {
    assert_eq!(SettingsKind::ALL.len(), 21);
    for &kind in SettingsKind::ALL {
        assert_eq!(SettingsKind::from_filename(kind.filename()), Some(kind));
    }
    assert!(SettingsKind::from_filename("unrelated-private-file.json").is_none());
    let document = SettingsKind::Application.parse("{'GameDetection':null,'IncludedProcesses':null,'ExcludedProcesses':null,'ExcludedRefreshRates':null,'PowerPlans':null,'Future':1e9999}").unwrap();
    for name in [
        "IncludedProcesses",
        "ExcludedProcesses",
        "ExcludedRefreshRates",
    ] {
        assert!(
            document
                .root()
                .get(name)
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    assert!(
        document
            .root()
            .get("PowerPlans")
            .unwrap()
            .as_object()
            .unwrap()
            .is_empty()
    );
    for key in [
        "UseDiscreteGPU",
        "UseGameConfigStore",
        "UseEffectiveGameMode",
    ] {
        assert_eq!(
            document
                .root()
                .get_path(&["GameDetection", key])
                .unwrap()
                .as_bool(),
            Some(true)
        );
    }
    assert!(document.serialize().unwrap().contains("1e9999"));
    for invalid in [
        "{\"TemperatureUnit\":\"not-a-unit\"}",
        "{\"GameDetection\":{\"UseDiscreteGPU\":null}}",
        "{\"PowerPlans\":{\"Quiet\":\"invalid-guid\"}}",
    ] {
        assert!(SettingsKind::Application.parse(invalid).is_err());
    }
    assert!(
        SettingsKind::Application
            .parse("{\"TemperatureUnit\":1234,\"FutureEnum\":\"Unknown\"}")
            .is_ok()
    );
}

#[test]
fn persistence_serialization_rejects_manually_constructed_bad_raw_numbers() {
    let mut raw = JsonNode::object();
    raw.set("Unsafe", JsonNode::Number("0,\"injected\":true".into()))
        .unwrap();
    assert!(SettingsDocument::from_root(raw.clone()).is_err());
    assert!(raw.try_to_json().is_err());
    let mut safe = SettingsDocument::parse("{}").unwrap();
    assert!(
        safe.set_path(&["Unsafe"], JsonNode::Number("NaN".into()))
            .is_err()
    );
    assert_eq!(safe.serialize().unwrap(), "{}");
}

#[test]
fn macro_identifiers_do_not_pass_through_float_or_lose_u64_key_bits() {
    let identifier = MacroIdentifier::parse("Keyboard,18446744073709551615").unwrap();
    assert_eq!(identifier.key, u64::MAX);
    assert_eq!(identifier.legacy_key(), "Keyboard,18446744073709551615");
    assert!(MacroIdentifier::parse("Keyboard,18446744073709551616").is_err());
    assert!(MacroIdentifier::parse("UnknownSource,1").is_err());
}
