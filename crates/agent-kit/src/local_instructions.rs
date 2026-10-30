//! Bounded, descriptor-checked instruction discovery. Call off the UI thread.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use agent_core::local_context::{
    InstructionDocument, InstructionScope, InstructionSettings, InstructionSnapshot,
    InstructionSource,
};

#[derive(Debug, Clone, Default)]
pub struct InstructionPaths {
    pub home: Option<PathBuf>,
    pub config_home: Option<PathBuf>,
}

impl InstructionPaths {
    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os("HOME").map(PathBuf::from),
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        )
    }

    pub fn new(home: Option<PathBuf>, config_home: Option<PathBuf>) -> Self {
        let home = home.filter(|p| p.is_absolute());
        let config_home = config_home
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|p| p.join(".config")));
        Self { home, config_home }
    }

    fn expand(&self, path: &str, cwd: &Path) -> Result<PathBuf, String> {
        let path = if let Some(rest) = path
            .strip_prefix("~/")
            .or_else(|| path.strip_prefix("$HOME/"))
            .or_else(|| path.strip_prefix("${HOME}/"))
        {
            self.home
                .as_ref()
                .ok_or("HOME is unavailable for an instruction path.")?
                .join(rest)
        } else if let Some(rest) = path
            .strip_prefix("$XDG_CONFIG_HOME/")
            .or_else(|| path.strip_prefix("${XDG_CONFIG_HOME}/"))
        {
            self.config_home
                .as_ref()
                .ok_or("XDG configuration home is unavailable for an instruction path.")?
                .join(rest)
        } else {
            if path.starts_with('~') || path.contains('$') {
                return Err(format!("Unsupported instruction path expansion: {path}"));
            }
            PathBuf::from(path)
        };
        Ok(if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        })
    }
}

fn problem(path: &Path, reason: impl std::fmt::Display) -> String {
    format!(
        "Instruction file {}: {reason}. Increase budget or deselect these files.",
        path.display()
    )
}

/// Metadata is checked on the opened descriptor; `take(cap + 1)` also catches growth.
fn read_regular(file: File, path: &Path, cap: usize) -> Result<String, String> {
    let meta = file.metadata().map_err(|e| problem(path, e))?;
    if !meta.is_file() {
        return Err(problem(path, "not a regular file"));
    }
    if meta.len() > cap as u64 {
        return Err(problem(path, format!("exceeds the {cap}-byte file cap")));
    }
    read_bounded(file, path, cap)
}

fn read_bounded(reader: impl Read, path: &Path, cap: usize) -> Result<String, String> {
    let mut bytes = Vec::new();
    reader
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| problem(path, e))?;
    if bytes.len() > cap {
        return Err(problem(
            path,
            format!("grew beyond the {cap}-byte file cap"),
        ));
    }
    String::from_utf8(bytes).map_err(|_| problem(path, "not valid UTF-8"))
}

struct Discovery<'a> {
    settings: &'a InstructionSettings,
    seen: BTreeSet<(u64, u64)>,
    total: usize,
    snapshot: InstructionSnapshot,
}

impl Discovery<'_> {
    /// True also for a deduplicated file: it still wins the directory's precedence choice.
    fn load(
        &mut self,
        path: &Path,
        scope: InstructionScope,
        required: bool,
    ) -> Result<bool, String> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling projection is unreadable, rather than an absent optional file.
                if std::fs::symlink_metadata(path).is_ok() {
                    return Err(problem(path, e));
                }
                return Ok(false);
            }
            Err(e) => return Err(problem(path, e)),
        };
        let meta = file.metadata().map_err(|e| problem(path, e))?;
        if !meta.is_file() {
            return Err(problem(path, "not a regular file"));
        }
        if !self.seen.insert((meta.dev(), meta.ino())) {
            return Ok(true);
        }
        let text = read_regular(file, path, self.settings.max_file_bytes)?;
        self.total = self.total.saturating_add(text.len());
        if self.total > self.settings.max_total_bytes {
            return Err(problem(
                path,
                format!(
                    "aggregate instructions exceed {} bytes",
                    self.settings.max_total_bytes
                ),
            ));
        }
        let resolved = std::fs::canonicalize(path).map_err(|e| problem(path, e))?;
        let resolved_meta = std::fs::metadata(&resolved).map_err(|e| problem(path, e))?;
        if (resolved_meta.dev(), resolved_meta.ino()) != (meta.dev(), meta.ino()) {
            return Err(problem(
                path,
                "changed identity during discovery; retry the turn",
            ));
        }
        self.snapshot.documents.push(InstructionDocument::new(
            InstructionSource {
                path: resolved.display().to_string(),
                scope,
                bytes: text.len(),
            },
            &text,
        ));
        Ok(true)
    }
}

