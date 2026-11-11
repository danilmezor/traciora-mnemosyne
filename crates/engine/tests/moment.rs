use mnemosyne_engine::moment;
use serde_json::Value;
use time::{OffsetDateTime, UtcOffset};

#[test]
fn pinned_moment_locale_timezone_and_boundary_outputs() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/reference_moment_clock.json")).unwrap();
    let mut count = 0;
    for timezone in fixture["timezones"].as_array().unwrap() {
        for row in timezone["outputs"].as_array().unwrap() {
            let epoch = row["epoch"].as_i64().unwrap();
            let now = OffsetDateTime::from_unix_timestamp_nanos(i128::from(epoch) * 1_000_000)
                .unwrap()
                .to_offset(
                    UtcOffset::from_whole_seconds(row["offset"].as_i64().unwrap() as i32 * 60)
                        .unwrap(),
                );
            for (format, expected) in row["formats"].as_object().unwrap() {
                let locale = row["locale"].as_str().unwrap();
                assert_eq!(
                    moment::format(now, format, locale).unwrap(),
                    expected.as_str().unwrap(),
                    "zone={} epoch={epoch} locale={locale} format={format}",
                    timezone["TZ"]
                );
                count += 1;
            }
        }
    }
    assert_eq!(count, 1008);
    assert_eq!(moment::locale_ids().count(), 138);
}

#[test]
#[cfg(unix)]
fn clock_with_server_thread_uses_process_timezone() {
    if let Ok(zone) = std::env::var("MNEMOSYNE_CLOCK_CHILD") {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let _ = rx.recv();
        });
        let epoch = 1767240306;
        let utc = OffsetDateTime::from_unix_timestamp(epoch).unwrap();
        let offset = UtcOffset::local_offset_at(utc).expect("local offset with live server thread");
        let expected = if zone == "UTC" { 0 } else { -28800 };
        assert_eq!(offset.whole_seconds(), expected);
        let local = OffsetDateTime::now_local().expect("now_local with live server thread");
        let observed = UtcOffset::local_offset_at(local).unwrap();
        assert_eq!(local.offset(), observed);
        assert_eq!(
            moment::format(utc.to_offset(offset), "YYYY-MM-DD", "en").unwrap(),
            if zone == "UTC" {
                "2026-01-01"
            } else {
                "2025-12-31"
            }
        );
        tx.send(()).unwrap();
        thread.join().unwrap();
        return;
    }
    for zone in ["UTC", "America/Los_Angeles"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "clock_with_server_thread_uses_process_timezone",
                "--nocapture",
            ])
            .env("TZ", zone)
            .env("MNEMOSYNE_CLOCK_CHILD", zone)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn all_bundled_locales_match_captured_era_context_presets_and_meridiem() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/reference_moment_locales.json")).unwrap();
    let formats = fixture["formats"].as_array().unwrap();
    let mut count = 0;
    let mut mismatches = Vec::new();
    for group in fixture["groups"].as_array().unwrap() {
        for row in group["rows"].as_array().unwrap() {
            let epoch = row["epoch"].as_i64().unwrap();
            let now = OffsetDateTime::from_unix_timestamp_nanos(i128::from(epoch) * 1_000_000)
                .unwrap()
                .to_offset(
                    UtcOffset::from_whole_seconds(row["offset"].as_i64().unwrap() as i32 * 60)
                        .unwrap(),
                );
            let locale = row["locale"].as_str().unwrap();
            for (format, expected) in formats.iter().zip(row["outputs"].as_array().unwrap()) {
                let format = format.as_str().unwrap();
                let expected = fixture["output_strings"][expected.as_u64().unwrap() as usize]
                    .as_str()
                    .unwrap();
                let actual = moment::format(now, format, locale).unwrap();
                if actual != expected {
                    mismatches.push(format!("locale={locale} epoch={epoch} pattern={format:?}\nexpected {expected:?}\nactual   {actual:?}"));
                }
                count += 1;
            }
        }
    }
    assert_eq!(count, 44712);
    assert!(
        mismatches.is_empty(),
        "{} mismatches; first20:\n{}",
        mismatches.len(),
        mismatches
            .into_iter()
            .take(20)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn formatter_does_not_panic_at_rust_calendar_range_edges() {
    for year in [-9999, 9999] {
        let now = time::Date::from_calendar_date(year, time::Month::December, 31)
            .unwrap()
            .midnight()
            .assume_utc();
        assert!(!moment::format(now, "YYYY GGGG WW gggg ww N y", "en")
            .unwrap()
            .is_empty());
    }
}
