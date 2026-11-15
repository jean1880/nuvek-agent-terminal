//! The external editor at run time: the one shared current editor (so every card and the diff
//! panel follow Settings at once), and opening a file in it at a line.
//!
//! The editor is run as an argv, never through a shell ([`agent_kit::editor`]). Resolving the
//! file, finding the program and starting it all run off the main thread. The file is resolved
//! to its canonical path and must be a regular file inside the thread's folder and not
//! sensitive; anything else is refused. With no editor configured the system's handler for the
//! file is asked instead (`gtk::FileLauncher`), which cannot jump to a line. Logging names the
//! editor, never a file's path.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use agent_kit::editor::{self, EditorTool, Target, Values};
use gtk4::{gio, glib};

use crate::launch::{self, Prepared};
use crate::probe::ListenerSet;

/// The configured editor, shared by every window.
#[derive(Default)]
pub struct EditorTools {
    current: RefCell<Option<EditorTool>>,
    listeners: ListenerSet,
}

thread_local! {
    static SHARED: std::rc::Rc<EditorTools> = std::rc::Rc::default();
}

impl EditorTools {
    pub fn shared() -> std::rc::Rc<EditorTools> {
        SHARED.with(std::rc::Rc::clone)
    }

    /// The editor to open files in, when one is configured and valid. One that fails validation
    /// (a hand edit) reads as none: the buttons fall back to the system handler rather than fail.
    pub fn get(&self) -> Option<EditorTool> {
        self.current
            .borrow()
            .clone()
            .filter(|t| editor::validate(t).is_ok())
    }

    /// Replaces the editor and tells the listeners when it changed.
    pub fn set(&self, tool: Option<EditorTool>) {
        if *self.current.borrow() == tool {
            return;
        }
        *self.current.borrow_mut() = tool;
        self.listeners.notify();
    }

    /// Calls `f` (on the main thread) whenever the editor changes; the id disconnects it.
    pub fn connect_changed(&self, f: impl Fn() + 'static) -> u64 {
        self.listeners.add(f)
    }

    pub fn disconnect(&self, id: u64) {
        self.listeners.remove(id);
    }
}

/// The button label: "Open in Zed".
pub fn open_label(tool: &EditorTool) -> String {
    format!("Open in {}", tool.name)
}

/// The button label with no editor configured.
pub const DEFAULT_LABEL: &str = "Open";

/// The tooltip a button shows when no editor is configured.
pub const NO_EDITOR_HINT: &str =
    "Opens in the system's default app, which cannot jump to a line. Set an editor in Settings \
     for line jumps";

/// One file to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// The thread's folder: the file must be inside it.
    pub folder: PathBuf,
    /// The file, absolute or relative to `folder`.
    pub path: String,
    /// Where to put the cursor, when known.
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// Resolves the request's file and refuses one outside the folder or a sensitive one.
/// Blocking.
pub fn contain(req: &OpenRequest) -> Result<Target, String> {
    let target = editor::resolve_target(&req.folder, &req.path)?;
    if !target.inside {
        return Err("That file is outside this thread's folder, so it is not opened".to_owned());
    }
    if target.sensitive {
        return Err(
            "That file holds configuration an agent's edits always ask about, so it is not \
             opened from here"
                .to_owned(),
        );
    }
    Ok(target)
}

/// Everything short of starting the process: confines the file, finds the program, and fills
/// the argv. Blocking.
///
/// `locate` is `SystemProbe::locate`; only an absolute answer is used.
pub fn prepare(
    tool: &EditorTool,
    req: &OpenRequest,
    locate: impl Fn(&str) -> Option<String>,
) -> Result<Prepared, String> {
    editor::validate(tool)?;
    let program_name = tool.argv.first().map(String::as_str).unwrap_or_default();
    let program = locate(program_name)
        .filter(|p| Path::new(p).is_absolute())
        .ok_or_else(|| {
            format!(
                "{} is not installed ({program_name} was not found)",
                tool.name
            )
        })?;
    let target = contain(req)?;
    let dir = req
        .folder
        .canonicalize()
        .map_err(|e| format!("Could not resolve the thread's folder: {e}"))?;
    let values = Values {
        file: target.abs.into_os_string(),
        line: req.line.unwrap_or(1),
        column: req.column.unwrap_or(1),
        dir: dir.clone().into_os_string(),
    };
    let mut argv = editor::substitute(tool, &values)?.into_iter();
    argv.next(); // the program as written; the located path replaces it
    Ok(Prepared {
        program: PathBuf::from(program),
        args: argv.collect::<Vec<OsString>>(),
        cwd: dir,
    })
}

