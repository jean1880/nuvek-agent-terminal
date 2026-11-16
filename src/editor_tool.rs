//! The external editor at run time: the one shared current editor (so every card and the diff
//! panel follow Settings at once), and opening a file in it at a line.
//!
//! The editor is run as an argv, never through a shell ([`agent_kit::editor`]). Resolving the
//! file, finding the program and starting it all run off the main thread. The file is resolved
//! to its canonical path (every link followed) and must be a regular file that is not
//! sensitive; a sensitive one is refused. One inside the thread's folder opens at once. One
//! outside it opens only after the user confirms that exact file, asked on every open, and only
//! if the path still resolves to it when the editor starts. With no editor configured the
//! system's handler for the file is asked instead (`gtk::FileLauncher`), which cannot jump to a
//! line. Logging names the editor, never a file's path.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use adw::prelude::*;
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
    /// The thread's folder: a file inside it opens at once, one outside it only once confirmed.
    pub folder: PathBuf,
    /// The file, absolute or relative to `folder`.
    pub path: String,
    /// Where to put the cursor, when known.
    pub line: Option<u32>,
    pub column: Option<u32>,
}

impl OpenRequest {
    /// A request for `location` (as a reply, a card or a link named it) in `folder`.
    pub fn at(folder: impl Into<PathBuf>, location: editor::Location) -> Self {
        Self {
            folder: folder.into(),
            path: location.path,
            line: location.line,
            column: location.column,
        }
    }
}

/// Why a sensitive file is not opened.
const SENSITIVE: &str = "That file holds configuration an agent's edits always ask about, so it \
                         is not opened from here";

/// Why a file outside the folder was not opened: it was not confirmed, or it is no longer the
/// file that was.
const NOT_CONFIRMED: &str =
    "That file is outside this thread's folder and was not confirmed, so it is not opened";

/// Resolves the request's file and refuses a sensitive one, inside the folder or out. Blocking.
pub fn judge(req: &OpenRequest) -> Result<Target, String> {
    let target = editor::resolve_target(&req.folder, &req.path)?;
    if target.sensitive {
        return Err(SENSITIVE.to_owned());
    }
    Ok(target)
}

/// [`judge`], then refuses a file outside the folder unless `confirmed` is exactly the file it
/// resolves to now: a link moved after the question was asked opens nothing. Blocking.
pub fn contain(req: &OpenRequest, confirmed: Option<&Path>) -> Result<Target, String> {
    let target = judge(req)?;
    if !target.inside && confirmed != Some(target.abs.as_path()) {
        return Err(NOT_CONFIRMED.to_owned());
    }
    Ok(target)
}

/// Everything short of starting the process: confines the file, finds the program, and fills
/// the argv. `confirmed` is the file outside the folder the user agreed to open, if any.
/// Blocking.
///
/// `locate` is `SystemProbe::locate`; only an absolute answer is used.
pub fn prepare(
    tool: &EditorTool,
    req: &OpenRequest,
    confirmed: Option<&Path>,
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
    let target = contain(req, confirmed)?;
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
    confirmed: Option<&Path>,
    locate: impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let prepared = prepare(tool, req, confirmed, locate)?;
    launch::spawn_detached("editor", &tool.name, &prepared)
}

/// The question asked before a file outside the thread's folder opens: its heading and body.
/// The body names the file the path resolved to (where a link leads, not the link).
pub fn outside_question(abs: &Path) -> (&'static str, String) {
    (
        "Open a File Outside This Folder?",
        format!(
            "{} is outside this thread's folder. The path came from the conversation, so check \
             that it is a file you mean to open.",
            abs.display()
        ),
    )
}

/// The steps of an open, with the question and the opening supplied: resolves and judges the
/// file off the main thread, asks `confirm` when it is outside the folder (on every open: no
/// answer is remembered), then calls `run` with the confirmed file, if any. A cancelled
/// question opens nothing and reports nothing.
pub async fn open_with<C, CF, R, RF>(req: OpenRequest, confirm: C, run: R) -> Result<(), String>
where
    C: FnOnce(PathBuf) -> CF,
    CF: std::future::Future<Output = bool>,
    R: FnOnce(OpenRequest, Option<PathBuf>) -> RF,
    RF: std::future::Future<Output = Result<(), String>>,
{
    let judged = {
        let req = req.clone();
        gio::spawn_blocking(move || judge(&req))
            .await
            .unwrap_or_else(|_| Err("resolving the file panicked".to_owned()))?
    };
    let confirmed = if judged.inside {
        None
    } else {
        if !confirm(judged.abs.clone()).await {
            return Ok(());
        }
        Some(judged.abs)
    };
    run(req, confirmed).await
}

