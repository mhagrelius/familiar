//! The Python sandbox against real podman.
//!
//! Everything in `model::sandbox`'s own tests is about the argv and the
//! framing, and everything in the eval suite is about what the model reaches
//! for — no test in either place ever starts a container. This is the seam
//! neither can reach: whether the invocation those tests assert about actually
//! isolates anything.
//!
//! Which is the part worth checking against the real thing, because the whole
//! reason `run_python` is ungated is four claims about that container. If
//! `--network=none` stopped working, every unit test here would still pass and
//! the security argument in `tools::gate_of` would be false.
//!
//! The whole file skips when podman or the image is missing, because a machine
//! that has not run `packaging/build-sandbox.sh` is not a machine with a bug.
//!
//! Two layers, and both are needed. The isolation checks drive podman through
//! `std::process`, because what they are about is the container and a main loop
//! would only be in the way. The last few go through `Runner` and gio, which is
//! the code that actually ships: it reads two pipes and works the exit status
//! out of a `Subprocess`, and if it got that wrong every failing script would
//! arrive framed as a finished calculation.

use std::path::Path;
use std::process::Command;

use familiar::model::sandbox::{self, Sandbox};

/// A sandbox over a temporary directory, with a workspace to read.
fn sandbox() -> Option<(tempfile::TempDir, tempfile::TempDir, Sandbox)> {
    if !available() {
        return None;
    }
    let root = tempfile::tempdir().expect("temp dir");
    let workspace = tempfile::tempdir().expect("temp dir");
    std::fs::write(workspace.path().join("rows.csv"), "n\n1\n2\n3\n4\n").expect("csv");

    let sandbox = Sandbox::new(root.path()).reading(Some(workspace.path().to_path_buf()));
    sandbox.prepare().expect("prepare");
    Some((root, workspace, sandbox))
}

/// Whether this machine can run the sandbox at all.
fn available() -> bool {
    let built = Command::new("podman")
        .args(["image", "exists", sandbox::IMAGE])
        .status();
    match built {
        Ok(status) if status.success() => true,
        Ok(_) => {
            eprintln!(
                "skipping: {} is not built — {}",
                sandbox::IMAGE,
                sandbox::BUILD_COMMAND
            );
            false
        }
        Err(_) => {
            eprintln!("skipping: podman is not installed");
            false
        }
    }
}

/// Run a script through the real argv and report what came back.
fn run(sandbox: &Sandbox, code: &str) -> sandbox::Ran {
    std::fs::write(sandbox.script_path(), code).expect("script");
    let before = sandbox.listing();

    let argv = sandbox.command();
    let started = std::time::Instant::now();
    let finished = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .expect("podman");

    let code = finished.status.code().unwrap_or(1);
    sandbox::Ran {
        stdout: String::from_utf8_lossy(&finished.stdout).to_string(),
        stderr: String::from_utf8_lossy(&finished.stderr).to_string(),
        timed_out: sandbox::killed_by_clock(code, started.elapsed().as_secs()),
        code,
        created: sandbox.listing().difference(&before).cloned().collect(),
    }
}

#[test]
fn a_script_runs_and_its_output_comes_back() {
    let Some((_root, _workspace, sandbox)) = sandbox() else {
        return;
    };
    let ran = run(&sandbox, "print('total:', sum(range(101)))\n");
    assert!(ran.finished(), "{ran:?}");
    assert_eq!(ran.stdout.trim(), "total: 5050");
    assert!(sandbox::frame(&ran).contains("5050"));
}

#[test]
fn the_libraries_the_tool_promises_are_all_there() {
    // The declaration names these to the model. A model that imports pandas
    // because it was told pandas is installed, and gets an ImportError, has
    // been lied to by its own tool description — and with no network it cannot
    // put that right.
    let Some((_root, _workspace, sandbox)) = sandbox() else {
        return;
    };
    let ran = run(
        &sandbox,
        "import numpy, pandas, scipy, sympy, matplotlib, openpyxl, docx, pptx, pypdf, \
         reportlab, PIL, dateutil, tabulate\nprint('all present')\n",
    );
    assert!(ran.finished(), "{}", ran.stderr);
    assert_eq!(ran.stdout.trim(), "all present");
}

