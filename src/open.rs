use std::path::Path;
#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
use std::path::PathBuf;
#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
use std::sync::mpsc::{self, Receiver, Sender};
#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
use std::thread;
#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
use std::time::Duration;

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
use anyhow::anyhow;
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
    #[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
    automation: Box<dyn FinderAutomation>,
    #[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
    window_id: Option<u64>,
    #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
    apple: AppleFinderSession,
}

impl FinderRevealSession {
    pub fn new() -> Self {
        #[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
        {
            Self {
                automation: Box::new(OsascriptAutomation),
                window_id: None,
            }
        }

        #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
        {
            Self {
                apple: AppleFinderSession::new(),
            }
        }

        #[cfg(all(not(target_os = "macos"), not(test)))]
        {
            Self {}
        }
    }

    pub fn reveal(&mut self, path: &Path) -> Result<()> {
        #[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
        {
            self.reveal_with_finder(path)
        }

        #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
        {
            let target = path.to_path_buf();
            self.request_reveal(path)?;
            loop {
                if let Some(response) = self.try_reveal_response() {
                    if response.path == target {
                        return response.result.map_err(|error| anyhow!(error));
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
        }

        #[cfg(all(not(target_os = "macos"), not(test)))]
        {
            reveal_once(path)
        }
    }

    /// Closes only this session's dedicated Finder window.
    pub fn close(&mut self) -> Result<()> {
        #[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
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

        #[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
        {
            self.apple.close()
        }

        #[cfg(all(not(target_os = "macos"), not(test)))]
        {
            Ok(())
        }
    }

    #[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
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

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
pub(crate) struct FinderRevealResponse {
    pub(crate) path: PathBuf,
    pub(crate) result: std::result::Result<(), String>,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
enum AppleFinderCommand {
    Reveal {
        path: PathBuf,
        window_id: Option<u64>,
    },
    Close {
        window_id: u64,
    },
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
enum AppleFinderReply {
    Reveal {
        path: PathBuf,
        result: std::result::Result<u64, String>,
    },
    Close {
        result: std::result::Result<(), String>,
    },
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
struct AppleFinderSession {
    command_tx: Sender<AppleFinderCommand>,
    reply_rx: Receiver<AppleFinderReply>,
    active: bool,
    pending: Option<PathBuf>,
    window_id: Option<u64>,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
impl AppleFinderSession {
    fn new() -> Self {
        initialize_apple_event_runtime();
        let (command_tx, command_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        thread::spawn(move || {
            let script = compile_finder_script();
            while let Ok(command) = command_rx.recv() {
                let reply = match command {
                    AppleFinderCommand::Reveal { path, window_id } => {
                        let result = match &script {
                            Ok(script) => run_finder_script(script, 1, window_id, Some(&path)),
                            Err(error) => Err(error.clone()),
                        };
                        AppleFinderReply::Reveal { path, result }
                    }
                    AppleFinderCommand::Close { window_id } => {
                        let result = match &script {
                            Ok(script) => {
                                run_finder_script(script, 2, Some(window_id), None).map(|_| ())
                            }
                            Err(error) => Err(error.clone()),
                        };
                        AppleFinderReply::Close { result }
                    }
                };
                if reply_tx.send(reply).is_err() {
                    break;
                }
            }
        });
        Self {
            command_tx,
            reply_rx,
            active: false,
            pending: None,
            window_id: None,
        }
    }

    fn dispatch_reveal(&mut self, path: PathBuf) -> Result<()> {
        self.command_tx
            .send(AppleFinderCommand::Reveal {
                path,
                window_id: self.window_id,
            })
            .context("queue Finder reveal")?;
        self.active = true;
        Ok(())
    }

    fn request_reveal(&mut self, path: &Path) -> Result<()> {
        if self.active {
            self.pending = Some(path.to_path_buf());
            return Ok(());
        }
        self.dispatch_reveal(path.to_path_buf())
    }

    fn is_pending(&self) -> bool {
        self.active || self.pending.is_some()
    }

    fn handle_reveal_reply(
        &mut self,
        path: PathBuf,
        result: std::result::Result<u64, String>,
    ) -> FinderRevealResponse {
        self.active = false;
        let result = result.map(|window_id| {
            self.window_id = Some(window_id);
        });
        if let Some(pending) = self.pending.take() {
            let _ = self.dispatch_reveal(pending);
        }
        FinderRevealResponse {
            path,
            result: result.map(|_| ()),
        }
    }

    fn try_reveal_response(&mut self) -> Option<FinderRevealResponse> {
        match self.reply_rx.try_recv().ok()? {
            AppleFinderReply::Reveal { path, result } => {
                Some(self.handle_reveal_reply(path, result))
            }
            AppleFinderReply::Close { .. } => None,
        }
    }

    fn close(&mut self) -> Result<()> {
        self.pending = None;
        while self.active {
            let reply = self.reply_rx.recv().context("receive Finder reveal")?;
            if let AppleFinderReply::Reveal { path, result } = reply {
                let _ = self.handle_reveal_reply(path, result);
            }
        }

        let Some(window_id) = self.window_id.take() else {
            return Ok(());
        };
        self.command_tx
            .send(AppleFinderCommand::Close { window_id })
            .context("queue Finder window close")?;
        match self
            .reply_rx
            .recv()
            .context("receive Finder window close")?
        {
            AppleFinderReply::Close { result } => result.map_err(|error| anyhow!(error)),
            AppleFinderReply::Reveal { .. } => {
                bail!("Finder returned a reveal while closing window {window_id}")
            }
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
fn initialize_apple_event_runtime() {
    use objc2::rc::autoreleasepool;
    use objc2_foundation::NSAppleEventDescriptor;

    autoreleasepool(|_| {
        let _ = NSAppleEventDescriptor::nullDescriptor();
    });
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
impl FinderRevealSession {
    pub(crate) fn request_reveal(&mut self, path: &Path) -> Result<()> {
        self.apple.request_reveal(path)
    }

    pub(crate) fn try_reveal_response(&mut self) -> Option<FinderRevealResponse> {
        self.apple.try_reveal_response()
    }

    pub(crate) fn reveal_pending(&self) -> bool {
        self.apple.is_pending()
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
const APPLE_FINDER_SCRIPT: &str = r#"
on locatorReveal(operation, existingWindowID, filePath)
    set operation to operation as integer
    set existingWindowID to existingWindowID as integer
    tell application "Finder"
        if operation is 1 then
            set itemRef to POSIX file filePath as alias
            set parentRef to container of itemRef
            if existingWindowID is 0 then
                open parentRef
                set finderWindow to front window
            else
                try
                    set finderWindow to window id existingWindowID
                on error
                    open parentRef
                    set finderWindow to front window
                end try
            end if
            set target of finderWindow to parentRef
            set index of finderWindow to 1
            activate
            select itemRef
            return id of finderWindow
        else
            try
                close window id existingWindowID
            on error
                return 0
            end try
            return 0
        end if
    end tell
end locatorReveal
"#;

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
fn compile_finder_script() -> Result<objc2::rc::Retained<objc2_foundation::NSAppleScript>, String> {
    use objc2::AnyThread;
    use objc2_foundation::{NSAppleScript, NSString};

    let source = NSString::from_str(APPLE_FINDER_SCRIPT);
    let Some(script) = NSAppleScript::initWithSource(NSAppleScript::alloc(), &source) else {
        return Err("allocate NSAppleScript".to_string());
    };
    let mut error = None;
    let compiled = unsafe { script.compileAndReturnError(Some(&mut error)) };
    if compiled {
        Ok(script)
    } else {
        Err(format_apple_error(error))
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
fn run_finder_script(
    script: &objc2_foundation::NSAppleScript,
    operation: i32,
    window_id: Option<u64>,
    path: Option<&Path>,
) -> std::result::Result<u64, String> {
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::{msg_send, ClassType};
    use objc2_foundation::{NSAppleEventDescriptor, NSDictionary, NSString};

    autoreleasepool(|_| {
        let arguments = NSAppleEventDescriptor::listDescriptor();
        let operation = NSAppleEventDescriptor::descriptorWithInt32(operation);
        arguments.insertDescriptor_atIndex(&operation, 1);
        let window = NSAppleEventDescriptor::descriptorWithInt32(
            window_id.unwrap_or(0).min(i32::MAX as u64) as i32,
        );
        arguments.insertDescriptor_atIndex(&window, 2);
        let path = path.map_or_else(String::new, |path| path.to_string_lossy().into_owned());
        let path = NSString::from_str(&path);
        let path = NSAppleEventDescriptor::descriptorWithString(&path);
        arguments.insertDescriptor_atIndex(&path, 3);

        let event: objc2::rc::Retained<NSAppleEventDescriptor> = unsafe {
            msg_send![
                NSAppleEventDescriptor::class(),
                appleEventWithEventClass: fourcc(*b"ascr"),
                eventID: fourcc(*b"psbr"),
                targetDescriptor: Option::<&NSAppleEventDescriptor>::None,
                returnID: -1i16,
                transactionID: 0i32
            ]
        };
        let _: () = unsafe {
            msg_send![
                &*event,
                setParamDescriptor: &*arguments,
                forKeyword: fourcc(*b"----")
            ]
        };
        let handler_name = NSString::from_str("locatorReveal");
        let handler_name = NSAppleEventDescriptor::descriptorWithString(&handler_name);
        let _: () = unsafe {
            msg_send![
                &*event,
                setParamDescriptor: &*handler_name,
                forKeyword: fourcc(*b"snam")
            ]
        };
        let mut error: Option<Retained<NSDictionary<NSString, objc2::runtime::AnyObject>>> = None;
        let response: Option<Retained<NSAppleEventDescriptor>> = unsafe {
            msg_send![
                script,
                executeAppleEvent: &*event,
                error: Some(&mut error)
            ]
        };
        if let Some(error) = error {
            return Err(format_apple_error(Some(error)));
        }
        let Some(response) = response else {
            return Err("AppleScript returned no result and no error details".to_string());
        };
        Ok(response.int32Value().max(0) as u64)
    })
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
fn format_apple_error(
    error: Option<
        objc2::rc::Retained<
            objc2_foundation::NSDictionary<objc2_foundation::NSString, objc2::runtime::AnyObject>,
        >,
    >,
) -> String {
    error
        .map(|error| format!("{error:?}"))
        .unwrap_or_else(|| "unknown AppleScript error".to_string())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64", not(test)))]
const fn fourcc(value: [u8; 4]) -> u32 {
    ((value[0] as u32) << 24)
        | ((value[1] as u32) << 16)
        | ((value[2] as u32) << 8)
        | value[3] as u32
}

#[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
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

#[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
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

#[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
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

#[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
struct AutomationOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
trait FinderAutomation {
    fn run(&mut self, source: &str, args: &[std::ffi::OsString]) -> Result<AutomationOutput>;
}

#[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
struct OsascriptAutomation;

#[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
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

#[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
fn ensure_success(output: &AutomationOutput, action: &str) -> Result<()> {
    if output.success {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!("{action}: osascript failed: {stderr}");
}

#[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
fn stdout_text(output: &AutomationOutput, action: &str) -> Result<String> {
    String::from_utf8(output.stdout.clone())
        .with_context(|| format!("{action}: osascript returned non-UTF-8 output"))
        .map(|text| text.trim().to_string())
}

#[cfg(any(test, all(target_os = "macos", not(target_arch = "aarch64"))))]
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
