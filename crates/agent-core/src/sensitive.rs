//! Names of files and folders that run code outside the agent, or steer the next agent run,
//! wherever they appear below a folder: an edit of one always asks, and so does a read, since
//! they are as likely to hold credentials as to be run. Lexical only: the callers resolve paths.

/// Directory or file names that run code outside the agent when found anywhere below the
/// workspace: git hooks and config (`.git`, also a worktree's `.git` file), direnv, editor task
/// and launch configs, dev containers, hook managers, Cargo's `runner`/`build` config.
pub const SENSITIVE_NAMES: &[&str] = &[
    ".git",
    ".envrc",
    ".direnv",
    ".vscode",
    ".idea",
    ".devcontainer",
    ".husky",
    ".pre-commit-config.yaml",
    ".cargo",
    ".gitlab-ci.yml",
    ".circleci",
    // Project-level agent config: hooks, permissions and MCP servers the next agent run obeys.
    ".claude",
    ".gemini",
    ".agents",
    ".mcp.json",
    ".codex",
    // Other version control, editor and tool configs that run code.
    ".hg",
    ".zed",
    ".exrc",
    ".nvim.lua",
    "mise.toml",
    // Agent instruction files: the next agent run follows what they say. Names are compared
    // lowercased.
    "agents.md",
    "claude.md",
    "gemini.md",
];

/// Whether a path, given as its lowercased component names, is sensitive: any component in
/// [`SENSITIVE_NAMES`], or `.github/workflows` anywhere in it.
pub fn names_sensitive<S: AsRef<str>>(names: &[S]) -> bool {
    names.iter().any(|p| SENSITIVE_NAMES.contains(&p.as_ref()))
        || names
            .windows(2)
            .any(|w| w[0].as_ref() == ".github" && w[1].as_ref() == "workflows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listed_name_or_a_workflow_anywhere_is_sensitive() {
        assert!(names_sensitive(&["src", ".git", "config"]));
        assert!(names_sensitive(&["agents.md"]));
        assert!(names_sensitive(&["a", ".github", "workflows", "ci.yml"]));
        assert!(!names_sensitive(&[".github", "issue_template.md"]));
        assert!(!names_sensitive(&["src", "main.rs"]));
        assert!(!names_sensitive::<&str>(&[]));
    }
}