#[test]
fn there_is_no_network_at_all() {
    // One of the four claims that make this tool safe to run ungated. Without
    // it the sandbox could read the workspace and post it somewhere.
    let Some((_root, _workspace, sandbox)) = sandbox() else {
        return;
    };
    let ran = run(
        &sandbox,
        "import socket\n\
         try:\n    socket.create_connection(('1.1.1.1', 80), timeout=3)\n    \
         print('REACHED THE NETWORK')\n\
         except OSError as error:\n    print('blocked:', type(error).__name__)\n",
    );
    assert!(ran.finished(), "{}", ran.stderr);
    assert!(
        ran.stdout.contains("blocked:"),
        "the sandbox reached the network: {}",
        ran.stdout
    );
}

#[test]
fn the_workspace_can_be_read_and_cannot_be_written() {
    // The other claim the design rests on: a script may compute over the
    // user's files, and every change to them still goes through a tool they
    // approve. A read-write mount here would make `write_file`'s approval
    // dialog decorative.
    let Some((_root, workspace, sandbox)) = sandbox() else {
        return;
    };
    let ran = run(
        &sandbox,
        "import pathlib\n\
         print('read:', pathlib.Path('/workspace/rows.csv').read_text().count('\\n'))\n\
         try:\n    pathlib.Path('/workspace/planted.txt').write_text('x')\n    \
         print('WROTE TO THE WORKSPACE')\n\
         except OSError as error:\n    print('refused:', type(error).__name__)\n",
    );
    assert!(ran.finished(), "{}", ran.stderr);
    assert!(ran.stdout.contains("read: 5"), "{}", ran.stdout);
    assert!(ran.stdout.contains("refused:"), "{}", ran.stdout);
    assert!(
        !workspace.path().join("planted.txt").exists(),
        "a script wrote into the user's workspace"
    );
}

#[test]
fn nothing_of_the_host_is_reachable_but_the_two_mounts() {
    // The container has a root filesystem of its own, so `/etc/shadow` in
    // there is the image's and says nothing about this machine — reading it
    // proves nothing either way, which is what the first version of this test
    // got wrong. What has to be true is that the *host's* paths are not
    // there: the sandbox may see its own directory and the workspace, and the
    // filesystem the app is running on is not either of them.
    let Some((_root, _workspace, sandbox)) = sandbox() else {
        return;
    };
    let home = std::env::var("HOME").expect("a home directory");
    let ran = run(
        &sandbox,
        &format!(
            "import pathlib\n\
             for path in ({home:?}, '/etc/hosts', '/root/.ssh'):\n    \
             print(path, pathlib.Path(path).exists())\n\
             print('os:', pathlib.Path('/etc/os-release').read_text().splitlines()[0])\n",
        ),
    );
    assert!(ran.finished(), "{}", ran.stderr);
    assert!(
        ran.stdout.contains(&format!("{home} False")),
        "the host's home directory is inside the container: {}",
        ran.stdout
    );
    assert!(
        ran.stdout.contains("/root/.ssh False"),
        "there are keys in the sandbox: {}",
        ran.stdout
    );
    // The root filesystem is the image's, which is what makes the paths above
    // meaningless rather than dangerous.
    assert!(
        ran.stdout.to_lowercase().contains("debian"),
        "the container is not running the image's own filesystem: {}",
        ran.stdout
    );
}

#[test]
fn the_directory_persists_between_calls_and_the_container_does_not() {
    // The claim the module header makes: state lives in the filesystem, and
    // each call is a fresh container. Both halves are checked here, because a
    // second call that could see the first one's *variables* would mean a
    // long-lived interpreter had crept in.
    let Some((root, _workspace, sandbox)) = sandbox() else {
        return;
    };
    let first = run(
        &sandbox,
        "import pathlib\nleft = 41\npathlib.Path('/work/kept.txt').write_text('42')\n\
         print('written')\n",
    );
    assert!(first.finished(), "{}", first.stderr);
    assert_eq!(first.created, ["kept.txt"]);
    assert!(root.path().join("kept.txt").is_file());

    let second = run(
        &sandbox,
        "import pathlib\nprint('file:', pathlib.Path('/work/kept.txt').read_text())\n\
         print('variable:', 'left' in dir())\n",
    );
    assert!(second.finished(), "{}", second.stderr);
    assert!(second.stdout.contains("file: 42"), "{}", second.stdout);
    assert!(
        second.stdout.contains("variable: False"),
        "a variable survived between calls, so something is keeping an interpreter alive: {}",
        second.stdout
    );
    // The second call created nothing, so nothing is reported as new.
    assert!(second.created.is_empty(), "{:?}", second.created);
}

