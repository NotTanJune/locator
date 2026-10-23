//! Rules that keep low-value files out of the embedding pass, and the order in which the pass
//! handles files. Pure functions over paths, so the scanner and the filename index are unaffected.

use std::path::Path;

use crate::db::EmbeddableFile;

/// Detail recorded for files skipped by a rule, so later runs do not retry them.
pub const LOW_VALUE_DETAIL: &str = "low-value file";

const DEPENDENCY_OR_BUILD_DIRS: [&str; 15] = [
    "node_modules",
    "target",
    "build",
    "dist",
    "out",
    ".next",
    ".nuxt",
    "__pycache__",
    ".gradle",
    "Pods",
    ".cache",
    ".idea",
    "site-packages",
    ".terraform",
    "coverage",
];
const VIRTUALENV_DIRS: [&str; 3] = [".venv", "venv", "env"];
const VENDOR_MANIFESTS: [&str; 3] = ["Cargo.toml", "go.mod", "composer.json"];
const LOCK_FILE_NAMES: [&str; 8] = [
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Cargo.lock",
    "poetry.lock",
    "uv.lock",
    "Gemfile.lock",
    "composer.lock",
];
const DOCUMENT_EXTENSIONS: [&str; 9] = [
    "md", "txt", "rst", "org", "tex", "pdf", "rtf", "adoc", "markdown",
];
const IMAGE_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "gif", "bmp", "webp", "tiff"];
const CODE_EXTENSIONS: [&str; 22] = [
    "rs", "py", "js", "ts", "tsx", "jsx", "mjs", "cjs", "go", "c", "h", "cc", "cpp", "hpp", "java",
    "kt", "swift", "rb", "php", "cs", "sh", "sql",
];

/// Rule label for keys, certificates, keystores and credential files. Detail recorded for them.
pub const SECRET_RULE: &str = "secret or credential file";
/// Rule label for any path under a hidden (dot-prefixed) folder, or a dot-prefixed file name.
pub const HIDDEN_RULE: &str = "hidden file or folder";

/// Extensions of key, certificate and keystore files. Taken from `isSecret` in DigUp's
/// `Sources/DigUpKit/Rules.swift` (MIT; see tests/fixtures/digup-reference/NOTICE.md).
const SECRET_EXTENSIONS: [&str; 15] = [
    "pem",
    "key",
    "p12",
    "pfx",
    "keystore",
    "jks",
    "ppk",
    "kdbx",
    "gpg",
    "asc",
    "crt",
    "cer",
    "der",
    "p8",
    "mobileprovision",
];
/// Lowercase name prefixes of key and credential files. Also from DigUp's `isSecret`. A prefix
/// match also covers longer names such as `.env.local` or `id_rsa.pub`.
const SECRET_NAME_PREFIXES: [&str; 12] = [
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    ".env",
    "secret",
    "credential",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "htpasswd",
    ".htpasswd",
];

/// Rule for files that must never be read: secret names, and anything under a hidden path. Checked
/// before `low_value_rule`. `relative` is the path under the scan root.
pub fn secret_rule(relative: &Path) -> Option<&'static str> {
    let parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .filter(|part| part != ".")
        .collect();
    let file_name = parts.last()?.to_ascii_lowercase();
    let extension = Path::new(&file_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    if SECRET_EXTENSIONS.contains(&extension)
        || SECRET_NAME_PREFIXES
            .iter()
            .any(|prefix| file_name.starts_with(prefix))
    {
        return Some(SECRET_RULE);
    }
    if parts.iter().any(|part| part.starts_with('.')) {
        return Some(HIDDEN_RULE);
    }
    None
}

/// The rule that keeps a file out of the embedding pass before any bytes are read: `secret_rule`
/// first, then `low_value_rule`. Used by the pipeline and by `embed bench --count-only`.
pub fn never_read_rule(
    relative: &Path,
    absolute: &Path,
    home: Option<&Path>,
    has_manifest: &dyn Fn(&Path) -> bool,
) -> Option<&'static str> {
    secret_rule(relative).or_else(|| low_value_rule(relative, absolute, home, has_manifest))
}

