use std::path::Path;

use anyhow::{bail, Context, Result};

pub fn open_file(path: &Path) -> Result<()> {
    std::process::Command::new("open")
        .arg(path)
        .status()
        .with_context(|| format!("open {}", path.display()))?;
    Ok(())
}

/// Owns one Finder window created by a single TUI search session.
pub struct FinderRevealSession {
    #[cfg(any(target_os = "macos", test))]
    automation: Box<dyn FinderAutomation>,
    #[cfg(any(target_os = "macos", test))]
    window_id: Option<u64>,
}

impl FinderRevealSession {
    pub fn new() -> Self {
        Self {
            #[cfg(any(target_os = "macos", test))]
            automation: Box::new(OsascriptAutomation),
            #[cfg(any(target_os = "macos", test))]
            window_id: None,
        }
    }

    pub fn reveal(&mut self, path: &Path) -> Result<()> {
        #[cfg(any(target_os = "macos", test))]
        {
            self.reveal_with_finder(path)
        }

        #[cfg(all(not(target_os = "macos"), not(test)))]
        {
            reveal_once(path)
        }
    }

    /// Closes only this session's dedicated Finder window.
    pub fn close(&mut self) -> Result<()> {
        #[cfg(any(target_os = "macos", test))]
        {
            let Some(window_id) = self.window_id.take() else {
                return Ok(());
            };
            let output = self
                .automation
                .run(CLOSE_WINDOW_SCRIPT, &[window_id.to_string().into()])
                .with_context(|| format!("close Finder window {window_id}"))?;
            ensure_success(&output, &format!("close Finder window {window_id}"))?;
            let response = stdout_text(&output, &format!("close Finder window {window_id}"))?;
            if response == "CLOSED" || response == "MISSING" {
                return Ok(());
            }
            bail!("close Finder window {window_id}: unexpected osascript output {response:?}");
        }

        #[cfg(all(not(target_os = "macos"), not(test)))]
        {
            Ok(())
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn reveal_with_finder(&mut self, path: &Path) -> Result<()> {
        if let Some(window_id) = self.window_id {
            let output = self
                .automation
                .run(
                    RETARGET_WINDOW_SCRIPT,
                    &[
                        path.as_os_str().to_os_string(),
                        window_id.to_string().into(),
                    ],
                )
                .with_context(|| {
                    format!("reveal {} in Finder window {window_id}", path.display())
                })?;
            ensure_success(
                &output,
                &format!("reveal {} in Finder window {window_id}", path.display()),
            )?;
            let response = stdout_text(
                &output,
                &format!("reveal {} in Finder window {window_id}", path.display()),
            )?;
            if response == "MISSING" {
                self.window_id = None;
            } else if parse_window_id(&response, path)? != window_id {
                bail!(
                    "reveal {} in Finder window {window_id}: Finder returned unexpected window ID {response:?}",
                    path.display()
                );
            } else {
                return Ok(());
            }
        }

        let output = self
            .automation
            .run(CREATE_WINDOW_SCRIPT, &[path.as_os_str().to_os_string()])
            .with_context(|| format!("create Finder window for {}", path.display()))?;
        ensure_success(
            &output,
            &format!("create Finder window for {}", path.display()),
        )?;
        let response = stdout_text(
            &output,
            &format!("create Finder window for {}", path.display()),
        )?;
        self.window_id = Some(parse_window_id(&response, path)?);
        Ok(())
    }
}

impl Default for FinderRevealSession {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FinderRevealSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(any(target_os = "macos", test))]
const CREATE_WINDOW_SCRIPT: &str = r#"
on run argv
    set filePath to item 1 of argv
    tell application "Finder"
        set itemRef to POSIX file filePath as alias
        set parentRef to container of itemRef
        open parentRef
        set finderWindow to front window
        set target of finderWindow to parentRef
        set index of finderWindow to 1
        activate
        select itemRef
        return id of finderWindow
    end tell
end run
"#;

#[cfg(any(target_os = "macos", test))]
const RETARGET_WINDOW_SCRIPT: &str = r#"
on run argv
    set filePath to item 1 of argv
    set windowID to (item 2 of argv) as integer
    tell application "Finder"
        try
            set finderWindow to window id windowID
        on error
            return "MISSING"
        end try
        set itemRef to POSIX file filePath as alias
        set parentRef to container of itemRef
        set target of finderWindow to parentRef
        set index of finderWindow to 1
        activate
        select itemRef
        return id of finderWindow
    end tell
end run
"#;

#[cfg(any(target_os = "macos", test))]
const CLOSE_WINDOW_SCRIPT: &str = r#"
on run argv
    set windowID to (item 1 of argv) as integer
    tell application "Finder"
        try
            close window id windowID
        on error
            return "MISSING"
        end try
        return "CLOSED"
    end tell
end run
"#;

#[cfg(any(target_os = "macos", test))]
struct AutomationOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[cfg(any(target_os = "macos", test))]
trait FinderAutomation {
    fn run(&mut self, source: &str, args: &[std::ffi::OsString]) -> Result<AutomationOutput>;
}

#[cfg(any(target_os = "macos", test))]
struct OsascriptAutomation;

#[cfg(any(target_os = "macos", test))]
impl FinderAutomation for OsascriptAutomation {
    fn run(&mut self, source: &str, args: &[std::ffi::OsString]) -> Result<AutomationOutput> {
        let output = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(source)
            .args(args)
            .output()
            .context("launch /usr/bin/osascript")?;
        Ok(AutomationOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[cfg(any(target_os = "macos", test))]
fn ensure_success(output: &AutomationOutput, action: &str) -> Result<()> {
    if output.success {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!("{action}: osascript failed: {stderr}");
}

#[cfg(any(target_os = "macos", test))]
fn stdout_text(output: &AutomationOutput, action: &str) -> Result<String> {
    String::from_utf8(output.stdout.clone())
        .with_context(|| format!("{action}: osascript returned non-UTF-8 output"))
        .map(|text| text.trim().to_string())
}

#[cfg(any(target_os = "macos", test))]
fn parse_window_id(response: &str, path: &Path) -> Result<u64> {
    response.parse::<u64>().with_context(|| {
        format!(
            "create Finder window for {}: invalid window ID {response:?}",
            path.display()
        )
    })
}

#[cfg(all(not(target_os = "macos"), not(test)))]
fn reveal_once(path: &Path) -> Result<()> {
    let status = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .status()
        .with_context(|| format!("reveal {}", path.display()))?;
    if status.success() {
        Ok(())
    } else {
        bail!("reveal {}: open exited with {status}", path.display());
    }
}

pub fn copy_path(path: &Path) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("open clipboard")?;
    clipboard
        .set_text(path.to_string_lossy().to_string())
        .context("copy path")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use anyhow::{bail, Result};

    use super::{
        AutomationOutput, FinderAutomation, FinderRevealSession, CLOSE_WINDOW_SCRIPT,
        CREATE_WINDOW_SCRIPT, RETARGET_WINDOW_SCRIPT,
    };

    type Calls = Arc<Mutex<Vec<(String, Vec<OsString>)>>>;

    struct FakeAutomation {
        responses: VecDeque<Result<AutomationOutput>>,
        calls: Calls,
    }

    impl FakeAutomation {
        fn responding(
            responses: impl IntoIterator<Item = Result<AutomationOutput>>,
        ) -> (Self, Calls) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    responses: responses.into_iter().collect(),
                    calls: Arc::clone(&calls),
                },
                calls,
            )
        }

        fn success(stdout: &str) -> Result<AutomationOutput> {
            Ok(AutomationOutput {
                success: true,
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        }

        fn failure(stderr: &str) -> Result<AutomationOutput> {
            Ok(AutomationOutput {
                success: false,
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            })
        }
    }

    impl FinderAutomation for FakeAutomation {
        fn run(&mut self, source: &str, args: &[OsString]) -> Result<AutomationOutput> {
            self.calls
                .lock()
                .expect("record calls")
                .push((source.to_string(), args.to_vec()));
            self.responses
                .pop_front()
                .unwrap_or_else(|| bail!("unexpected automation call"))
        }
    }

    fn session(fake: FakeAutomation) -> FinderRevealSession {
        FinderRevealSession {
            automation: Box::new(fake),
            window_id: None,
        }
    }

    #[test]
    fn first_reveal_creates_and_stores_window_id() {
        let (fake, calls) = FakeAutomation::responding([FakeAutomation::success("41\n")]);
        let mut session = session(fake);

        session
            .reveal(Path::new("/tmp/report.txt"))
            .expect("reveal");

        assert_eq!(session.window_id, Some(41));
        let calls = calls.lock().expect("calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, CREATE_WINDOW_SCRIPT);
    }

    #[test]
    fn second_reveal_reuses_stored_window_id() {
        let (fake, calls) = FakeAutomation::responding([
            FakeAutomation::success("41"),
            FakeAutomation::success("41"),
        ]);
        let mut session = session(fake);

        session
            .reveal(Path::new("/tmp/one.txt"))
            .expect("first reveal");
        session
            .reveal(Path::new("/tmp/two.txt"))
            .expect("second reveal");

        assert_eq!(session.window_id, Some(41));
        let calls = calls.lock().expect("calls");
        assert_eq!(calls[1].0, RETARGET_WINDOW_SCRIPT);
        assert_eq!(calls[1].1[1], OsString::from("41"));
    }

    #[test]
    fn missing_window_creates_one_replacement() {
        let (fake, calls) = FakeAutomation::responding([
            FakeAutomation::success("41"),
            FakeAutomation::success("MISSING"),
            FakeAutomation::success("77"),
        ]);
        let mut session = session(fake);

        session
            .reveal(Path::new("/tmp/one.txt"))
            .expect("first reveal");
        session
            .reveal(Path::new("/tmp/two.txt"))
            .expect("replacement reveal");

        assert_eq!(session.window_id, Some(77));
        let calls = calls.lock().expect("calls");
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2].0, CREATE_WINDOW_SCRIPT);
    }

    #[test]
    fn close_targets_stored_window_once_and_is_idempotent() {
        let (fake, calls) = FakeAutomation::responding([
            FakeAutomation::success("41"),
            FakeAutomation::success("CLOSED"),
        ]);
        let mut session = session(fake);
        session
            .reveal(Path::new("/tmp/report.txt"))
            .expect("reveal");

        session.close().expect("close");
        session.close().expect("second close");

        assert_eq!(session.window_id, None);
        let calls = calls.lock().expect("calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, CLOSE_WINDOW_SCRIPT);
        assert_eq!(calls[1].1, vec![OsString::from("41")]);
    }

    #[test]
    fn drop_after_early_return_closes_stored_window_once() {
        let (fake, calls) = FakeAutomation::responding([
            FakeAutomation::success("41"),
            FakeAutomation::success("CLOSED"),
        ]);
        let mut session = session(fake);
        session
            .reveal(Path::new("/tmp/report.txt"))
            .expect("reveal");

        drop(session);

        let calls = calls.lock().expect("calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, CLOSE_WINDOW_SCRIPT);
    }

    #[test]
    fn nonzero_status_and_malformed_id_are_errors() {
        let (fake, _) = FakeAutomation::responding([FakeAutomation::failure("denied")]);
        let mut failed = session(fake);
        let error = failed
            .reveal(Path::new("/tmp/report.txt"))
            .expect_err("nonzero status fails");
        assert!(error.to_string().contains("osascript failed"));

        let (fake, _) = FakeAutomation::responding([FakeAutomation::success("window")]);
        let mut malformed = session(fake);
        let error = malformed
            .reveal(Path::new("/tmp/report.txt"))
            .expect_err("malformed id fails");
        assert!(error.to_string().contains("invalid window ID"));
    }

    #[test]
    fn paths_are_positional_arguments_not_applescript_source() {
        let path = Path::new("/tmp/quote ' space; $(bad).txt");
        let (fake, calls) = FakeAutomation::responding([FakeAutomation::success("41")]);
        let mut session = session(fake);

        session.reveal(path).expect("reveal");

        let calls = calls.lock().expect("calls");
        let (source, args) = &calls[0];
        assert_eq!(source, CREATE_WINDOW_SCRIPT);
        assert!(!source.contains(path.to_str().expect("utf-8 path")));
        assert_eq!(args, &vec![path.as_os_str().to_os_string()]);
    }

    #[test]
    fn reveal_scripts_raise_activate_and_select_the_item() {
        for script in [CREATE_WINDOW_SCRIPT, RETARGET_WINDOW_SCRIPT] {
            assert!(script.contains("set target of finderWindow to parentRef"));
            assert!(script.contains("set index of finderWindow to 1"));
            assert!(script.contains("activate"));
            assert!(script.contains("select itemRef"));
            assert!(!script.contains("set selection of finderWindow"));
        }
        assert!(CREATE_WINDOW_SCRIPT.contains("open parentRef"));
    }
}
