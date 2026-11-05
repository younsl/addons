//! The versions of one package an advisory affects, built from OSV `SEMVER`
//! and `ECOSYSTEM` range events plus the explicit version list. It answers
//! whether a version is affected and renders the set as an npm semver range
//! (`>=1.0.0 <1.2.3 || <0.9.1`) for the npm bulk audit response.

use std::fmt::Write as _;

use semver::Version;

/// One OSV range event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Introduced(String),
    Fixed(String),
    LastAffected(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum End {
    Open,
    Fixed(Version),
    LastAffected(Version),
}

/// A contiguous affected span. `introduced` is `None` for OSV's "0" (every
/// version before the end).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Interval {
    introduced: Option<Version>,
    end: End,
}

impl Interval {
    fn contains(&self, v: &Version) -> bool {
        if self.introduced.as_ref().is_some_and(|lo| v < lo) {
            return false;
        }
        match &self.end {
            End::Open => true,
            End::Fixed(hi) => v < hi,
            End::LastAffected(hi) => v <= hi,
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        if let Some(lo) = &self.introduced {
            let _ = write!(out, ">={lo}");
        }
        let upper = match &self.end {
            End::Open => None,
            End::Fixed(hi) => Some(format!("<{hi}")),
            End::LastAffected(hi) => Some(format!("<={hi}")),
        };
        if let Some(upper) = upper {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&upper);
        }
        if out.is_empty() {
            out.push('*');
        }
        out
    }
}

/// The affected versions of one package for one advisory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Affected {
    intervals: Vec<Interval>,
    versions: Vec<String>,
}

impl Affected {
    /// Builds the set from OSV ranges (each a list of events) and explicit
    /// versions. A range holding a version that is not semver is dropped, so
    /// the advisory falls back to its explicit version list.
    pub fn new(ranges: Vec<Vec<Event>>, versions: Vec<String>) -> Self {
        let mut intervals = Vec::new();
        for events in ranges {
            if let Some(mut found) = sweep(events) {
                intervals.append(&mut found);
            }
        }
        Self {
            intervals,
            versions,
        }
    }

    /// Reports whether no version is known to be affected.
    pub const fn is_empty(&self) -> bool {
        self.intervals.is_empty() && self.versions.is_empty()
    }

    /// Reports whether `version` is affected.
    pub fn contains(&self, version: &str) -> bool {
        if self.versions.iter().any(|v| v == version) {
            return true;
        }
        parse(version).is_some_and(|v| self.intervals.iter().any(|i| i.contains(&v)))
    }

    /// Renders the set as an npm semver range. The explicit version list is
    /// used only when no range parsed, as OSV lists the same versions twice.
    pub fn npm_range(&self) -> String {
        if self.intervals.is_empty() {
            return self.versions.join(" || ");
        }
        self.intervals
            .iter()
            .map(Interval::render)
            .collect::<Vec<_>>()
            .join(" || ")
    }
}

fn parse(v: &str) -> Option<Version> {
    Version::parse(v.trim().trim_start_matches(['v', '='])).ok()
}