/// The whole open, blocking: [`prepare`] then start it.
pub fn open_blocking(
    tool: &EditorTool,
    req: &OpenRequest,
    locate: impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let prepared = prepare(tool, req, locate)?;
    launch::spawn_detached("editor", &tool.name, &prepared)
}

/// Opens the file in the configured editor, else in the system's handler for it. Failures are
/// the toast text. Everything that touches the disk or a process runs off the main thread.
pub async fn open_file(req: OpenRequest) -> Result<(), String> {
    match EditorTools::shared().get() {
        Some(tool) => open_in(tool, req).await,
        None => open_default(req).await,
    }
}

async fn open_in(tool: EditorTool, req: OpenRequest) -> Result<(), String> {
    let (path, home, shell) = launch::probe_env();
    gio::spawn_blocking(move || {
        let probe = crate::utils::SystemProbe::new(path, home, shell);
        open_blocking(&tool, &req, |c| probe.locate(c))
    })
    .await
    .unwrap_or_else(|_| Err("opening the editor panicked".to_owned()))
}

/// The system's handler: no line to jump to.
async fn open_default(req: OpenRequest) -> Result<(), String> {
    let target = gio::spawn_blocking(move || contain(&req))
        .await
        .unwrap_or_else(|_| Err("resolving the file panicked".to_owned()))?;
    let launcher = gtk4::FileLauncher::new(Some(&gio::File::for_path(&target.abs)));
    match launcher.launch_future(None::<&gtk4::Window>).await {
        Ok(()) => Ok(()),
        // The user closed the chooser: nothing to report.
        Err(e) if e.matches(gtk4::DialogError::Dismissed) => Ok(()),
        Err(e) => Err(format!("Could not open the file: {}", e.message())),
    }
}

