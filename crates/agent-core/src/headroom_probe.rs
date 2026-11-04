//! A Local server's headroom probe: a command the user sets per server that prints one number
//! (the free memory of the GPU it runs on, say), read against a threshold the user sets too. A
//! reading under the threshold means the server is busy (headroom `low`), so delegation routes
//! work elsewhere; a reading at or over it is `ok`; no reading (the probe is missing, fails, times
//! out or prints no number), or one too old, is `unknown`, never healthy.
//!
//! The command is run directly, never through a shell, so its arguments are taken as they are.
//! Nothing here names a command or a unit: both are the user's.

use serde::{Deserialize, Serialize};

use crate::model_tags::Headroom;

/// How long a probe may run when its setting names no time.
pub const DEFAULT_TIMEOUT_S: u64 = 5;
/// The longest a probe may be given.
pub const MAX_TIMEOUT_S: u64 = 30;
/// The most arguments a probe command may have, its program included.
pub const MAX_ARGS: usize = 32;
/// The longest one argument may be.
pub const MAX_ARG_BYTES: usize = 4096;
/// How much of a probe's output is read for its number.
pub const MAX_OUTPUT_BYTES: usize = 4096;

/// `Profile::headroom_probe`: the command and the reading under which the server counts as busy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeadroomProbe {
    /// The program and its arguments, run directly (no shell).
    pub argv: Vec<String>,
    /// A reading under this is headroom `low`; at or over it, `ok`.
    pub low_below: f64,
    /// Seconds the probe may run before it is killed and read as no reading.
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_S
}

/// Compared by value, the threshold bit for bit (a valid one is finite, so this is equality).
impl PartialEq for HeadroomProbe {
    fn eq(&self, other: &Self) -> bool {
        self.argv == other.argv
            && self.low_below.to_bits() == other.low_below.to_bits()
            && self.timeout_s == other.timeout_s
    }
}

impl Eq for HeadroomProbe {}

impl HeadroomProbe {
    /// Why this probe cannot run, if it cannot: no program, too many or too long arguments, a
    /// NUL byte, a threshold that is not a finite number, or a time outside 1 to
    /// [`MAX_TIMEOUT_S`] seconds. A probe that fails this is never run; its headroom is unknown.
    pub fn validate(&self) -> Result<(), String> {
        match self.argv.first() {
            Some(program) if !program.trim().is_empty() => {}
            _ => return Err("The probe needs a command to run.".into()),
        }
        if self.argv.len() > MAX_ARGS {
            return Err(format!(
                "The probe command may have at most {MAX_ARGS} words."
            ));
        }
        if self.argv.iter().any(|a| a.len() > MAX_ARG_BYTES) {
            return Err(format!(
                "Each word of the probe command may be at most {MAX_ARG_BYTES} bytes."
            ));
        }
        if self.argv.iter().any(|a| a.contains('\0')) {
            return Err("The probe command cannot hold a NUL byte.".into());
        }
        if !self.low_below.is_finite() {
            return Err("Low Below must be a number.".into());
        }
        if !(1..=MAX_TIMEOUT_S).contains(&self.timeout_s) {
            return Err(format!(
                "The probe's time limit must be 1 to {MAX_TIMEOUT_S} seconds."
            ));
        }
        Ok(())
    }

    /// The headroom a `reading` taken `age_s` seconds ago shows: `unknown` without a reading,
    /// without an age, or with one past `max_age_s`.
    pub fn headroom(&self, reading: Option<f64>, age_s: Option<i64>, max_age_s: i64) -> Headroom {
        let fresh = age_s.is_some_and(|age| (0..=max_age_s).contains(&age));
        match reading {
            Some(value) if fresh && value.is_finite() && self.low_below.is_finite() => {
                if value < self.low_below {
                    Headroom::Low
                } else {
                    Headroom::Ok
                }
            }
            _ => Headroom::Unknown,
        }
    }
}

/// The load a probed server reports, to rank it among agents: none while `ok` (a free server
/// costs nothing), full while `low` (busy: ranked last, and left out by routing), and null
/// exactly when its headroom is `unknown`.
pub fn load(headroom: Headroom) -> Option<f64> {
    match headroom {
        Headroom::Ok => Some(0.0),
        Headroom::Low | Headroom::Exhausted => Some(1.0),
        Headroom::Unknown => None,
    }
}

