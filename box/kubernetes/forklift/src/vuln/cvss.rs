//! CVSS v3.x base score computation from a vector string, per the v3.1
//! specification.

use std::collections::HashMap;

/// Attack Vector weight.
fn cvss_av(v: &str) -> Option<f64> {
    match v {
        "N" => Some(0.85),
        "A" => Some(0.62),
        "L" => Some(0.55),
        "P" => Some(0.2),
        _ => None,
    }
}

/// Attack Complexity weight.
fn cvss_ac(v: &str) -> Option<f64> {
    match v {
        "L" => Some(0.77),
        "H" => Some(0.44),
        _ => None,
    }
}

/// User Interaction weight.
fn cvss_ui(v: &str) -> Option<f64> {
    match v {
        "N" => Some(0.85),
        "R" => Some(0.62),
        _ => None,
    }
}

/// Confidentiality / Integrity / Availability impact weight.
fn cvss_cia(v: &str) -> Option<f64> {
    match v {
        "H" => Some(0.56),
        "L" => Some(0.22),
        "N" => Some(0.0),
        _ => None,
    }
}

/// Returns the Privileges Required weight, which depends on whether the scope
/// changed.
fn cvss_pr(v: &str, scope_changed: bool) -> Option<f64> {
    match v {
        "N" => Some(0.85),
        "L" => Some(if scope_changed { 0.68 } else { 0.62 }),
        "H" => Some(if scope_changed { 0.5 } else { 0.27 }),
        _ => None,
    }
}

/// Computes the CVSS v3.x base score from a vector string such as
/// "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H". It returns `None` when the
/// string is not a parseable CVSS v3 base vector.
pub(crate) fn cvss_base_score(vector: &str) -> Option<f64> {
    if !vector.starts_with("CVSS:3") {
        return None;
    }
    let mut m: HashMap<&str, &str> = HashMap::new();
    for part in vector.split('/') {
        if let Some((k, val)) = part.split_once(':') {
            m.insert(k, val);
        }
    }
    let get = |k: &str| m.get(k).copied().unwrap_or("");
    let scope_changed = get("S") == "C";
    let av = cvss_av(get("AV"))?;
    let ac = cvss_ac(get("AC"))?;
    let pr = cvss_pr(get("PR"), scope_changed)?;
    let ui = cvss_ui(get("UI"))?;
    let c = cvss_cia(get("C"))?;
    let i = cvss_cia(get("I"))?;
    let a = cvss_cia(get("A"))?;
    let iss = 1.0 - (1.0 - c) * (1.0 - i) * (1.0 - a);
    let impact = if scope_changed {
        7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powf(15.0)
    } else {
        6.42 * iss
    };
    if impact <= 0.0 {
        return Some(0.0);
    }
    let expl = 8.22 * av * ac * pr * ui;
    if scope_changed {
        return Some(cvss_roundup((1.08 * (impact + expl)).min(10.0)));
    }
    Some(cvss_roundup((impact + expl).min(10.0)))
}

/// Rounds up to the nearest 0.1, as defined by the CVSS v3.1 spec.
fn cvss_roundup(x: f64) -> f64 {
    let i = (x * 100000.0).round() as i64;
    if i % 10000 == 0 {
        return i as f64 / 100000.0;
    }
    ((i as f64 / 10000.0).floor() + 1.0) / 10.0
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::vuln::cvss::cvss_base_score;
    use crate::vuln::osv::{OsvSeverity, OsvVuln, score_of};

    #[test]
    fn cvss_base_score_cases() {
        let cases: &[(&str, f64, bool)] = &[
            // AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H -> 9.8 critical.
            ("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H", 9.8, true),
            // Low-impact ReDoS (only availability, low) -> 5.3.
            ("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:L", 5.3, true),
            // Scope changed raises the score.
            ("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H", 10.0, true),
            // Not a CVSS v3 vector.
            ("CVSS:2.0/AV:N", 0.0, false),
            ("not-a-vector", 0.0, false),
            // Missing metrics.
            ("CVSS:3.1/AV:N/AC:L", 0.0, false),
        ];
        for (vector, want, ok) in cases {
            let got = cvss_base_score(vector);
            assert_eq!(
                got.is_some(),
                *ok,
                "cvss_base_score({vector:?}) = {got:?}, want ({want}, {ok})"
            );
            if let Some(got) = got {
                assert_eq!(
                    got, *want,
                    "cvss_base_score({vector:?}) = {got}, want {want}"
                );
            }
        }
    }

    #[test]
    fn score_of_cases() {
        // CVSS vector -> computed base score.
        let v = OsvVuln {
            severity: vec![OsvSeverity {
                r#type: "CVSS_V3".into(),
                score: "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H".into(),
            }],
            ..Default::default()
        };
        assert_eq!(score_of(&v), "9.8", "score_of(vector)");
        // No severity entries -> empty.
        assert_eq!(score_of(&OsvVuln::default()), "", "score_of(empty)");
    }
}
