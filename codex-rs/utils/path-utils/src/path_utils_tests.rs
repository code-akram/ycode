#[cfg(unix)]
mod symlinks {
    use super::super::resolve_symlink_write_paths;
    use pretty_assertions::assert_eq;
    use std::os::unix::fs::symlink;

    #[test]
    fn symlink_cycles_fall_back_to_root_write_path() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a");
        let b = dir.path().join("b");

        symlink(&b, &a)?;
        symlink(&a, &b)?;

        let resolved = resolve_symlink_write_paths(&a)?;

        assert_eq!(resolved.read_path, None);
        assert_eq!(resolved.write_path, a);
        Ok(())
    }
}

mod path_comparison {
    use super::super::paths_match_after_normalization;
    use std::path::PathBuf;

    #[test]
    fn matches_identical_existing_paths() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;

        assert!(paths_match_after_normalization(dir.path(), dir.path()));
        Ok(())
    }

    #[test]
    fn falls_back_to_raw_equality_when_paths_cannot_be_normalized() {
        assert!(paths_match_after_normalization(
            PathBuf::from("missing"),
            PathBuf::from("missing"),
        ));
        assert!(!paths_match_after_normalization(
            PathBuf::from("missing-a"),
            PathBuf::from("missing-b"),
        ));
    }

    #[cfg(windows)]
    #[test]
    fn matches_windows_verbatim_paths() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let verbatim_dir = PathBuf::from(format!(r"\\?\{}", dir.path().display()));

        assert!(paths_match_after_normalization(verbatim_dir, dir.path()));
        Ok(())
    }
}