/// Turns one OSV range's events into intervals per the OSV evaluation order:
/// events sorted by version, an `introduced` opening a span and the next
/// `fixed` or `last_affected` closing it.
fn sweep(events: Vec<Event>) -> Option<Vec<Interval>> {
    let mut parsed: Vec<(Option<Version>, Event)> = Vec::with_capacity(events.len());
    for e in events {
        let raw = match &e {
            Event::Introduced(v) | Event::Fixed(v) | Event::LastAffected(v) => v,
        };
        let version = if matches!(e, Event::Introduced(_)) && raw.trim() == "0" {
            None
        } else {
            Some(parse(raw)?)
        };
        parsed.push((version, e));
    }
    parsed.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::new();
    let mut open: Option<Option<Version>> = None;
    for (version, event) in parsed {
        match (event, open.take()) {
            (Event::Introduced(_), None) => open = Some(version),
            (Event::Introduced(_), Some(lo)) => open = Some(lo),
            (Event::Fixed(_), Some(lo)) => out.push(Interval {
                introduced: lo,
                end: End::Fixed(version?),
            }),
            (Event::LastAffected(_), Some(lo)) => out.push(Interval {
                introduced: lo,
                end: End::LastAffected(version?),
            }),
            (Event::Fixed(_) | Event::LastAffected(_), None) => {}
        }
    }
    if let Some(lo) = open {
        out.push(Interval {
            introduced: lo,
            end: End::Open,
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intro(v: &str) -> Event {
        Event::Introduced(v.to_string())
    }
    fn fixed(v: &str) -> Event {
        Event::Fixed(v.to_string())
    }
    fn last(v: &str) -> Event {
        Event::LastAffected(v.to_string())
    }

    #[test]
    fn fixed_from_zero() {
        let a = Affected::new(vec![vec![intro("0"), fixed("1.16.1")]], vec![]);
        assert_eq!(a.npm_range(), "<1.16.1");
        assert!(a.contains("1.16.0"));
        assert!(!a.contains("1.16.1"));
        assert!(
            a.contains("1.16.1-rc.1"),
            "prerelease of the fix is affected"
        );
    }

    #[test]
    fn several_spans_and_last_affected() {
        let a = Affected::new(
            vec![
                vec![intro("8.0.0"), fixed("8.1.6")],
                vec![intro("7.0.0"), last("7.3.2")],
            ],
            vec![],
        );
        assert_eq!(a.npm_range(), ">=8.0.0 <8.1.6 || >=7.0.0 <=7.3.2");
        assert!(a.contains("8.0.16"));
        assert!(a.contains("7.3.2"));
        assert!(!a.contains("7.3.3"));
        assert!(!a.contains("8.1.6"));
        assert!(!a.contains("6.9.9"));
    }

    #[test]
    fn unsorted_events_in_one_range() {
        let a = Affected::new(
            vec![vec![
                fixed("2.0.1"),
                intro("2.0.0"),
                fixed("1.0.5"),
                intro("1.0.0"),
            ]],
            vec![],
        );
        assert_eq!(a.npm_range(), ">=1.0.0 <1.0.5 || >=2.0.0 <2.0.1");
        assert!(a.contains("1.0.4"));
        assert!(!a.contains("1.5.0"));
        assert!(a.contains("2.0.0"));
    }

    #[test]
    fn open_ended() {
        let a = Affected::new(vec![vec![intro("3.1.0")]], vec![]);
        assert_eq!(a.npm_range(), ">=3.1.0");
        assert!(a.contains("99.0.0"));
        assert!(!a.contains("3.0.9"));
        let all = Affected::new(vec![vec![intro("0")]], vec![]);
        assert_eq!(all.npm_range(), "*");
        assert!(all.contains("0.0.1"));
    }

    #[test]
    fn unparseable_range_falls_back_to_versions() {
        let a = Affected::new(
            vec![vec![intro("0"), fixed("not-a-version")]],
            vec!["1.0.0".to_string(), "1.0.1".to_string()],
        );
        assert_eq!(a.npm_range(), "1.0.0 || 1.0.1");
        assert!(a.contains("1.0.1"));
        assert!(!a.contains("1.0.2"));
        assert!(!a.is_empty());
        assert!(Affected::new(vec![], vec![]).is_empty());
    }

    #[test]
    fn loose_and_invalid_requested_versions() {
        let a = Affected::new(vec![vec![intro("0"), fixed("2.0.0")]], vec![]);
        assert!(a.contains("v1.2.3"));
        assert!(a.contains("=1.2.3"));
        assert!(!a.contains("latest"));
    }

    #[test]
    fn stray_end_events_are_ignored() {
        let a = Affected::new(vec![vec![fixed("1.0.0"), intro("2.0.0")]], vec![]);
        assert_eq!(a.npm_range(), ">=2.0.0");
        let twice = Affected::new(vec![vec![intro("1.0.0"), intro("1.5.0")]], vec![]);
        assert_eq!(twice.npm_range(), ">=1.0.0");
    }
}