#[test]
fn a_failing_script_reports_its_traceback_rather_than_an_empty_success() {
    let Some((_root, _workspace, sandbox)) = sandbox() else {
        return;
    };
    let ran = run(
        &sandbox,
        "print('before')\ntotals = {}\nprint(totals['x'])\n",
    );
    assert!(!ran.finished());
    // PYTHONUNBUFFERED in the image is what keeps the first line.
    assert!(ran.stdout.contains("before"), "{}", ran.stdout);
    assert!(ran.stderr.contains("KeyError"), "{}", ran.stderr);

    let framed = sandbox::frame(&ran);
    assert!(framed.contains("KeyError"), "{framed}");
    assert!(framed.contains("one corrected version"), "{framed}");
}

#[test]
fn an_image_that_is_not_built_says_so_and_says_it_immediately() {
    // `--pull=never` is why this is instant and legible. Without it podman
    // spends three seconds trying to reach a registry and reports a connection
    // error, which tells the model nothing it can act on.
    if !available() {
        return;
    }
    let root = tempfile::tempdir().expect("temp dir");
    let sandbox = Sandbox::new(root.path());
    sandbox.prepare().expect("prepare");
    std::fs::write(sandbox.script_path(), "print(1)\n").expect("script");

    let argv: Vec<String> = sandbox
        .command()
        .into_iter()
        .map(|word| {
            if word == sandbox::IMAGE {
                "localhost/familiar-sandbox-not-built:0".to_string()
            } else {
                word
            }
        })
        .collect();

    let started = std::time::Instant::now();
    let finished = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .expect("podman");
    let stderr = String::from_utf8_lossy(&finished.stderr).to_string();

    assert_eq!(
        sandbox::trouble(&stderr),
        Some(sandbox::Trouble::NoImage),
        "podman said something this does not recognise: {stderr}"
    );
    assert!(
        started.elapsed().as_secs() < 5,
        "an unbuilt image took {:?}, so it went looking for a registry",
        started.elapsed()
    );
}

/// Drive `Runner::run_python` the way the application drives it, and wait.
///
/// Everything above runs podman through `std::process`, which proves the
/// container and proves nothing about the code that ships: the app spawns
/// through gio, reads two pipes and works the exit code out of a `Subprocess`.
/// That path had no test at all, and it is where a wrong exit code would turn
/// every failing script into a silent success.
fn through_the_runner(sandbox: Sandbox, code: &str) -> familiar::model::turn::ToolOutcome {
    use familiar::model::turn::ToolCall;
    use familiar::ui::runner::Runner;
    use std::cell::RefCell;
    use std::rc::Rc;

    // A context of this thread's own, made the thread default for the duration.
    // gio refuses an async operation from a thread that does not own one, and
    // cargo runs these tests in parallel — so sharing the default context makes
    // whichever test starts second fail on a race rather than on anything to do
    // with the sandbox.
    let context = gtk::glib::MainContext::new();
    context
        .with_thread_default(|| {
            let main_loop = gtk::glib::MainLoop::new(Some(&context), false);
            let outcome = Rc::new(RefCell::new(None));

            let runner = Runner::new(Rc::new(RefCell::new(None)), None).with_sandbox(Some(sandbox));
            runner.run(
                &ToolCall {
                    id: "1".into(),
                    name: "run_python".into(),
                    arguments: serde_json::json!({ "code": code }).to_string(),
                    complete: true,
                    outcome: None,
                },
                {
                    let outcome = outcome.clone();
                    let main_loop = main_loop.clone();
                    move |result| {
                        outcome.replace(Some(result));
                        main_loop.quit();
                    }
                },
            );
            // A refusal is decided before podman is reached and answers before
            // `run` has returned, so waiting on the loop would wait for
            // something that has already happened. Every tool here may answer
            // either way — that is the contract `Runner::run` is written to —
            // and a harness that assumed one of them hangs on the other.
            if outcome.borrow().is_none() {
                main_loop.run();
            }
            let answer = outcome.borrow_mut().take();
            answer.expect("the runner answered")
        })
        .expect("a thread-default main context")
}