/// The presets that are installed, by index into [`editor::PRESETS`], found off the main thread
/// (the probe may start the user's shell). Only absolute locations count.
pub async fn installed_presets() -> Vec<bool> {
    let (path, home, shell) = launch::probe_env();
    gio::spawn_blocking(move || {
        let probe = crate::utils::SystemProbe::new(path, home, shell);
        editor::PRESETS
            .iter()
            .map(|p| p.is_installed(|c| probe.locate(c)))
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Runs `open_file` for `req` on the main context and toasts a failure with `toast`.
pub fn open_with_toast(req: OpenRequest, toast: impl Fn(&str) + 'static) {
    glib::MainContext::default().spawn_local(async move {
        if let Err(why) = open_file(req).await {
            tracing::info!("the editor did not open: {why}");
            toast(&why);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A thread folder with a file with a hostile name, plus a tool that records its argv.
    struct Fixture {
        work: tempfile::TempDir,
        run: tempfile::TempDir,
        recorder: String,
        log: PathBuf,
    }

    const HOSTILE: &str = "a b;$(touch pwned)`x`.txt";

    fn fixture() -> Fixture {
        let work = tempfile::tempdir().expect("work");
        std::fs::write(work.path().join(HOSTILE), "x").expect("file");
        let run = tempfile::tempdir().expect("run");
        let log = run.path().join("argv.log");
        let recorder = run.path().join("recorder");
        std::fs::write(
            &recorder,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{}'; done\n",
                log.display()
            ),
        )
        .expect("script");
        std::fs::set_permissions(&recorder, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Fixture {
            work,
            run,
            recorder: recorder.display().to_string(),
            log,
        }
    }

    fn tool(argv: &[&str]) -> EditorTool {
        EditorTool {
            name: "Recorder".into(),
            argv: argv.iter().map(|s| (*s).to_owned()).collect(),
            terminal: false,
        }
    }

    fn request(f: &Fixture, path: &str) -> OpenRequest {
        OpenRequest {
            folder: f.work.path().to_owned(),
            path: path.to_owned(),
            line: Some(42),
            column: None,
        }
    }

    #[test]
    fn the_editor_is_started_with_each_element_as_one_argument() {
        let f = fixture();
        let recorder = f.recorder.clone();
        let root = f.work.path().canonicalize().expect("canon");
        open_blocking(
            &tool(&[
                "recorder",
                "$(touch /tmp/pwned-by-editor-test)",
                "{file}:{line}:{column}",
                "--dir={dir}",
                "a;b",
            ]),
            &request(&f, HOSTILE),
            |c| (c == "recorder").then(|| recorder.clone()),
        )
        .expect("open");
        let mut waited = 0;
        while !f.log.exists() && waited < 100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            waited += 1;
        }
        let logged = std::fs::read_to_string(&f.log).expect("the editor ran");
        assert_eq!(
            logged,
            format!(
                "$(touch /tmp/pwned-by-editor-test)\n{}/{HOSTILE}:42:1\n--dir={}\na;b\n",
                root.display(),
                root.display()
            )
        );
        assert!(!Path::new("/tmp/pwned-by-editor-test").exists());
        assert!(!root.join("pwned").exists());
        drop(f.run);
    }

    #[test]
    fn a_missing_program_is_an_error_and_a_hand_edited_tool_reads_as_none() {
        let f = fixture();
        let t = tool(&["nonesuch", "{file}"]);
        let err = prepare(&t, &request(&f, HOSTILE), |_| None).expect_err("missing");
        assert!(err.contains("not installed"), "{err}");
        // A bare name is not a location.
        assert!(prepare(&t, &request(&f, HOSTILE), |c| Some(c.to_owned())).is_err());
        // An invalid tool never reaches a process.
        assert!(prepare(
            &tool(&["recorder", "{nope}"]),
            &request(&f, HOSTILE),
            |_| Some(f.recorder.clone())
        )
        .is_err());

        let tools = EditorTools::default();
        let seen = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = seen.clone();
        tools.connect_changed(move || counter.set(counter.get() + 1));
        tools.set(Some(tool(&["zed", "{file}"])));
        tools.set(Some(tool(&["zed", "{file}"])));
        assert_eq!(
            seen.get(),
            1,
            "setting the same editor again is not a change"
        );
        assert!(tools.get().is_some());
        tools.set(Some(tool(&["zed", "{bogus}"])));
        assert_eq!(tools.get(), None);
        assert_eq!(open_label(&tool(&["zed", "{file}"])), "Open in Recorder");
    }

    #[test]
    fn only_a_regular_file_inside_the_folder_that_is_not_sensitive_opens() {
        let f = fixture();
        let root = f.work.path();
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        std::fs::write(elsewhere.path().join("o.txt"), "x").expect("outside");
        std::fs::create_dir(root.join(".git")).expect("git");
        std::fs::write(root.join(".git/config"), "x").expect("config");
        std::os::unix::fs::symlink(elsewhere.path().join("o.txt"), root.join("leaves"))
            .expect("link");
        assert!(contain(&request(&f, HOSTILE)).is_ok());
        let outside = elsewhere.path().join("o.txt");
        for bad in [
            outside.to_str().expect("utf8"),
            "leaves",
            "../escape",
            ".git/config",
            "missing.txt",
        ] {
            let recorder = f.recorder.clone();
            let t = tool(&["recorder", "{file}"]);
            assert!(
                prepare(&t, &request(&f, bad), |_| Some(recorder.clone())).is_err(),
                "{bad}"
            );
        }
        assert!(!f.log.exists(), "nothing was started");
    }
}
