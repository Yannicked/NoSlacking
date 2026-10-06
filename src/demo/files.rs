//! What the demo's files hold, for the file viewer: a workbook, a CSV
//! file, a zip archive and a source file, made here from a few lines so
//! nothing large is checked in.

use std::io::Write as _;

use crate::viewer::SampleCell::{self, Date, Empty, Number, Text};

/// The contents of the demo file at `url`, by the file id in it, or
/// `None` for a file the demo has no contents for.
pub fn contents(url: &str) -> Option<Vec<u8>> {
    let id = url
        .split('/')
        .find_map(|part| part.strip_prefix("TDEMO-"))?;
    let id = id.split('-').next()?;
    match id {
        "F20" => Some(BACKOFF.as_bytes().to_vec()),
        "F21" => Some(RECONNECT_LOG.as_bytes().to_vec()),
        "F23" => Some(budget()),
        "F26" => Some(deploys_csv().into_bytes()),
        "F27" => Some(logs_zip()),
        _ => None,
    }
}

/// The whole of `backoff.rs`, of which Slack previews the start.
pub const BACKOFF: &str = "/// Waits longer after each failed attempt, up to a minute.
fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_millis(500);
    // Doubling, capped so it never waits for hours.
    let factor = 2u32.saturating_pow(attempt.min(7));
    (base * factor).min(Duration::from_secs(60))
}

#[test]
fn backoff_is_capped() {
    assert_eq!(backoff(0), Duration::from_millis(500));
    assert_eq!(backoff(1), Duration::from_secs(1));
    assert_eq!(backoff(30), Duration::from_secs(60));
}

/// Retries `attempt` until it works, waiting in between.
async fn retry<T, E>(mut attempt: impl AsyncFnMut() -> Result<T, E>) -> T {
    let mut tries = 0;
    loop {
        match attempt().await {
            Ok(value) => return value,
            Err(_) => tokio::time::sleep(backoff(tries)).await,
        }
        tries += 1;
    }
}
";

const RECONNECT_LOG: &str = "2026-09-30 11:02:14 INFO  socket: connected (wss-primary)
2026-09-30 11:47:51 WARN  socket: no pong in 30s, reconnecting
2026-09-30 11:47:52 INFO  socket: connected (wss-backup)
";