#[test]
fn the_runner_reports_what_a_script_printed() {
    let Some((_root, _workspace, sandbox)) = sandbox() else {
        return;
    };
    let outcome = through_the_runner(sandbox, "print('total:', 6 * 7)");
    let familiar::model::turn::ToolOutcome::Ok(said) = outcome else {
        panic!("a script that ran should not be a failure: {outcome:?}");
    };
    assert!(said.contains("total: 42"), "{said}");
    assert!(said.contains("That output is the answer"), "{said}");
}

#[test]
fn the_runner_does_not_report_a_crash_as_a_success() {
    // The exit code has to survive the trip through gio. If it did not, a
    // traceback would arrive framed as a finished calculation and the model
    // would answer from an empty result.
    let Some((_root, _workspace, sandbox)) = sandbox() else {
        return;
    };
    let outcome = through_the_runner(sandbox, "print('before')\nraise ValueError('nope')");
    let familiar::model::turn::ToolOutcome::Ok(said) = outcome else {
        panic!("a script that ran and failed is still a tool that ran: {outcome:?}");
    };
    assert!(said.contains("ValueError"), "{said}");
    assert!(said.contains("The script failed"), "{said}");
    assert!(!said.contains("That output is the answer"), "{said}");
}

#[test]
fn the_runner_refuses_an_empty_script_without_starting_a_container() {
    let Some((_root, _workspace, sandbox)) = sandbox() else {
        return;
    };
    let outcome = through_the_runner(sandbox, "   \n  ");
    let familiar::model::turn::ToolOutcome::Failed(why) = outcome else {
        panic!("an empty script should be refused: {outcome:?}");
    };
    assert!(why.contains("no code to run"), "{why}");
}

#[test]
fn copying_out_of_the_sandbox_cannot_reach_past_it() {
    // `copy_to_workspace` is the one seam between a sandbox that runs anything
    // and the user's own files, so the path it takes has to be checked the
    // same way every other workspace path is.
    use familiar::model::turn::{ToolCall, ToolOutcome};
    use familiar::ui::runner::Runner;
    use std::cell::RefCell;
    use std::rc::Rc;

    let Some((root, workspace, sandbox)) = sandbox() else {
        return;
    };
    std::fs::write(root.path().join("chart.png"), b"png").expect("chart");

    let runner = Runner::new(Rc::new(RefCell::new(None)), None)
        .with_sandbox(Some(sandbox))
        .with_workspace(Some(familiar::model::workspace::Workspace::new(
            workspace.path(),
        )));
    let copy = |from: &str, to: &str| {
        let outcome = Rc::new(RefCell::new(None));
        runner.run(
            &ToolCall {
                id: "1".into(),
                name: "copy_to_workspace".into(),
                arguments: serde_json::json!({ "from": from, "to": to }).to_string(),
                complete: true,
                outcome: None,
            },
            {
                let outcome = outcome.clone();
                move |result| {
                    outcome.replace(Some(result));
                }
            },
        );
        let answer = outcome.borrow_mut().take();
        answer.expect("an answer")
    };

    assert!(matches!(
        copy("chart.png", "reports/chart.png"),
        ToolOutcome::Ok(_)
    ));
    assert!(workspace.path().join("reports/chart.png").is_file());

    // Out of the sandbox at the reading end, and out of the workspace at the
    // writing end. Neither may be smuggled past with `..`.
    assert!(matches!(
        copy("../../../etc/passwd", "stolen.txt"),
        ToolOutcome::Failed(_)
    ));
    assert!(matches!(
        copy("chart.png", "../../../tmp/planted.png"),
        ToolOutcome::Failed(_)
    ));
    assert!(!workspace.path().join("stolen.txt").exists());
}

#[test]
fn the_script_path_is_inside_the_sandbox_and_nowhere_else() {
    // No podman needed: this is about the argv, and it is the one place a
    // path could point somewhere the mount does not cover.
    let root = tempfile::tempdir().expect("temp dir");
    let sandbox = Sandbox::new(root.path());
    let argv = sandbox.command();
    let script = argv.last().expect("a script path");
    assert!(script.starts_with("/work/"), "{script}");
    assert!(Path::new(script).is_absolute());
    assert!(
        sandbox.script_path().starts_with(root.path()),
        "the script is written outside the sandbox directory"
    );
}
