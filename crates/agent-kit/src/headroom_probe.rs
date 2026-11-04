//! Runs a Local server's headroom probe ([`agent_core::headroom_probe`]): the configured command,
//! exec'd directly (never through a shell), with a minimal environment, no input and a time
//! limit, and its output read as one number.
//!
//! Blocking: call it off the main thread.

use agent_core::headroom_probe::{reading, HeadroomProbe};

/// The variables a probe inherits from the app; nothing else of the app's environment (no
/// credential the app holds) reaches it.
const PASSED: [&str; 6] = [
    "HOME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "USER",
];

/// The probe's reading, or `None` when there is none: a probe that is not valid, cannot start,
/// runs past its time limit, exits unsuccessfully or prints no number. `path` is the `PATH` it
/// runs with (the user's shell's, when known), else the app's own.
pub fn run(probe: &HeadroomProbe, path: Option<&str>) -> Option<f64> {
    probe.validate().ok()?;
    let (program, args) = probe.argv.split_first()?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).env_clear();
    match path {
        Some(path) => {
            cmd.env("PATH", path);
        }
        None => {
            if let Some(path) = std::env::var_os("PATH") {
                cmd.env("PATH", path);
            }
        }
    }
    for name in PASSED {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    let output = crate::exec::run_command(cmd, "headroom probe", probe.timeout_s).ok()?;
    if !output.status.success() {
        return None;
    }
    reading(&output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(argv: &[&str], timeout_s: u64) -> HeadroomProbe {
        HeadroomProbe {
            argv: argv.iter().map(|a| (*a).to_owned()).collect(),
            low_below: 100.0,
            timeout_s,
        }
    }

    #[test]
    fn a_probe_that_prints_a_number_is_read() {
        assert_eq!(run(&probe(&["printf", "8123\\n"], 5), None), Some(8123.0));
    }

    #[test]
    fn its_arguments_reach_it_unparsed_by_any_shell() {
        // `$HOME` and `;` are plain text to printf: no shell expands or splits them.
        assert_eq!(run(&probe(&["printf", "%s", "1;$HOME"], 5), None), None);
        assert_eq!(run(&probe(&["printf", "%s", "42"], 5), None), Some(42.0));
    }

    #[test]
    fn no_reading_when_it_fails_is_missing_prints_text_or_runs_too_long() {
        assert_eq!(run(&probe(&["false"], 5), None), None);
        assert_eq!(run(&probe(&["sh", "-c", "echo 5; exit 3"], 5), None), None);
        assert_eq!(
            run(&probe(&["agent-terminal-no-such-probe-command"], 5), None),
            None
        );
        assert_eq!(run(&probe(&["echo", "busy"], 5), None), None);
        let started = std::time::Instant::now();
        assert_eq!(run(&probe(&["sleep", "10"], 1), None), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(
            run(&probe(&[], 5), None),
            None,
            "an invalid probe never runs"
        );
    }

    #[test]
    fn it_never_sees_the_apps_other_variables() {
        // The probe's whole environment: PATH, the passed names, and what `sh` adds itself
        // (`PWD`, `SHLVL`, `_` at most).
        let count = run(&probe(&["sh", "-c", "env | wc -l"], 5), None).expect("a count");
        assert!(count <= (PASSED.len() + 1 + 3) as f64, "{count} variables");
    }
}