/// Detail recorded for a file skipped by a rule. Secret and hidden rules keep their own detail;
/// every other rule uses `LOW_VALUE_DETAIL`.
pub fn detail_for_rule(rule: &str) -> &'static str {
    match rule {
        SECRET_RULE => SECRET_RULE,
        HIDDEN_RULE => HIDDEN_RULE,
        _ => LOW_VALUE_DETAIL,
    }
}

/// The rule that makes a file low value, by name for the `LCTR_TIMING` counts. `None` keeps it.
///
/// `relative` is the path under the scan root. `absolute` is the same file on disk. `home` is the
/// user's home directory when known. `has_manifest(dir)` reports whether `dir` holds a package
/// manifest, which decides whether a `vendor` directory is third-party code.
pub fn low_value_rule(
    relative: &Path,
    absolute: &Path,
    home: Option<&Path>,
    has_manifest: &dyn Fn(&Path) -> bool,
) -> Option<&'static str> {
    if let Some(home) = home {
        if let Ok(under_home) = absolute.strip_prefix(home) {
            if under_home
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                == Some("Library")
            {
                return Some("home Library folder");
            }
        }
    }

    let components: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let (file_name, directories) = components.split_last()?;
    for (index, directory) in directories.iter().enumerate() {
        let name = directory.as_str();
        // Xcode names per-target build folders DerivedData-ios, DerivedData-mac, and so on.
        if DEPENDENCY_OR_BUILD_DIRS.contains(&name) || name.starts_with("DerivedData") {
            return Some("dependency or build directory");
        }
        // Inside a Photos library, database/ holds its own search index and derivatives/ holds
        // resized copies of the photos.
        if name.to_ascii_lowercase().ends_with(".photoslibrary") {
            let inner = &directories[index + 1..];
            if inner.iter().any(|part| part == "database") {
                return Some("photos library database");
            }
            if inner.iter().any(|part| part == "derivatives") {
                return Some("photos library thumbnails");
            }
        }
        if VIRTUALENV_DIRS.contains(&name) {
            return Some("virtualenv directory");
        }
        if name == "vendor" {
            let parent = directories[..index]
                .iter()
                .fold(relative_root(absolute, relative), |path, part| {
                    path.join(part)
                });
            if has_manifest(&parent) {
                return Some("vendor directory");
            }
        }
    }

    let lower = file_name.to_ascii_lowercase();
    if LOCK_FILE_NAMES.contains(&file_name.as_str()) || lower.ends_with(".lock") {
        return Some("lock file");
    }
    if lower.ends_with(".min.js") || lower.ends_with(".min.css") || lower.ends_with(".map") {
        return Some("minified or source map");
    }
    if lower.ends_with(".svg") {
        return Some("svg image");
    }
    if lower.ends_with(".log") {
        return Some("log file");
    }
    None
}

/// Directory that contains the scan root, given the file's absolute and relative paths.
fn relative_root(absolute: &Path, relative: &Path) -> std::path::PathBuf {
    let mut root = absolute.to_path_buf();
    for _ in relative.components() {
        root.pop();
    }
    root
}

/// Whether `dir` contains a manifest that marks a `vendor` directory as third-party code.
pub fn dir_has_vendor_manifest(dir: &Path) -> bool {
    VENDOR_MANIFESTS.iter().any(|name| dir.join(name).is_file())
}

/// Order of the embedding pass: documents, then images, then code, then everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Document,
    Image,
    Code,
    Other,
}