pub fn discover(
    settings: &InstructionSettings,
    cwd: &Path,
    paths: &InstructionPaths,
) -> Result<InstructionSnapshot, String> {
    settings.validate()?;
    let cwd = std::fs::canonicalize(cwd)
        .map_err(|e| format!("Cannot discover instructions for {}: {e}", cwd.display()))?;
    if !cwd.is_dir() {
        return Err("The instruction folder is not a directory.".into());
    }
    let mut discovery = Discovery {
        settings,
        seen: BTreeSet::new(),
        total: 0,
        snapshot: InstructionSnapshot::default(),
    };
    if let Some(config) = &paths.config_home {
        discovery.load(
            &config.join("agent-terminal/AGENTS.md"),
            InstructionScope::NativeGlobal,
            false,
        )?;
    }
    for file in &settings.global_files {
        discovery.load(&paths.expand(file, &cwd)?, InstructionScope::Global, true)?;
    }
    if settings.discover_ancestors {
        let ancestors: Vec<_> = cwd.ancestors().collect();
        for dir in ancestors.into_iter().rev() {
            if discovery.load(
                &dir.join("AGENTS.override.md"),
                InstructionScope::Project,
                false,
            )? {
                continue;
            }
            if discovery.load(&dir.join("AGENTS.md"), InstructionScope::Project, false)? {
                continue;
            }
            if settings.claude_fallback {
                discovery.load(&dir.join("CLAUDE.md"), InstructionScope::Project, false)?;
            }
        }
    }
    Ok(discovery.snapshot)
}

