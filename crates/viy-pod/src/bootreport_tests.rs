use chrono::TimeZone;
use pretty_assertions::assert_eq;
use viy_testkit::fixtures;
use viy_testkit::scratch::ScratchDir;
use viy_testkit::time::FakeTime;

use super::*;

pub(crate) fn entries(value: &Value) -> Vec<LogEntry> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| LogEntry {
            ts: e["ts"].as_str().map(str::to_string),
            source: e["source"].as_str().map(str::to_string),
            line: e["line"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

fn lines(lines: &[&str]) -> Vec<LogEntry> {
    lines
        .iter()
        .map(|l| LogEntry {
            line: l.to_string(),
            ..LogEntry::default()
        })
        .collect()
}

fn ts(text: &str) -> DateTime<Utc> {
    parse_ts(text).unwrap()
}

fn stage(key: &'static str, label: &'static str, seconds: f64) -> Stage {
    Stage {
        key,
        label,
        seconds,
    }
}

/// A stage of the fixture, by its key.
fn stage_of(row: &Value) -> Stage {
    let key = row[0].as_str().unwrap();
    let (key, label) = STAGES
        .iter()
        .map(|(k, l, _)| (*k, *l))
        .chain([LOCAL_STAGE])
        .find(|(k, _)| *k == key)
        .unwrap();
    assert_eq!(row[1], label);
    stage(key, label, row[2].as_f64().unwrap())
}

fn details_of(value: &Value) -> Details {
    Details {
        weights_read_s: value["weights_read_s"].as_f64(),
        compile_s: value["compile_s"].as_f64(),
        prewarm_s: value["prewarm_s"].as_f64(),
        gpu: value["gpu"].as_str().map(str::to_string),
    }
}

/// rpctl's text with viy's commands in it.
fn as_viy(text: &str) -> String {
    text.replace("in rp logs", "in viy logs")
}

/// bootreport.measure, facts and render, case by case.
#[test]
fn the_report_reads_the_log_as_rpctls_does() {
    let fixture = fixtures::load("pod/bootreport").unwrap();
    let mut played = 0;
    for case in &fixture.cases {
        let name = case["name"].as_str().unwrap();
        match case["call"].as_str().unwrap() {
            "measure" => {
                let log = entries(&case["entries"]);
                let created = ts(case["created"].as_str().unwrap());
                let ready = ts(case["ready"].as_str().unwrap());
                let (stages, details) =
                    measure(latest_boot(&log), created, ready, &table(EngineName::Vllm));
                let want: Vec<Stage> = case["stages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(stage_of)
                    .collect();
                assert_eq!(stages.len(), want.len(), "{name}");
                for (got, want) in stages.iter().zip(&want) {
                    assert_eq!((got.key, got.label), (want.key, want.label), "{name}");
                    // rpctl keeps microseconds in a float; the difference is noise.
                    assert!((got.seconds - want.seconds).abs() < 1e-3, "{name}: {got:?}");
                }
                assert_eq!(details, details_of(&case["details"]), "{name}");
            }
            "facts" => {
                let found = facts(&entries(&case["entries"]));
                assert_eq!(Value::Object(found.to_json()), case["facts"], "{name}");
            }
            "render" => {
                let stages: Vec<Stage> = case["stages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(stage_of)
                    .collect();
                let outcome = match case["outcome"].as_str().unwrap() {
                    "ready" => Outcome::Ready,
                    "failed" => Outcome::Failed,
                    _ => Outcome::Interrupted,
                };
                let (text, over) = render(
                    &stages,
                    &details_of(&case["details"]),
                    case["total"].as_f64().unwrap(),
                    BUDGET_S,
                    outcome,
                    &table(EngineName::Vllm),
                );
                assert_eq!(text, as_viy(case["text"].as_str().unwrap()), "{name}");
                assert_eq!(over, case["over"].as_bool().unwrap(), "{name}");
            }
            _ => continue,
        }
        played += 1;
    }
    assert_eq!(played, 29);
}

#[test]
fn timestamps_keep_nanoseconds_and_need_utc() {
    let at = ts("2026-06-25T23:35:49.123456789Z");
    assert_eq!(at.timestamp_subsec_nanos(), 123_456_789);
    assert_eq!(ts("2026-06-25T23:35:49"), ts("2026-06-25T23:35:49.000Z"));
    for bad in [
        "",
        "yesterday",
        "2026-06-25 23:35:49Z",
        "2026-13-08T23:35:49Z",
        "2026-06-25T23:35:49+02:00",
    ] {
        assert_eq!(parse_ts(bad), None, "{bad}");
    }
}

// ------------------------------------------------- rpctl tests/test_boot_budget.py

#[test]
fn report_names_the_slowest_stage_when_over_budget() {
    let stages = [
        stage("pull", "GPU allocation + image pull", 150.0),
        stage("weights", "weights to GPU", 60.0),
    ];
    let (text, over) = render(
        &stages,
        &Details::default(),
        210.0,
        BUDGET_S,
        Outcome::Ready,
        &table(EngineName::Vllm),
    );
    assert!(over);
    assert!(text.contains("OVER BUDGET by 30s"));
    assert!(text.lines().last().unwrap().contains("image pull"));
    let (_, over) = render(
        &stages[1..],
        &Details::default(),
        150.0,
        BUDGET_S,
        Outcome::Ready,
        &table(EngineName::Vllm),
    );
    assert!(!over);
}

#[test]
fn measure_splits_the_boot_into_stages() {
    let fixture = fixtures::load("pod/bootreport").unwrap();
    let case = fixture.case("measure_rpctl_test").unwrap();
    let log = entries(&case["entries"]);
    let created = ts("2026-06-25T23:23:03Z");
    let ready = ts("2026-06-25T23:40:05Z");
    let (stages, details) = measure(latest_boot(&log), created, ready, &table(EngineName::Vllm));
    // Rounded as rpctl's round() and the report round: halves to even.
    let got: Vec<(&str, String)> = stages
        .iter()
        .map(|s| (s.key, format!("{:.0}", s.seconds)))
        .collect();
    let want = [
        ("pull", 766),
        ("prepare", 1),
        ("imports", 19),
        ("api_init", 30),
        ("engine_init", 7),
        ("weights", 93),
        ("compile", 75),
        ("engine", 8),
        ("api", 19),
        ("endpoint", 3),
    ];
    assert_eq!(got, want.map(|(k, s)| (k, s.to_string())).to_vec());
    assert_eq!(details.compile_s, Some(71.8));
    assert_eq!(details.prewarm_s, Some(80.0));
}

#[test]
fn facts_explain_pull_and_caches() {
    let found = facts(&lines(&[
        "9a54fb40f516 Already exists",
        "0f06d63f7039 Pull complete",
        "0f06d63f7039 Retrying in 1 second",
        "Status: Downloaded newer image for x",
        "RPCTL_PHASE verified model cache; using offline metadata",
        "RPCTL_PHASE cache restore unavailable; compiling normally (FilterError)",
        "Compiling a graph for compile range (1, 2048) takes 52.85 s",
    ]));
    assert_eq!(found.image, Some("downloaded"));
    assert_eq!(found.layers_already_on_host, Some(1));
    assert_eq!(found.layers_downloaded, Some(1));
    assert_eq!(found.layer_retries, Some(1));
    assert_eq!(found.model_metadata, Some("offline (verified cache)"));
    assert_eq!(
        found.compile_archive.as_deref(),
        Some("restore failed: (FilterError)")
    );
    assert_eq!(found.compile_cache, Some("miss (compiled)"));
    let found = facts(&lines(&[
        "Status: Downloaded newer image for x",
        "Status: Image is up to date for x",
    ]));
    assert_eq!(found.image, Some("downloaded"));
}

#[tokio::test(start_paused = true)]
async fn record_keeps_timeline_config_and_prunes() {
    let dir = ScratchDir::new("bootreport").unwrap();
    let paths = Paths::under(dir.path());
    let time = FakeTime::at(Utc.with_ymd_and_hms(2026, 7, 16, 9, 30, 0).unwrap());
    let records = Records::keeping(&paths, 2);
    let mut timeline = Timeline::start(&time);
    tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
    timeline.mark("pod created", [("pod_id", json!("p1"))]);
    let mut cfg = Config::defaults();
    cfg.set("RUNPOD_API_KEY", "secret-value-123");
    let stages = [stage("pull", "x", 5.0)];
    let mut ids = Vec::new();
    for i in 0..3 {
        let pod_id = format!("p{i}");
        let boot = Boot {
            pod_id: &pod_id,
            stages: &stages,
            details: &Details::default(),
            total: 90.0,
            outcome: if i == 2 {
                Outcome::Failed
            } else {
                Outcome::Ready
            },
            facts: &Facts::default(),
        };
        let row = records.record(&time, &cfg, boot, &timeline).unwrap();
        let id = row["boot_id"].as_str().unwrap().to_string();
        fs::write(records.file(&id, "platform.log"), "x").unwrap();
        ids.push(id);
        time.advance(std::time::Duration::from_secs(60));
    }
    assert_eq!(
        ids,
        [
            "20260716T093001Z-p0",
            "20260716T093101Z-p1",
            "20260716T093201Z-p2"
        ]
    );
    let data = records.read(&ids[2]).unwrap();
    assert_eq!(data["outcome"], "failed");
    assert_eq!(data["at"], "2026-07-16T09:32:01+00:00");
    assert_eq!(
        data["timeline"],
        json!([
            {"t": 0.0, "event": "viy up started"},
            {"t": 1.25, "event": "pod created", "pod_id": "p1"},
        ])
    );
    assert!(!data.to_string().contains("secret-value-123"));
    assert_eq!(data["config"]["VLLM_IMAGE"], cfg.get("VLLM_IMAGE").unwrap());
    assert_eq!(data["config"].as_object().unwrap().len(), BOOT_KEYS.len());
    let left = |suffix: &str| {
        let mut names: Vec<String> = fs::read_dir(paths.boots_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(suffix))
            .collect();
        names.sort();
        names
    };
    assert_eq!(
        left(".json"),
        [format!("{}.json", ids[1]), format!("{}.json", ids[2])]
    );
    assert_eq!(left(".platform.log").len(), 2);
    let history = records.history(10);
    assert_eq!(history.len(), 3);
    assert_eq!(history[0]["stages"], json!({"pull": 5.0}));
    assert_eq!(history[0]["total_s"], json!(90.0));
    assert_eq!(history[0]["budget_s"], json!(180));
    assert_eq!(records.history(1)[0]["boot_id"], ids[2].as_str());
}

#[test]
fn an_unreadable_history_reads_as_none() {
    let dir = ScratchDir::new("bootreport").unwrap();
    let paths = Paths::under(dir.path());
    let records = Records::new(&paths);
    assert_eq!(records.history(10), Vec::<Value>::new());
    fs::create_dir_all(paths.pod_state_dir()).unwrap();
    fs::write(
        paths.boots_history_file(),
        "{\"boot_id\": \"a\"}\n\nnot json\n",
    )
    .unwrap();
    assert_eq!(records.history(10), Vec::<Value>::new());
    assert_eq!(records.read("missing"), None);
}

// ------------------------------------------------------- rpctl tests/test_idle.py

/// test_reports_a_key_runpod_refuses, its last assertion: startup.py's
/// line, which startup_tests.py checks it prints, read back as a fact.
#[test]
fn reports_a_key_runpod_refuses() {
    let line = "RPCTL_PHASE WATCHDOG CANNOT STOP THIS POD: RunPod refused its key (HTTP 403)";
    assert_eq!(
        facts(&lines(&[line])).watchdog.as_deref(),
        Some("CANNOT STOP THIS POD: RunPod refused its key (HTTP 403)")
    );
}

// ------------------------------------------------------------- Strata (kb d171)

/// A Strata boot's log as its script and server print it (the server's
/// lines from pod n2k01nm2n0ftmx's server.log, 2026-08-08).
fn strata_log() -> Vec<LogEntry> {
    [
        (
            "2026-08-08T04:22:00Z",
            "=== boot Sat Aug  8 04:22:00 UTC 2026 ===",
        ),
        (
            "2026-08-08T04:22:01Z",
            "RPCTL_PHASE container started; preparing SSH",
        ),
        ("2026-08-08T04:22:03Z", "RPCTL_PHASE starting Strata"),
        (
            "2026-08-08T04:22:04Z",
            "loading the model (the first start takes a minute or two) ...",
        ),
        (
            "2026-08-08T04:23:18Z",
            "[strata] experts loaded: 85.55 GiB at 2.12 GiB/s (75 s so far)",
        ),
        (
            "2026-08-08T04:23:23Z",
            "ready: http://127.0.0.1:8000/v1  (OpenAI: /v1/chat/completions)",
        ),
        ("2026-08-08T04:23:24Z", "RPCTL_PHASE model ready"),
    ]
    .iter()
    .map(|(ts, line)| LogEntry {
        ts: Some(ts.to_string()),
        source: Some("container".into()),
        line: line.to_string(),
    })
    .collect()
}

#[test]
fn a_strata_boot_is_measured_by_its_own_stages() {
    let created = ts("2026-08-08T04:20:00Z");
    let ready = ts("2026-08-08T04:23:30Z");
    let strata = table(EngineName::Strata);
    let (stages, details) = measure(latest_boot(&strata_log()), created, ready, &strata);
    let got: Vec<(&str, f64)> = stages.iter().map(|s| (s.key, s.seconds)).collect();
    assert_eq!(
        got,
        [
            ("pull", 120.0),
            ("prepare", 3.0),
            ("experts", 75.0),
            ("gpu_cache", 5.0),
            ("endpoint", 7.0)
        ]
    );
    assert_eq!(details, Details::default());
    // vLLM's table finds only the stages both scripts mark
    let (vllm, _) = measure(
        latest_boot(&strata_log()),
        created,
        ready,
        &table(EngineName::Vllm),
    );
    assert_eq!(
        vllm.iter().map(|s| s.key).collect::<Vec<_>>(),
        ["pull", "endpoint"]
    );
}

#[test]
fn a_strata_overrun_gets_stratas_hint() {
    let stages = [
        stage("pull", "GPU allocation + image pull", 150.0),
        stage(
            "prepare",
            "boot script (first boot: release, model, pack)",
            40.0,
        ),
    ];
    let (text, over) = render(
        &stages,
        &Details::default(),
        190.0,
        BUDGET_S,
        Outcome::Ready,
        &table(EngineName::Strata),
    );
    assert!(over);
    assert!(
        text.contains("keep STRATA_IMAGE on RunPod's stock runpod/pytorch image (kb d172)"),
        "{text}"
    );
    // a target for Strata (kb d161), a hard limit for vLLM (kb d004)
    assert!(text.contains("Over the 180s target by 10s."), "{text}");
    assert!(!text.contains("OVER BUDGET"), "{text}");
}

#[tokio::test(start_paused = true)]
async fn a_strata_record_says_its_engine_and_lists_beside_vllms() {
    let dir = ScratchDir::new("bootreport-strata").unwrap();
    let paths = Paths::under(dir.path());
    let time = FakeTime::at(Utc.with_ymd_and_hms(2026, 8, 8, 4, 30, 0).unwrap());
    let records = Records::new(&paths);
    let timeline = Timeline::start(&time);
    let (details, facts) = (Details::default(), Facts::default());
    fn boot<'a>(
        pod_id: &'a str,
        stages: &'a [Stage],
        details: &'a Details,
        facts: &'a Facts,
    ) -> Boot<'a> {
        Boot {
            pod_id,
            stages,
            details,
            total: 130.0,
            outcome: Outcome::Ready,
            facts,
        }
    }
    let vllm_stages = [stage("pull", "x", 60.0), stage("weights", "x", 40.0)];
    let strata_stages = [stage("pull", "x", 10.0), stage("experts", "x", 75.0)];
    let vllm = records
        .record(
            &time,
            &Config::defaults(),
            boot("p1", &vllm_stages, &details, &facts),
            &timeline,
        )
        .unwrap();
    assert!(
        vllm.get("engine").is_none(),
        "vLLM's rows stay as they were"
    );
    time.advance(std::time::Duration::from_secs(60));
    let mut cfg = Config::defaults();
    cfg.set("ENGINE", "strata");
    let strata = records
        .record(
            &time,
            &cfg,
            boot("p2", &strata_stages, &details, &facts),
            &timeline,
        )
        .unwrap();
    assert_eq!(strata["engine"], "strata");
    assert_eq!(
        strata["image"],
        "runpod/pytorch:1.0.3-cu1281-torch280-ubuntu2404"
    );
    let data = records.read(strata["boot_id"].as_str().unwrap()).unwrap();
    assert_eq!(data["config"]["STRATA_PARALLEL"], "2");
    assert_eq!(
        data["config"].as_object().unwrap().len(),
        STRATA_BOOT_KEYS.len()
    );
    let table = history_table(&records, 10);
    let header = table.lines().next().unwrap();
    assert!(header.contains("  weights"), "{header}");
    assert!(header.contains("  experts"), "{header}");
    assert!(header.contains("gpu_cache"), "{header}");
    assert!(header.trim_end().ends_with("endpoint  gpu"), "{header}");
    let shown = show(&records, Some("p2")).unwrap();
    assert!(shown.contains("  experts to RAM"), "{shown}");
}
