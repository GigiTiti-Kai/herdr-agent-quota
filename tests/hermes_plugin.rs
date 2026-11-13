//! The Hermes bridge plugin is Python that runs inside Hermes. Its own tests
//! are Python too; this runs them as part of `cargo test`.

use std::path::Path;
use std::process::Command;

/// Run one test command from the repository root and return how many tests it
/// ran. A command that ran nothing is a failure: a gate that silently runs no
/// tests is no gate.
fn run(program: &str, args: &[&str]) -> usize {
    let report = output(program, args);
    let ran = report
        .lines()
        .find_map(|line| line.strip_prefix("Ran ")?.split(' ').next()?.parse().ok())
        .unwrap_or_else(|| panic!("{args:?} reported no test count:\n{report}"));
    assert!(ran > 0, "{args:?} ran no tests");
    ran
}

fn output(program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .args(args)
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
        // Importing the plugin must not leave a __pycache__ in assets/.
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap_or_else(|error| {
            panic!("{program} is required for the Hermes plugin tests: {error}")
        });
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{args:?} failed:\n{report}");
    report
}

#[test]
fn the_bridge_plugin_unit_tests_pass() {
    let ran = run("python3", &["tests/hermes_plugin/test_bridge.py"]);
    assert!(ran >= 73, "only {ran} plugin tests ran");
}

/// `test_real_hermes.py` must run no test and write nothing when loaded outside the
/// sandbox; the guard tests point every child at a temporary HOME.
#[test]
fn the_real_hermes_tests_refuse_to_run_outside_the_sandbox() {
    let ran = run(
        "python3",
        &["tests/hermes_plugin/test_real_hermes_guard.py"],
    );
    assert!(ran >= 6, "only {ran} guard tests ran");
}

/// An earlier version of this file would run a Hermes interpreter straight on
/// the host when this variable was set. Running one outside a sandbox makes
/// Hermes' bootstrap download a runtime and rewrite the real launchers, so the
/// variable is refused rather than honoured or silently ignored.
#[test]
fn a_hermes_interpreter_on_the_host_is_refused() {
    assert!(
        std::env::var_os("HERDR_AGENT_QUOTA_HERMES_PYTHON").is_none(),
        "HERDR_AGENT_QUOTA_HERMES_PYTHON is refused: running Hermes outside a sandbox rewrites \
         its launchers. Use tests/hermes_plugin/real_hermes_sandbox.sh."
    );
}

/// The sandbox writes its fresh home and work directory into the scratch it is
/// given, so a scratch inside the Hermes home (or a missing one) is refused
/// before anything is created. HOME is a temporary directory, never the caller's.
#[test]
fn the_sandbox_refuses_a_scratch_inside_the_hermes_home() {
    let home = tempfile::tempdir().unwrap();
    let inside = home.path().join(".hermes/cache/scratch/run");
    std::fs::create_dir_all(&inside).unwrap();
    let linked = home.path().join("linked");
    std::os::unix::fs::symlink(&inside, &linked).unwrap();
    let missing = home.path().join("missing");
    let hermes = home.path().join(".hermes");
    for scratch in [&inside, &linked, &missing, &hermes] {
        let output = Command::new("bash")
            .arg("tests/hermes_plugin/real_hermes_sandbox.sh")
            .arg(scratch)
            .arg("/bin/true")
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")))
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{scratch:?} was not refused");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("outside"),
            "{scratch:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let names = |dir: &Path| -> Vec<_> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    };
    assert_eq!(names(&inside), Vec::<std::ffi::OsString>::new());
    assert_eq!(names(&hermes), ["cache"]);
}

/// The plugin inside the real Hermes checkout of this machine: real bootstrap,
/// discovery, hooks, agent, model switch and usage parser, in a bubblewrap
/// sandbox with no network and Hermes mounted read-only. The usage answer and
/// the notify executable are stubs.
///
/// `HERDR_AGENT_QUOTA_SANDBOX_SCRATCH=<existing dir outside ~/.hermes> cargo test -- --ignored`
#[test]
#[ignore = "needs a local Hermes install and bubblewrap"]
fn the_bridge_plugin_works_inside_real_hermes_in_a_sandbox() {
    let scratch = std::env::var("HERDR_AGENT_QUOTA_SANDBOX_SCRATCH")
        .expect("set HERDR_AGENT_QUOTA_SANDBOX_SCRATCH to an existing directory outside ~/.hermes");
    let sandbox = "tests/hermes_plugin/real_hermes_sandbox.sh";
    let python = "/usr/bin/python3";

    // The boundary first: nothing below means anything unless it holds.
    let probe = output(
        "bash",
        &[sandbox, &scratch, python, "/tests/sandbox_probe.py"],
    );
    assert!(probe.contains("PROBE PASS"), "{probe}");
    let refusal = output(
        "bash",
        &[sandbox, &scratch, "/bin/sh", "/tests/sandbox_refusal.sh"],
    );
    assert!(refusal.contains("REFUSAL OK"), "{refusal}");

    let real = [
        sandbox,
        &scratch,
        "--shadow-install-locks",
        python,
        "/tests/test_real_hermes.py",
    ];
    let ran = run("bash", &real);
    assert!(ran >= 12, "only {ran} real-Hermes tests ran");
}