/// The same discovery, composition and planner as a real turn; only source metadata is shown.
pub fn preview(
    context: &agent_core::local_context::LocalLaunchContext,
    cwd: &Path,
    paths: &InstructionPaths,
    mode: agent_core::adapter::Mode,
) -> Result<String, String> {
    use agent_core::local_context::{compose_system, plan_request, SchemaCandidate};
    context.validate()?;
    let snapshot = discover(&context.instructions, cwd, paths)?;
    let canonical =
        std::fs::canonicalize(cwd).map_err(|e| format!("Cannot resolve preview folder: {e}"))?;
    let system = compose_system(&canonical.display().to_string(), mode, &snapshot, true);
    let candidates: Vec<_> = agent_core::local_agent::tools_for(mode)
        .into_iter()
        .map(|tool| SchemaCandidate {
            tool: agent_core::local_mcp::ResolvedTool::builtin(tool, tool.schema()),
            priority: 0,
        })
        .collect();
    let plan = plan_request("preview", &system, &candidates, &[], &context.budget)
        .map_err(|why| format!("{why}\n{}", snapshot.sources_summary()))?;
    let mut text = format!("{} instruction files, in application order. {}\nEstimates use one token per two serialized UTF-8 bytes plus eight per message. This preview excludes conversation history; runtime also reserves the current exchange and tool results.\nScope: the selected folder and its ancestors only.\n", snapshot.documents.len(), plan.summary());
    for document in &snapshot.documents {
        text.push_str(&format!(
            "\n{} — {} ({} bytes)",
            document.source.scope.label(),
            document.source.path,
            document.source.bytes
        ));
    }
    Ok(agent_core::redact::redact(&text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn preview_uses_selected_paths_and_reports_only_metadata_even_on_budget_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("profile-config");
        std::fs::create_dir_all(config.join("agent-terminal")).unwrap();
        let native = config.join("agent-terminal/AGENTS.md");
        std::fs::write(&native, "private document convention").unwrap();
        let paths = InstructionPaths::new(Some(tmp.path().into()), Some(config));
        let mut context = agent_core::local_context::LocalLaunchContext::default();
        context.instructions.discover_ancestors = false;
        let text = preview(&context, tmp.path(), &paths, agent_core::adapter::Mode::Ask).unwrap();
        assert!(text.contains(&native.display().to_string()));
        assert!(text.contains("app global"));
        assert!(text.contains("estimated input"));
        assert!(!text.contains("private document convention"));
        context.budget.static_context_bytes = 1024;
        let error =
            preview(&context, tmp.path(), &paths, agent_core::adapter::Mode::Ask).unwrap_err();
        assert!(error.contains("Increase budget"));
        assert!(error.contains(&native.display().to_string()));
        assert!(!error.contains("private document convention"));
    }

    #[test]
    fn deduplication_still_selects_the_override_and_cwd_is_canonical() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("real/child");
        std::fs::create_dir_all(&folder).unwrap();
        let global = tmp.path().join("global");
        std::fs::write(&global, "global").unwrap();
        symlink(&global, folder.join("AGENTS.override.md")).unwrap();
        std::fs::write(folder.join("AGENTS.md"), "must not fall through").unwrap();
        std::fs::write(tmp.path().join("real/AGENTS.md"), "real ancestor").unwrap();
        symlink(&folder, tmp.path().join("alias")).unwrap();
        let settings = InstructionSettings {
            global_files: vec![global.display().to_string()],
            ..Default::default()
        };
        let snapshot = discover(
            &settings,
            &tmp.path().join("alias"),
            &InstructionPaths::default(),
        )
        .unwrap();
        assert_eq!(snapshot.documents.len(), 2);
        assert!(snapshot.documents[1]
            .source
            .path
            .ends_with("real/AGENTS.md"));
    }

    #[test]
    fn native_global_then_explicit_order_then_specific_project_with_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let folder = root.join("project/child");
        let config = root.join("config");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(config.join("agent-terminal")).unwrap();
        let native = config.join("agent-terminal/AGENTS.md");
        std::fs::write(&native, "native").unwrap();
        let global = root.join("global.md");
        std::fs::write(&global, "global").unwrap();
        symlink(&native, root.join("projection.md")).unwrap();
        std::fs::write(root.join("project/AGENTS.md"), "ordinary").unwrap();
        std::fs::write(root.join("project/AGENTS.override.md"), "override").unwrap();
        std::fs::write(folder.join("CLAUDE.md"), "fallback").unwrap();
        let settings = InstructionSettings {
            global_files: vec![
                global.display().to_string(),
                root.join("projection.md").display().to_string(),
            ],
            claude_fallback: true,
            ..Default::default()
        };
        let snapshot = discover(
            &settings,
            &folder,
            &InstructionPaths::new(Some(root.into()), Some(config)),
        )
        .unwrap();
        let paths: Vec<_> = snapshot
            .documents
            .iter()
            .map(|d| d.source.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                native.display().to_string(),
                global.display().to_string(),
                root.join("project/AGENTS.override.md")
                    .display()
                    .to_string(),
                folder.join("CLAUDE.md").display().to_string()
            ]
        );
        assert_eq!(
            snapshot.documents[0].source.scope,
            InstructionScope::NativeGlobal
        );
        assert_eq!(snapshot.documents[1].source.scope, InstructionScope::Global);
        assert_eq!(
            snapshot.documents[2].source.scope,
            InstructionScope::Project
        );
        let mut disabled = settings;
        disabled.claude_fallback = false;
        assert_eq!(
            discover(&disabled, &folder, &InstructionPaths::default())
                .unwrap()
                .documents
                .len(),
            3
        );
    }

    #[test]
    fn explicit_missing_invalid_utf8_nonregular_and_caps_block() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rules");
        let settings = InstructionSettings {
            global_files: vec![path.display().to_string()],
            discover_ancestors: false,
            max_file_bytes: 8,
            max_total_bytes: 10,
            ..Default::default()
        };
        assert!(
            discover(&settings, tmp.path(), &InstructionPaths::default())
                .unwrap_err()
                .contains("rules")
        );
        std::fs::write(&path, [0xff]).unwrap();
        assert!(
            discover(&settings, tmp.path(), &InstructionPaths::default())
                .unwrap_err()
                .contains("UTF-8")
        );
        std::fs::write(&path, "éééé").unwrap();
        assert!(discover(&settings, tmp.path(), &InstructionPaths::default()).is_ok());
        std::fs::write(&path, "ééééé").unwrap();
        assert!(
            discover(&settings, tmp.path(), &InstructionPaths::default())
                .unwrap_err()
                .contains("cap")
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            discover(&settings, tmp.path(), &InstructionPaths::default())
                .unwrap_err()
                .contains("regular")
        );
        std::fs::remove_dir(&path).unwrap();
        let fifo = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the CString is live and names a disposable FIFO in this test's directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            discover(&settings, tmp.path(), &InstructionPaths::default())
                .unwrap_err()
                .contains("regular")
        );
        let device = InstructionSettings {
            global_files: vec!["/dev/null".into()],
            ..settings
        };
        assert!(discover(&device, tmp.path(), &InstructionPaths::default())
            .unwrap_err()
            .contains("regular"));
    }

    #[test]
    fn aggregate_growth_and_xdg_resolution_are_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a"), "123456").unwrap();
        std::fs::write(tmp.path().join("b"), "123456").unwrap();
        let settings = InstructionSettings {
            global_files: vec!["a".into(), "b".into()],
            discover_ancestors: false,
            max_file_bytes: 8,
            max_total_bytes: 10,
            ..Default::default()
        };
        assert!(
            discover(&settings, tmp.path(), &InstructionPaths::default())
                .unwrap_err()
                .contains("aggregate")
        );
        assert!(
            read_bounded(std::io::Cursor::new(b"123456789"), Path::new("growing"), 8)
                .unwrap_err()
                .contains("grew")
        );
        let paths = InstructionPaths::new(Some(tmp.path().into()), Some(PathBuf::from("relative")));
        assert_eq!(paths.config_home, Some(tmp.path().join(".config")));
        assert_eq!(
            paths.expand("$XDG_CONFIG_HOME/a", tmp.path()).unwrap(),
            tmp.path().join(".config/a")
        );
    }
}