pub fn priority_of(extension: Option<&str>, kind: &str) -> Priority {
    let lower = extension.map(str::to_ascii_lowercase).unwrap_or_default();
    if DOCUMENT_EXTENSIONS.contains(&lower.as_str()) {
        Priority::Document
    } else if kind == "image" || IMAGE_EXTENSIONS.contains(&lower.as_str()) {
        Priority::Image
    } else if CODE_EXTENSIONS.contains(&lower.as_str()) {
        Priority::Code
    } else {
        Priority::Other
    }
}

/// Sorts files by priority, then newest modification first, then path.
pub fn sort_for_embedding(files: &mut [EmbeddableFile]) {
    files.sort_by(|a, b| {
        priority_of(a.extension.as_deref(), &a.kind)
            .cmp(&priority_of(b.extension.as_deref(), &b.kind))
            .then_with(|| b.modified_at.cmp(&a.modified_at))
            .then_with(|| a.path.cmp(&b.path))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_manifest(_: &Path) -> bool {
        false
    }

    fn rule(relative: &str, absolute: &str, home: Option<&str>) -> Option<&'static str> {
        low_value_rule(
            Path::new(relative),
            Path::new(absolute),
            home.map(Path::new),
            &no_manifest,
        )
    }

    #[test]
    fn dependency_and_build_directories_are_skipped_at_any_depth() {
        assert_eq!(
            rule("target/debug/app.d", "/r/target/debug/app.d", None),
            Some("dependency or build directory")
        );
        assert_eq!(
            rule(
                "web/node_modules/x/readme.md",
                "/r/web/node_modules/x/readme.md",
                None
            ),
            Some("dependency or build directory")
        );
        assert_eq!(
            rule("src/build/notes.md", "/r/src/build/notes.md", None),
            Some("dependency or build directory")
        );
        assert_eq!(
            rule("app/.next/cache/a.txt", "/r/app/.next/cache/a.txt", None),
            Some("dependency or build directory")
        );
        assert_eq!(
            rule("py/__pycache__/m.pyc", "/r/py/__pycache__/m.pyc", None),
            Some("dependency or build directory")
        );
    }

    #[test]
    fn plain_names_that_only_look_similar_are_kept() {
        assert_eq!(rule("notes/targets.md", "/r/notes/targets.md", None), None);
        assert_eq!(rule("src/builder.rs", "/r/src/builder.rs", None), None);
        assert_eq!(rule("docs/outline.md", "/r/docs/outline.md", None), None);
    }

    #[test]
    fn virtualenv_directories_are_skipped() {
        assert_eq!(
            rule(".venv/lib/x.py", "/r/.venv/lib/x.py", None),
            Some("virtualenv directory")
        );
        assert_eq!(
            rule("proj/env/bin/a.txt", "/r/proj/env/bin/a.txt", None),
            Some("virtualenv directory")
        );
    }

    #[test]
    fn vendor_is_skipped_only_with_a_sibling_manifest() {
        let with_manifest = |dir: &Path| dir == Path::new("/r/go-app");
        let rule_with = |relative: &str, absolute: &str| {
            low_value_rule(
                Path::new(relative),
                Path::new(absolute),
                None,
                &with_manifest,
            )
        };
        assert_eq!(
            rule_with("go-app/vendor/lib/a.go", "/r/go-app/vendor/lib/a.go"),
            Some("vendor directory")
        );
        assert_eq!(
            rule_with("notes/vendor/memo.md", "/r/notes/vendor/memo.md"),
            None
        );
        assert_eq!(
            rule("notes/vendor/memo.md", "/r/notes/vendor/memo.md", None),
            None
        );
    }

    #[test]
    fn lock_and_generated_file_names_are_skipped() {
        for name in [
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "Cargo.lock",
            "poetry.lock",
            "uv.lock",
            "Gemfile.lock",
            "composer.lock",
            "anything.lock",
        ] {
            assert_eq!(
                rule(name, &format!("/r/{name}"), None),
                Some("lock file"),
                "{name}"
            );
        }
        assert_eq!(
            rule("web/app.min.js", "/r/web/app.min.js", None),
            Some("minified or source map")
        );
        assert_eq!(
            rule("web/app.min.css", "/r/web/app.min.css", None),
            Some("minified or source map")
        );
        assert_eq!(
            rule("web/app.js.map", "/r/web/app.js.map", None),
            Some("minified or source map")
        );
        assert_eq!(
            rule("img/logo.svg", "/r/img/logo.svg", None),
            Some("svg image")
        );
        assert_eq!(
            rule("run/server.log", "/r/run/server.log", None),
            Some("log file")
        );
    }

    #[test]
    fn derived_data_folders_with_a_suffix_are_build_output() {
        for dir in ["DerivedData", "DerivedData-mac", "DerivedData-ios"] {
            let relative = format!("app/{dir}/Build/Intermediates.noindex/main.d");
            assert_eq!(
                rule(&relative, &format!("/r/{relative}"), None),
                Some("dependency or build directory")
            );
        }
        assert_eq!(
            rule("notes/DerivedData.md", "/r/notes/DerivedData.md", None),
            None
        );
    }

    #[test]
    fn photos_library_internals_are_skipped_but_originals_are_kept() {
        let lib = "Pictures/Photos Library.photoslibrary";
        let check = |inner: &str| {
            let relative = format!("{lib}/{inner}");
            rule(
                &relative,
                &format!("/Users/me/{relative}"),
                Some("/Users/me"),
            )
        };
        assert_eq!(
            check("database/search/Spotlight/NSFileProtection/a.txt"),
            Some("photos library database")
        );
        assert_eq!(
            check("resources/derivatives/masters/2/2F9D_4_5005_c.jpeg"),
            Some("photos library thumbnails")
        );
        assert_eq!(
            check("scopes/syndication/resources/derivatives/B/x.jpeg"),
            Some("photos library thumbnails")
        );
        assert_eq!(check("originals/A/A411.jpeg"), None);
        assert_eq!(
            rule(
                "photos/database/notes.md",
                "/r/photos/database/notes.md",
                None
            ),
            None
        );
    }

    #[test]
    fn library_is_skipped_only_directly_under_home() {
        assert_eq!(
            rule(
                "Library/Caches/x.txt",
                "/Users/me/Library/Caches/x.txt",
                Some("/Users/me")
            ),
            Some("home Library folder")
        );
        assert_eq!(
            rule(
                "Projects/Library/notes.md",
                "/Users/me/Projects/Library/notes.md",
                Some("/Users/me")
            ),
            None
        );
        assert_eq!(rule("Library/x.md", "/Users/me/Library/x.md", None), None);
    }

    #[test]
    fn ordinary_documents_and_code_are_kept() {
        assert_eq!(
            rule("plans/024-plan.md", "/r/plans/024-plan.md", None),
            None
        );
        assert_eq!(rule("src/lib.rs", "/r/src/lib.rs", None), None);
        assert_eq!(rule("photo.png", "/r/photo.png", None), None);
    }

    #[test]
    fn priority_order_is_documents_images_code_then_other() {
        assert!(priority_of(Some("md"), "text") < priority_of(Some("png"), "image"));
        assert!(priority_of(Some("png"), "image") < priority_of(Some("rs"), "text"));
        assert!(priority_of(Some("rs"), "text") < priority_of(Some("bin"), "file"));
        assert_eq!(priority_of(Some("PDF"), "pdf"), Priority::Document);
        assert_eq!(priority_of(None, "file"), Priority::Other);
    }

    fn file(path: &str, extension: &str, kind: &str, modified_at: i64) -> EmbeddableFile {
        EmbeddableFile {
            path: path.to_string(),
            kind: kind.to_string(),
            extension: Some(extension.to_string()),
            size_bytes: 10,
            modified_at,
        }
    }

    #[test]
    fn sort_puts_documents_first_and_newest_first_within_a_group() {
        let mut files = vec![
            file("/r/main.rs", "rs", "text", 900),
            file("/r/old.md", "md", "text", 100),
            file("/r/new.md", "md", "text", 500),
            file("/r/pic.png", "png", "image", 800),
            file("/r/data.bin", "bin", "file", 999),
        ];
        sort_for_embedding(&mut files);
        let order: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "/r/new.md",
                "/r/old.md",
                "/r/pic.png",
                "/r/main.rs",
                "/r/data.bin"
            ]
        );
    }
}