/// Opens the file in the configured editor, else in the system's handler for it. A file outside
/// the folder is asked about first, over `parent` (else the active window). Failures are the
/// toast text. Everything that touches the disk or a process runs off the main thread.
pub async fn open_file(req: OpenRequest, parent: Option<gtk4::Widget>) -> Result<(), String> {
    let tool = EditorTools::shared().get();
    open_with(
        req,
        |abs| confirm_outside(parent, abs),
        |req, confirmed| async move {
            match tool {
                Some(tool) => open_in(tool, req, confirmed).await,
                None => open_default(req, confirmed).await,
            }
        },
    )
    .await
}

/// Asks whether to open `abs`, outside the thread's folder. Cancel is the default, and Escape or
/// a press outside the question answers it. With no window to ask over, the answer is no.
async fn confirm_outside(parent: Option<gtk4::Widget>, abs: PathBuf) -> bool {
    let parent = parent.filter(|p| p.root().is_some()).or_else(|| {
        gio::Application::default()
            .and_downcast::<gtk4::Application>()
            .and_then(|app| app.active_window())
            .map(|w| w.upcast())
    });
    let Some(parent) = parent else {
        return false;
    };
    let (heading, body) = outside_question(&abs);
    let dialog = crate::dialogs::confirm_dialog(
        heading,
        &body,
        ("cancel", "Cancel"),
        ("open", "Open"),
        adw::ResponseAppearance::Suggested,
        false,
    );
    crate::dialogs::confirmed(&dialog, &parent, "open").await
}

async fn open_in(
    tool: EditorTool,
    req: OpenRequest,
    confirmed: Option<PathBuf>,
) -> Result<(), String> {
    let (path, home, shell) = launch::probe_env();
    gio::spawn_blocking(move || {
        let probe = crate::utils::SystemProbe::new(path, home, shell);
        open_blocking(&tool, &req, confirmed.as_deref(), |c| probe.locate(c))
    })
    .await
    .unwrap_or_else(|_| Err("opening the editor panicked".to_owned()))
}

