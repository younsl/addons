use std::time::Duration;

use forklift::config;

/// Parses the command line into `cfg`, reporting whether `-version` was asked
/// for. The `config::load()` values seed the defaults so any `FORKLIFT_*` env
/// still applies, while flags take precedence.
pub(crate) fn parse_flags(cfg: &mut config::Config) -> Result<bool, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let Some(flag) = arg
            .strip_prefix("--")
            .or_else(|| arg.strip_prefix('-'))
            .filter(|f| !f.is_empty())
        else {
            return Err(format!("flag provided but not defined: {arg}"));
        };
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (flag, None),
        };
        i += 1;
        if name == "version" {
            if !bool_value(inline.as_deref(), name)? {
                continue;
            }
            return Ok(true);
        }
        if name == "help" || name == "h" {
            return Err(String::new());
        }
        if name == "ui-upload-enabled" {
            cfg.upload.enabled = bool_value(inline.as_deref(), name)?;
            continue;
        }
        // The name is checked before its argument is consumed, so an unknown
        // flag is reported as undefined rather than as a missing argument.
        const VALUE_FLAGS: [&str; 15] = [
            "osv-url",
            "vuln-rescan-interval",
            "vuln-ttl",
            "vuln-workers",
            "deps-dev-url",
            "license-rescan-interval",
            "license-ttl",
            "license-workers",
            "ui-upload-max-duration",
            "ui-upload-max-concurrent",
            "ui-upload-max-concurrent-user",
            "ui-upload-max-assets",
            "ui-upload-max-file-bytes",
            "ui-upload-max-batch-bytes",
            "ui-upload-go-max-zip-bytes",
        ];
        if !VALUE_FLAGS.contains(&name) {
            return Err(format!("flag provided but not defined: -{name}"));
        }
        let value = if let Some(value) = inline {
            value
        } else {
            let Some(next) = args.get(i) else {
                return Err(format!("flag needs an argument: -{name}"));
            };
            i += 1;
            next.clone()
        };
        match name {
            "osv-url" => cfg.vuln.osv_url = value,
            "vuln-rescan-interval" => cfg.vuln.rescan_interval = duration_value(&value, name)?,
            "vuln-ttl" => cfg.vuln.ttl = duration_value(&value, name)?,
            "vuln-workers" => cfg.vuln.workers = int_value(&value, name)?,
            "deps-dev-url" => cfg.license.deps_dev_url = value,
            "license-rescan-interval" => {
                cfg.license.rescan_interval = duration_value(&value, name)?;
            }
            "license-ttl" => cfg.license.ttl = duration_value(&value, name)?,
            "license-workers" => cfg.license.workers = int_value(&value, name)?,
            "ui-upload-max-duration" => cfg.upload.max_duration = duration_value(&value, name)?,
            "ui-upload-max-concurrent" => cfg.upload.max_concurrent = int_value(&value, name)?,
            "ui-upload-max-concurrent-user" => {
                cfg.upload.max_concurrent_user = int_value(&value, name)?;
            }
            "ui-upload-max-assets" => cfg.upload.max_assets = int_value(&value, name)?,
            "ui-upload-max-file-bytes" => cfg.upload.max_file_bytes = bytes_value(&value, name)?,
            "ui-upload-max-batch-bytes" => cfg.upload.max_batch_bytes = bytes_value(&value, name)?,
            "ui-upload-go-max-zip-bytes" => {
                cfg.upload.go_max_zip_bytes = bytes_value(&value, name)?;
            }
            // VALUE_FLAGS above is the same list, so this is unreachable.
            _ => return Err(format!("flag provided but not defined: -{name}")),
        }
    }
    Ok(false)
}

fn bool_value(value: Option<&str>, name: &str) -> Result<bool, String> {
    match value {
        None => Ok(true),
        Some("true" | "1" | "t" | "T" | "TRUE" | "True") => Ok(true),
        Some("false" | "0" | "f" | "F" | "FALSE" | "False") => Ok(false),
        Some(other) => Err(format!(
            "invalid boolean value {other:?} for -{name}: parse error"
        )),
    }
}

fn int_value(value: &str, name: &str) -> Result<i64, String> {
    value
        .parse::<i64>()
        .map_err(|e| format!("invalid value {value:?} for flag -{name}: {e}"))
}

fn duration_value(value: &str, name: &str) -> Result<Duration, String> {
    let nanos = config::parse_duration_nanos(value)
        .map_err(|e| format!("invalid value {value:?} for flag -{name}: {e}"))?;
    Ok(Duration::from_nanos(nanos.max(0) as u64))
}

fn bytes_value(value: &str, name: &str) -> Result<i64, String> {
    config::parse_byte_size(value)
        .map_err(|e| format!("invalid value {value:?} for flag -{name}: {e}"))
}

pub(crate) fn usage() -> String {
    [
        "Usage of forklift:",
        "  -deps-dev-url string",
        "        deps.dev API base URL for license scanning; empty disables it",
        "  -license-rescan-interval duration",
        "        how often stale license results are re-queried",
        "  -license-ttl duration",
        "        age at which a license result becomes stale",
        "  -license-workers int",
        "        number of concurrent license resolution workers draining the queue",
        "  -osv-url string",
        "        OSV API base URL for vulnerability scanning; empty disables it",
        "  -ui-upload-enabled",
        "        enable management-API and web UI artifact upload",
        "  -ui-upload-go-max-zip-bytes value",
        "        maximum Go module zip bytes (bytes or KiB/MiB/GiB)",
        "  -ui-upload-max-assets int",
        "        maximum files in one UI artifact upload",
        "  -ui-upload-max-batch-bytes value",
        "        maximum aggregate bytes per upload (bytes or KiB/MiB/GiB)",
        "  -ui-upload-max-concurrent int",
        "        maximum process-wide concurrent UI artifact uploads",
        "  -ui-upload-max-concurrent-user int",
        "        maximum concurrent UI artifact uploads per principal",
        "  -ui-upload-max-duration duration",
        "        maximum duration of one UI artifact upload",
        "  -ui-upload-max-file-bytes value",
        "        maximum bytes per non-Go upload file (bytes or KiB/MiB/GiB)",
        "  -version",
        "        print version and exit",
        "  -vuln-rescan-interval duration",
        "        how often stale vulnerability scan results are re-queried",
        "  -vuln-ttl duration",
        "        age at which a vulnerability scan result becomes stale",
        "  -vuln-workers int",
        "        number of concurrent vulnerability scan workers draining the queue",
    ]
    .join("\n")
}