/// A probe's reading from what it printed: the first line that is not blank, trimmed, as a
/// finite number. Anything else (no such line, a word, `NaN`) is no reading. Only the first
/// [`MAX_OUTPUT_BYTES`] are looked at.
pub fn reading(stdout: &[u8]) -> Option<f64> {
    let head = &stdout[..stdout.len().min(MAX_OUTPUT_BYTES)];
    let text = String::from_utf8_lossy(head);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    line.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// The command as Settings shows it: its words joined by spaces.
pub fn command_line(argv: &[String]) -> String {
    argv.join(" ")
}

/// What Settings stores for the command line `command` and the threshold `low`, keeping the
/// time limit of the probe saved now: no probe when the command is blank, else the probe, or why
/// the two cannot make one. The command is split on whitespace (a word with a space in it needs a
/// small script of its own).
pub fn edit(
    command: &str,
    low: &str,
    saved: Option<&HeadroomProbe>,
) -> Result<Option<HeadroomProbe>, String> {
    let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
    if argv.is_empty() {
        return Ok(None);
    }
    let low = low.trim();
    if low.is_empty() {
        return Err("Enter Low Below too: a reading under it means the server is busy.".into());
    }
    let low_below = low
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| "Low Below must be a number, e.g. 4096.".to_owned())?;
    let probe = HeadroomProbe {
        argv,
        low_below,
        timeout_s: saved.map_or(DEFAULT_TIMEOUT_S, |s| s.timeout_s),
    };
    probe.validate()?;
    Ok(Some(probe))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(low_below: f64) -> HeadroomProbe {
        HeadroomProbe {
            argv: vec!["probe-free".into(), "--unit".into(), "mib".into()],
            low_below,
            timeout_s: DEFAULT_TIMEOUT_S,
        }
    }

    #[test]
    fn a_reading_under_the_threshold_is_low_and_at_or_over_it_ok() {
        let p = probe(4096.0);
        assert_eq!(p.headroom(Some(1024.0), Some(5), 1200), Headroom::Low);
        assert_eq!(p.headroom(Some(4096.0), Some(5), 1200), Headroom::Ok);
        assert_eq!(p.headroom(Some(9000.0), Some(0), 1200), Headroom::Ok);
    }

    #[test]
    fn no_reading_or_an_old_one_is_unknown_never_healthy() {
        let p = probe(4096.0);
        assert_eq!(p.headroom(None, Some(5), 1200), Headroom::Unknown);
        assert_eq!(p.headroom(Some(9000.0), None, 1200), Headroom::Unknown);
        assert_eq!(
            p.headroom(Some(9000.0), Some(1201), 1200),
            Headroom::Unknown
        );
        assert_eq!(p.headroom(Some(9000.0), Some(-1), 1200), Headroom::Unknown);
        assert_eq!(p.headroom(Some(f64::NAN), Some(1), 1200), Headroom::Unknown);
    }

    #[test]
    fn load_follows_headroom() {
        assert_eq!(load(Headroom::Ok), Some(0.0));
        assert_eq!(load(Headroom::Low), Some(1.0));
        assert_eq!(load(Headroom::Unknown), None);
    }

    #[test]
    fn the_reading_is_the_first_line_with_text_as_a_number() {
        assert_eq!(reading(b"8123\n"), Some(8123.0));
        assert_eq!(reading(b"\n  \n 12.5 \nignored"), Some(12.5));
        assert_eq!(reading(b"-3"), Some(-3.0));
        assert_eq!(reading(b"8123 MiB\n"), None, "one number, nothing else");
        assert_eq!(reading(b"NaN"), None);
        assert_eq!(reading(b"inf"), None);
        assert_eq!(reading(b""), None);
        assert_eq!(reading(b"\xff\xfe"), None);
        let mut late = vec![b'\n'; MAX_OUTPUT_BYTES];
        late.extend_from_slice(b"5");
        assert_eq!(reading(&late), None, "only the head is read");
    }

    #[test]
    fn a_probe_that_cannot_run_says_why() {
        assert!(probe(1.0).validate().is_ok());
        let bad = |edit: fn(&mut HeadroomProbe)| {
            let mut p = probe(1.0);
            edit(&mut p);
            p.validate().is_err()
        };
        assert!(bad(|p| p.argv.clear()));
        assert!(bad(|p| p.argv[0] = "  ".into()));
        assert!(bad(|p| p.argv = vec!["a".into(); MAX_ARGS + 1]));
        assert!(bad(|p| p.argv[1] = "x".repeat(MAX_ARG_BYTES + 1)));
        assert!(bad(|p| p.argv[1] = "a\0b".into()));
        assert!(bad(|p| p.low_below = f64::INFINITY));
        assert!(bad(|p| p.timeout_s = 0));
        assert!(bad(|p| p.timeout_s = MAX_TIMEOUT_S + 1));
    }

    #[test]
    fn settings_store_a_probe_only_with_a_command_and_a_number() {
        assert_eq!(edit("  ", "abc", None), Ok(None), "blank: no probe");
        let made = edit(" probe-free  --unit mib ", "4096", None).expect("valid");
        assert_eq!(made, Some(probe(4096.0)));
        assert!(edit("probe-free", "", None)
            .unwrap_err()
            .contains("Low Below"));
        assert!(edit("probe-free", "lots", None)
            .unwrap_err()
            .contains("number"));
        let saved = HeadroomProbe {
            timeout_s: 12,
            ..probe(1.0)
        };
        let kept = edit("probe-free", "2", Some(&saved))
            .expect("valid")
            .expect("some");
        assert_eq!(kept.timeout_s, 12, "the saved time limit stays");
        assert_eq!(command_line(&saved.argv), "probe-free --unit mib");
    }

    #[test]
    fn the_setting_reads_and_writes_its_json_shape() {
        let v: HeadroomProbe =
            serde_json::from_str(r#"{"argv":["probe-free"],"low_below":4096}"#).expect("parse");
        assert_eq!(
            v.timeout_s, DEFAULT_TIMEOUT_S,
            "an absent time is the default"
        );
        assert_eq!(
            serde_json::to_value(&v).expect("ser"),
            serde_json::json!({"argv":["probe-free"],"low_below":4096.0,"timeout_s":5})
        );
    }
}