/// The system's handler: no line to jump to.
async fn open_default(req: OpenRequest, confirmed: Option<PathBuf>) -> Result<(), String> {
    let target = gio::spawn_blocking(move || contain(&req, confirmed.as_deref()))
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

/// Runs `open_file` for `req` on the main context (asking about a file outside the folder over
/// `parent`) and toasts a failure with `toast`.
pub fn open_with_toast(
    req: OpenRequest,
    parent: Option<gtk4::Widget>,
    toast: impl Fn(&str) + 'static,
) {
    glib::MainContext::default().spawn_local(async move {
        if let Err(why) = open_file(req, parent).await {
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

    /// The recorder's log once it has written one (it runs detached), within five seconds.
    fn wait_for_log(log: &Path) -> Option<String> {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(log) {
                if text.ends_with('\n') {
                    return Some(text);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
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
            None,
            |c| (c == "recorder").then(|| recorder.clone()),
        )
        .expect("open");
        let logged = wait_for_log(&f.log).expect("the editor ran");
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
        let err = prepare(&t, &request(&f, HOSTILE), None, |_| None).expect_err("missing");
        assert!(err.contains("not installed"), "{err}");
        // A bare name is not a location.
        assert!(prepare(&t, &request(&f, HOSTILE), None, |c| Some(c.to_owned())).is_err());
        // An invalid tool never reaches a process.
        assert!(prepare(
            &tool(&["recorder", "{nope}"]),
            &request(&f, HOSTILE),
            None,
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
    fn a_sensitive_file_is_refused_and_one_outside_opens_only_as_confirmed() {
        let f = fixture();
        let root = f.work.path();
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let outside = elsewhere
            .path()
            .canonicalize()
            .expect("canon")
            .join("o.txt");
        std::fs::write(&outside, "x").expect("outside");
        std::fs::create_dir(elsewhere.path().join(".git")).expect("git");
        let outside_hook = elsewhere.path().join(".git/config");
        std::fs::write(&outside_hook, "x").expect("config");
        std::fs::create_dir(root.join(".git")).expect("git");
        std::fs::write(root.join(".git/config"), "x").expect("config");
        std::os::unix::fs::symlink(&outside, root.join("leaves")).expect("link");
        std::os::unix::fs::symlink(root.join(".git/config"), root.join("innocent")).expect("link");
        assert!(contain(&request(&f, HOSTILE), None).is_ok());
        let t = tool(&["recorder", "{file}"]);
        let recorder = f.recorder.clone();
        let locate = |_: &str| Some(recorder.clone());

        // Never opened: missing, a folder, or sensitive, inside or out, confirmed or not (a link
        // is judged where it leads).
        for (bad, confirmed) in [
            ("missing.txt", None),
            (".git", None),
            (".git/config", None),
            ("innocent", None),
            (
                outside_hook.to_str().expect("utf8"),
                Some(outside_hook.as_path()),
            ),
        ] {
            assert!(
                prepare(&t, &request(&f, bad), confirmed, locate).is_err(),
                "{bad}"
            );
        }
        // Outside the folder: refused unconfirmed, or confirmed for another file; opened when
        // the confirmed file is where the path leads, by its path or through a link.
        for path in [outside.to_str().expect("utf8"), "leaves", "../escape"] {
            assert!(
                prepare(&t, &request(&f, path), None, locate).is_err(),
                "{path}"
            );
            assert!(
                prepare(
                    &t,
                    &request(&f, path),
                    Some(Path::new("/etc/hostname")),
                    locate
                )
                .is_err(),
                "{path}"
            );
        }
        let prepared = prepare(&t, &request(&f, "leaves"), Some(&outside), locate)
            .expect("the confirmed file opens");
        assert_eq!(prepared.args, [OsString::from(&outside)]);
        assert!(!f.log.exists(), "nothing was started");
    }

    /// The whole flow with the question answered by a stand-in: a file inside the folder is never
    /// asked about; one outside is asked about on every open, Cancel opens nothing, Open starts
    /// the editor on the file the question named; and a link moved after the answer opens
    /// nothing.
    #[test]
    fn a_file_outside_the_folder_is_asked_about_on_every_open() {
        use std::cell::{Cell, RefCell};
        let f = fixture();
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let outside = elsewhere
            .path()
            .canonicalize()
            .expect("canon")
            .join("o.txt");
        let other = elsewhere
            .path()
            .canonicalize()
            .expect("canon")
            .join("other.txt");
        std::fs::write(&outside, "x").expect("outside");
        std::fs::write(&other, "x").expect("other");
        let link = f.work.path().join("leaves");
        std::os::unix::fs::symlink(&outside, &link).expect("link");
        let t = tool(&["recorder", "{file}:{line}"]);
        let recorder = f.recorder.clone();
        let asked: RefCell<Vec<PathBuf>> = RefCell::default();
        let answer = Cell::new(false);
        let ctx = glib::MainContext::new();
        let open = |path: &str, before_run: &dyn Fn()| {
            ctx.block_on(open_with(
                request(&f, path),
                |abs| {
                    asked.borrow_mut().push(abs);
                    before_run();
                    std::future::ready(answer.get())
                },
                |req, confirmed| {
                    let (t, recorder) = (t.clone(), recorder.clone());
                    async move {
                        gio::spawn_blocking(move || {
                            open_blocking(&t, &req, confirmed.as_deref(), |_| {
                                Some(recorder.clone())
                            })
                        })
                        .await
                        .unwrap_or_else(|_| Err("panicked".to_owned()))
                    }
                },
            ))
        };

        // Inside: no question.
        open(HOSTILE, &|| {}).expect("inside opens");
        assert!(asked.borrow().is_empty());
        let logged = wait_for_log(&f.log).expect("the editor ran");
        std::fs::remove_file(&f.log).expect("reset");
        assert!(logged.ends_with(&format!("{HOSTILE}:42\n")), "{logged}");

        // Outside, cancelled twice: asked both times, nothing opened, nothing to report.
        assert_eq!(open("leaves", &|| {}), Ok(()));
        assert_eq!(open("leaves", &|| {}), Ok(()));
        assert_eq!(*asked.borrow(), [outside.clone(), outside.clone()]);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!f.log.exists(), "a cancelled question opens nothing");

        // Outside, confirmed: asked again (no answer is remembered), and the named file opens.
        answer.set(true);
        open("leaves", &|| {}).expect("confirmed opens");
        assert_eq!(asked.borrow().len(), 3);
        let logged = wait_for_log(&f.log).expect("the editor ran");
        std::fs::remove_file(&f.log).expect("reset");
        assert_eq!(logged, format!("{}:42\n", outside.display()));

        // The link moved to another file while the question was up: nothing opens.
        let moved = || {
            std::fs::remove_file(&link).expect("unlink");
            std::os::unix::fs::symlink(&other, &link).expect("relink");
        };
        let err = open("leaves", &moved).expect_err("a moved link opens nothing");
        assert_eq!(err, NOT_CONFIRMED);
        assert_eq!(asked.borrow().last(), Some(&outside));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!f.log.exists(), "nothing was started");

        // A sensitive file is refused before any question.
        std::fs::create_dir(elsewhere.path().join(".git")).expect("git");
        std::fs::write(elsewhere.path().join(".git/config"), "x").expect("config");
        let sensitive = elsewhere.path().join(".git/config");
        let asked_before = asked.borrow().len();
        assert_eq!(
            open(sensitive.to_str().expect("utf8"), &|| {}),
            Err(SENSITIVE.to_owned())
        );
        assert_eq!(asked.borrow().len(), asked_before, "never asked");

        // The question names where the path led, not the link.
        let (heading, body) = outside_question(&outside);
        assert_eq!(heading, "Open a File Outside This Folder?");
        assert!(body.starts_with(&outside.display().to_string()), "{body}");
    }
}