#[cfg(test)]
mod secret_tests {
    use super::*;

    fn secret(path: &str) -> Option<&'static str> {
        secret_rule(Path::new(path))
    }

    #[test]
    fn key_and_keystore_extensions_are_secret_in_any_case() {
        for name in [
            "server.pem",
            "tls.key",
            "cert.p12",
            "cert.PFX",
            "app.keystore",
            "store.jks",
            "putty.ppk",
            "vault.kdbx",
            "backup.gpg",
            "sig.asc",
            "ca.crt",
            "ca.cer",
            "ca.der",
            "api.p8",
            "profile.mobileprovision",
        ] {
            assert_eq!(secret(name), Some(SECRET_RULE), "{name}");
        }
    }

    #[test]
    fn key_and_credential_names_are_secret() {
        for name in [
            "id_rsa",
            "id_rsa.pub",
            "id_dsa",
            "id_ecdsa",
            "id_ed25519",
            ".env",
            ".env.local",
            "secrets.yaml",
            "Credentials.json",
            "credential_store",
            ".netrc",
            ".npmrc",
            ".pypirc",
            ".htpasswd",
            "htpasswd",
            "Secret notes.txt",
        ] {
            assert_eq!(secret(name), Some(SECRET_RULE), "{name}");
        }
    }

    #[test]
    fn hidden_folder_below_the_root_is_never_read() {
        assert_eq!(secret(".ssh/config"), Some(HIDDEN_RULE));
        assert_eq!(secret("work/.gnupg/pubring.txt"), Some(HIDDEN_RULE));
        assert_eq!(secret(".aws/notes.md"), Some(HIDDEN_RULE));
        assert_eq!(secret("./.config/tool.toml"), Some(HIDDEN_RULE));
    }

    #[test]
    fn hidden_file_name_is_never_read() {
        assert_eq!(secret(".bashrc"), Some(HIDDEN_RULE));
        assert_eq!(secret("notes/.todo.md"), Some(HIDDEN_RULE));
    }

    #[test]
    fn ordinary_files_are_not_secret() {
        for name in [
            "notes.md",
            "docs/guide.txt",
            "report.pdf",
            "keyboard.txt",
            "monkey.md",
        ] {
            assert_eq!(secret(name), None, "{name}");
        }
    }

    #[test]
    fn secret_rule_wins_over_hidden_for_the_label() {
        assert_eq!(secret(".ssh/id_rsa"), Some(SECRET_RULE));
    }

    #[test]
    fn detail_for_rule_keeps_secret_and_hidden_details() {
        assert_eq!(detail_for_rule(SECRET_RULE), "secret or credential file");
        assert_eq!(detail_for_rule(HIDDEN_RULE), "hidden file or folder");
        assert_eq!(detail_for_rule("lock file"), LOW_VALUE_DETAIL);
    }

    #[test]
    fn never_read_rule_checks_secrets_before_low_value() {
        let no_manifest = |_: &Path| false;
        let rule = never_read_rule(
            Path::new(".env"),
            Path::new("/root/.env"),
            None,
            &no_manifest,
        );
        assert_eq!(rule, Some(SECRET_RULE));
        let rule = never_read_rule(
            Path::new("Cargo.lock"),
            Path::new("/root/Cargo.lock"),
            None,
            &no_manifest,
        );
        assert_eq!(rule, Some("lock file"));
    }
}
