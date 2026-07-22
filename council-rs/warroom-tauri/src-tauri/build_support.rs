use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn tracked_file_rerun_paths(dir: &Path) -> Vec<PathBuf> {
    let Some(root) = git_output(dir, &["rev-parse", "--show-toplevel"]) else {
        return Vec::new();
    };
    let root = PathBuf::from(root);
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-z"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|relative| !relative.is_empty())
        .map(|relative| root.join(String::from_utf8_lossy(relative).as_ref()))
        .collect()
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempRepo(PathBuf);

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn tracked_source_edit_is_watched_without_git_metadata_change() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = TempRepo(std::env::temp_dir().join(format!(
            "irin-tauri-build-rerun-{}-{nonce}",
            std::process::id()
        )));
        fs::create_dir_all(&repo.0).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo.0)
            .status()
            .unwrap()
            .success());

        let source = repo.0.join("tracked-source.rs");
        fs::write(&source, "const VALUE: u8 = 1;\n").unwrap();
        assert!(Command::new("git")
            .args(["add", "tracked-source.rs"])
            .current_dir(&repo.0)
            .status()
            .unwrap()
            .success());

        // Editing the worktree file does not update HEAD or the index. Cargo
        // must still watch the file itself so build-time dirty identity reruns.
        let head_before = fs::read(repo.0.join(".git/HEAD")).unwrap();
        let index_before = fs::read(repo.0.join(".git/index")).unwrap();
        fs::write(&source, "const VALUE: u8 = 2;\n").unwrap();
        assert_eq!(fs::read(repo.0.join(".git/HEAD")).unwrap(), head_before);
        assert_eq!(fs::read(repo.0.join(".git/index")).unwrap(), index_before);
        let watched = tracked_file_rerun_paths(&repo.0);
        assert!(watched.contains(&source.canonicalize().unwrap()));
    }
}