/// "Q4 budget.xlsx": a budget sheet and a sheet of machines.
fn budget() -> Vec<u8> {
    let items = [
        ("Build machines", "Hardware", 4, 4_200.0),
        ("Monitors", "Hardware", 6, 389.5),
        ("CI minutes", "Services", 1, 1_250.0),
        ("Code signing certificate", "Services", 1, 499.0),
        ("Crash reporting", "Services", 1, 312.0),
        ("Translation review", "People", 2, 640.0),
        ("Conference tickets", "Travel", 3, 899.0),
        ("Hotel, Utrecht", "Travel", 3, 412.75),
        ("Train tickets", "Travel", 3, 96.4),
        ("Coffee & tea", "Office", 1, 180.0),
        ("Standing desks", "Office", 2, 725.0),
        ("Keyboards", "Hardware", 5, 129.99),
        ("Test phones", "Hardware", 3, 549.0),
        ("Backup storage", "Services", 1, 75.0),
    ]
    .map(|(item, group, count, cost): (&str, &str, u32, f64)| {
        (item, group, f64::from(count), cost)
    });
    let mut rows: Vec<Vec<SampleCell<'_>>> = vec![vec![
        Text("Item"),
        Text("Group"),
        Text("Count"),
        Text("Each"),
        Text("Total"),
        Text("Due"),
    ]];
    for (index, (item, group, count, each)) in items.iter().enumerate() {
        rows.push(vec![
            Text(item),
            Text(group),
            Number(*count),
            Number(*each),
            Number((count * each * 100.0).round() / 100.0),
            Date(46_295.0 + (index as f64) * 7.0),
        ]);
    }
    let total: f64 = items.iter().map(|(_, _, count, each)| count * each).sum();
    rows.push(vec![Empty]);
    rows.push(vec![
        Text("Total"),
        Empty,
        Empty,
        Empty,
        Number((total * 100.0).round() / 100.0),
    ]);
    let machines: Vec<Vec<SampleCell<'_>>> = vec![
        vec![
            Text("Host"),
            Text("Cores"),
            Text("Memory (GB)"),
            Text("Arrives"),
        ],
        vec![
            Text("build-01"),
            Number(32.0),
            Number(128.0),
            Date(46_301.0),
        ],
        vec![
            Text("build-02"),
            Number(32.0),
            Number(128.0),
            Date(46_301.0),
        ],
        vec![
            Text("build-03"),
            Number(64.0),
            Number(256.0),
            Date(46_308.0),
        ],
        vec![
            Text("build-04"),
            Number(64.0),
            Number(256.0),
            Date(46_308.0),
        ],
    ];
    let rows: Vec<&[SampleCell<'_>]> = rows.iter().map(Vec::as_slice).collect();
    let machines: Vec<&[SampleCell<'_>]> = machines.iter().map(Vec::as_slice).collect();
    crate::viewer::sample_workbook(&[("Budget", &rows), ("Machines", &machines)])
}

/// "deploys.csv": a few weeks of deploys.
fn deploys_csv() -> String {
    let mut csv = String::from("date,service,version,author,duration_s,result,notes\n");
    let services = ["api", "web", "worker", "search"];
    let authors = ["Ana Lima", "Bo Chen", "Cleo Park", "Dev Shah"];
    for day in 0..120u32 {
        let service = services[(day % 4) as usize];
        let author = authors[(day * 7 % 4) as usize];
        let result = if day % 11 == 5 { "rolled back" } else { "ok" };
        let notes = if day % 11 == 5 {
            "\"health check failed, see #incidents\""
        } else if day % 9 == 0 {
            "\"config only, no restart\""
        } else {
            ""
        };
        csv.push_str(&format!(
            "2026-{:02}-{:02},{service},1.{}.{},{author},{},{result},{notes}\n",
            7 + day / 30,
            1 + day % 30,
            day / 10,
            day % 10,
            40 + (day * 37) % 300
        ));
    }
    csv
}

/// "logs.zip": a folder of logs and a readme.
fn logs_zip() -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(
            zip::DateTime::from_date_and_time(2026, 9, 30, 11, 48, 0).unwrap_or_default(),
        );
    let mut files: Vec<(String, Vec<u8>)> = vec![(
        "README.md".into(),
        b"# Logs\n\nFrom the night of the 29th.\n".to_vec(),
    )];
    for (host, lines) in [("api-1", 4_000), ("api-2", 3_200), ("worker-1", 9_000)] {
        let mut log = String::new();
        for line in 0..lines {
            log.push_str(&format!(
                "2026-09-30 0{}:{:02}:{:02} INFO request handled in {} ms\n",
                line % 10,
                line % 60,
                line * 7 % 60,
                line % 97
            ));
        }
        files.push((format!("logs/{host}.log"), log.into_bytes()));
    }
    files.push(("logs/crash-dump.bin".into(), vec![7u8; 48_000]));
    let written = zip.add_directory("logs/", options).is_ok()
        && files.iter().all(|(name, data)| {
            zip.start_file(name.as_str(), options).is_ok() && zip.write_all(data).is_ok()
        });
    if !written {
        return Vec::new();
    }
    zip.finish()
        .map(std::io::Cursor::into_inner)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer::{Body, Kind, read};

    #[test]
    fn every_demo_file_reads() {
        let url = |id: &str| format!("https://files.slack.com/files-pri/TDEMO-{id}/x");
        for (id, kind) in [
            ("F20", Kind::Text),
            ("F21", Kind::Text),
            ("F23", Kind::Sheet),
            ("F26", Kind::Csv),
            ("F27", Kind::Zip),
        ] {
            let bytes = contents(&url(id)).expect("demo file");
            let document = read(kind, &bytes, false).expect("reads");
            let empty = match &document.body {
                Body::Sheets(sheets) => sheets.iter().all(|s| s.rows.is_empty()),
                Body::Archive(archive) => archive.entries.is_empty(),
                Body::Text(text) => text.lines.is_empty(),
            };
            assert!(!empty, "{id}");
        }
        assert!(contents(&url("F99")).is_none());
    }
}
