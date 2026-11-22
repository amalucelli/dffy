//! What the binary's end-to-end tests share.

use std::sync::LazyLock;

/// The tests' PATH without the directories that hold a review plugin. With
/// `review` unset, dffy uses the only one on PATH, so the user's own would
/// run, and ask its code host about the tests' repositories.
pub(crate) fn path_without_review_plugins() -> &'static str {
    static PATH: LazyLock<String> = LazyLock::new(|| {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let kept = std::env::split_paths(&path).filter(|dir| {
            !std::fs::read_dir(dir).is_ok_and(|mut entries| {
                entries.any(|entry| {
                    entry.is_ok_and(|e| {
                        e.file_name()
                            .as_encoded_bytes()
                            .starts_with(b"dffy-review-")
                    })
                })
            })
        });
        std::env::join_paths(kept)
            .ok()
            .and_then(|joined| joined.into_string().ok())
            .unwrap_or_default()
    });
    &PATH
}
