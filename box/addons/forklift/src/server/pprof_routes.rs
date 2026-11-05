//! CPU profiling endpoints using the pprof protobuf format.
//! The index, command line and CPU profile are available; unsupported
//! symbol and trace requests receive an explicit 501 response.

use std::io::Write;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Query;
use axum::response::Response;
use axum::routing::get;
use http::{HeaderValue, StatusCode, header};
use serde::Deserialize;

use super::http_error;

const DEFAULT_PROFILE_SECONDS: u64 = 30;

/// Sampling frequency in Hz for the CPU profiler.
const PROFILE_FREQUENCY: i32 = 100;

/// Frames that unwind through these objects are dropped: sampling inside the
/// allocator or the unwinder itself can deadlock.
const BLOCKLIST: &[&str] = &["libc", "libgcc", "pthread", "vdso"];

/// The profiling routes, mounted on their own listener.
pub(crate) fn routes() -> Router {
    Router::new()
        .route("/debug/pprof/", get(index))
        .route("/debug/pprof/cmdline", get(cmdline))
        .route("/debug/pprof/profile", get(profile))
        .route("/debug/pprof/symbol", get(symbol))
        .route("/debug/pprof/trace", get(trace))
}

/// The index page listing what this build can profile.
async fn index() -> Response {
    let html = r#"<html>
<head>
<title>/debug/pprof/</title>
</head>
<body>
/debug/pprof/<br>
<br>
Profiles:
<table>
<tr><td><a href="profile">profile</a></td><td>CPU profile in pprof format; add ?seconds=N to set the duration (default 30)</td></tr>
<tr><td><a href="cmdline">cmdline</a></td><td>The command line of this process</td></tr>
</table>
<br>
Not implemented in the Rust build: symbol, trace.
</body>
</html>
"#;
    let mut resp = Response::new(Body::from(html));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp
}

async fn cmdline() -> Response {
    let args: Vec<String> = std::env::args().collect();
    let mut resp = Response::new(Body::from(args.join("\u{0}")));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// Query parameters of `/debug/pprof/profile`.
#[derive(Debug, Deserialize)]
struct ProfileQuery {
    seconds: Option<u64>,
}

/// Runs a CPU profile for `seconds` and answers with a gzipped pprof protobuf,
/// the format `go tool pprof` and every pprof viewer reads.
async fn profile(Query(q): Query<ProfileQuery>) -> Response {
    let seconds = q.seconds.unwrap_or(DEFAULT_PROFILE_SECONDS);
    if seconds == 0 {
        return http_error(StatusCode::BAD_REQUEST, "invalid seconds");
    }
    // The profiler is a process-wide singleton driven by a signal timer, so
    // the sampling window runs on a blocking thread rather than a runtime
    // worker.
    let profiled =
        tokio::task::spawn_blocking(move || collect_profile(Duration::from_secs(seconds))).await;
    match profiled {
        Ok(Ok(body)) => {
            let mut resp = Response::new(Body::from(body));
            let h = resp.headers_mut();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            h.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment; filename=\"profile\""),
            );
            resp
        }
        Ok(Err(e)) => http_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not enable CPU profiling: {e}"),
        ),
        Err(e) => http_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not enable CPU profiling: {e}"),
        ),
    }
}

/// Samples for `duration` and encodes the report as gzipped pprof protobuf.
fn collect_profile(duration: Duration) -> anyhow::Result<Vec<u8>> {
    use pprof::protos::Message as _;

    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(PROFILE_FREQUENCY)
        .blocklist(BLOCKLIST)
        .build()?;
    std::thread::sleep(duration);
    let report = guard.report().build()?;
    let profile = report.pprof()?;
    let mut raw = Vec::new();
    profile.encode(&mut raw)?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&raw)?;
    Ok(gz.finish()?)
}

/// Rust profiles carry resolved symbols already, so there is nothing for a client to look up.
async fn symbol() -> Response {
    http_error(
        StatusCode::NOT_IMPLEMENTED,
        "symbol lookup is not implemented: the Rust build resolves symbols into the profile itself",
    )
}

async fn trace() -> Response {
    http_error(
        StatusCode::NOT_IMPLEMENTED,
        "execution tracing is not implemented in the Rust build: use /debug/pprof/profile",
    )
}
