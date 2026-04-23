use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use delay_mirror::{DelayChecker, Config, compare_versions, parse_datetime_flexible};
use serde_json::json;

fn make_large_time_json(version_count: usize) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("created".to_string(), json!("2020-01-01T00:00:00Z"));
    map.insert("modified".to_string(), json!("2024-12-31T23:59:59Z"));

    for i in 0..version_count {
        let major = i / 100;
        let minor = (i % 100) / 10;
        let patch = i % 10;
        let version = format!("{}.{}.{}", major, minor, patch);
        map.insert(version, json!(format!("2020-01-01T00:00:00Z")));
    }

    json!(map)
}

fn bench_delay_checker_creation(c: &mut Criterion) {
    c.bench_function("delay_checker_new", |b| {
        b.iter(|| DelayChecker::new(black_box(7)))
    });
}

fn bench_parse_time_field(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_time_field");

    for size in [3, 10, 50, 100, 500] {
        let time_json = make_large_time_json(size);
        group.bench_with_input(
            BenchmarkId::from_parameter(size),
            &time_json,
            |b, time_json| {
                let checker = DelayChecker::new(3);
                b.iter(|| checker.parse_time_field(black_box(time_json)).unwrap())
            },
        );
    }

    group.finish();
}

fn bench_is_version_allowed(c: &mut Criterion) {
    let checker = DelayChecker::new(3);
    let old_time: chrono::DateTime<chrono::Utc> =
        "2024-01-01T00:00:00Z".parse().unwrap();
    let recent_time = chrono::Utc::now() - chrono::Duration::days(1);

    let mut group = c.benchmark_group("is_version_allowed");
    group.bench_function("old_version", |b| {
        b.iter(|| checker.is_version_allowed(black_box(&old_time)))
    });
    group.bench_function("recent_version", |b| {
        b.iter(|| checker.is_version_allowed(black_box(&recent_time)))
    });
    group.finish();
}

fn bench_resolve_version(c: &mut Criterion) {
    let checker = DelayChecker::new(30);
    let now = chrono::Utc::now();
    let time_json = json!({
        "created": "2020-01-01T00:00:00Z",
        "modified": (now - chrono::Duration::days(1)).to_rfc3339(),
        "1.0.0": (now - chrono::Duration::days(100)).to_rfc3339(),
        "1.1.0": (now - chrono::Duration::days(80)).to_rfc3339(),
        "1.2.0": (now - chrono::Duration::days(60)).to_rfc3339(),
        "1.3.0": (now - chrono::Duration::days(40)).to_rfc3339(),
        "1.4.0": (now - chrono::Duration::days(20)).to_rfc3339(),
        "1.5.0": (now - chrono::Duration::days(5)).to_rfc3339(),
        "2.0.0": (now - chrono::Duration::days(1)).to_rfc3339()
    });
    let time_info = checker.parse_time_field(&time_json).unwrap();

    let mut group = c.benchmark_group("resolve_version");
    group.bench_function("allowed_version", |b| {
        b.iter(|| checker.resolve_version(black_box("1.0.0"), black_box(&time_info)).unwrap())
    });
    group.bench_function("denied_version", |b| {
        b.iter(|| checker.resolve_version(black_box("2.0.0"), black_box(&time_info)).unwrap())
    });
    group.bench_function("downgraded_version", |b| {
        b.iter(|| checker.resolve_version(black_box("1.5.0"), black_box(&time_info)).unwrap())
    });
    group.finish();
}

fn bench_find_eligible_version(c: &mut Criterion) {
    let checker = DelayChecker::new(30);
    let time_json = make_large_time_json(100);
    let time_info = checker.parse_time_field(&time_json).unwrap();

    c.bench_function("find_eligible_version_100", |b| {
        b.iter(|| {
            checker.find_eligible_version(black_box("0.9.9"), black_box(&time_info)).unwrap()
        })
    });
}

fn bench_config_from_env(c: &mut Criterion) {
    c.bench_function("config_from_env_vars", |b| {
        b.iter(|| {
            Config::from_env_vars(|key| match key {
                "DELAY_DAYS" => Some("7".to_string()),
                "NPM_REGISTRY" => Some("https://registry.npmmirror.com".to_string()),
                "DEBUG_MODE" => Some("true".to_string()),
                _ => None,
            })
        })
    });
}

fn bench_compare_versions(c: &mut Criterion) {
    let mut group = c.benchmark_group("compare_versions");
    group.bench_function("semver_numeric", |b| {
        b.iter(|| {
            let _ = compare_versions("1.2.3", "4.5.6");
        })
    });
    group.bench_function("pep440_prerelease", |b| {
        b.iter(|| {
            let _ = compare_versions("1.0.0-alpha.1", "1.0.0-beta.2");
        })
    });
    group.bench_function("v_prefix", |b| {
        b.iter(|| {
            let _ = compare_versions("v1.0.0", "v2.0.0");
        })
    });
    group.finish();
}

fn bench_parse_datetime_flexible(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_datetime_flexible");
    group.bench_function("rfc3339_z", |b| {
        b.iter(|| {
            parse_datetime_flexible(
                black_box("2024-01-15T10:30:00Z")
            ).unwrap()
        })
    });
    group.bench_function("iso_no_tz", |b| {
        b.iter(|| {
            parse_datetime_flexible(
                black_box("2024-03-15T14:30:00")
            ).unwrap()
        })
    });
    group.bench_function("with_microseconds", |b| {
        b.iter(|| {
            parse_datetime_flexible(
                black_box("2024-06-01T12:00:00.123456")
            ).unwrap()
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_delay_checker_creation,
    bench_parse_time_field,
    bench_is_version_allowed,
    bench_resolve_version,
    bench_find_eligible_version,
    bench_config_from_env,
    bench_compare_versions,
    bench_parse_datetime_flexible,
);

criterion_main!(benches);
