//! Scoped suppression for worker panics that are reported through the UI.

use std::cell::Cell;
use std::panic::{self, UnwindSafe};
use std::sync::Once;

thread_local! {
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// Catch an unwind without printing over the terminal. Other threads and
/// panics outside this scope retain the previous hook's normal behavior.
pub(crate) fn catch_quiet<T>(f: impl FnOnce() -> T + UnwindSafe) -> std::thread::Result<T> {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !QUIET.with(Cell::get) {
                previous(info);
            }
        }));
    });
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            QUIET.with(|quiet| quiet.set(self.0));
        }
    }
    let _restore = Restore(QUIET.with(|quiet| quiet.replace(true)));
    panic::catch_unwind(f)
}

#[cfg(test)]
mod tests {
    #[test]
    fn nested_guards_restore_the_previous_scope() {
        assert!(!super::QUIET.with(std::cell::Cell::get));
        let outcome = super::catch_quiet(|| {
            assert!(super::catch_quiet(|| panic!("inner panic")).is_err());
            assert!(super::QUIET.with(std::cell::Cell::get));
            panic!("outer panic");
        });
        assert!(outcome.is_err());
        assert!(!super::QUIET.with(std::cell::Cell::get));
    }

    #[test]
    fn caught_panics_are_quiet_and_unrelated_panics_still_print() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "panic_guard::tests::panic_output_child",
                "--nocapture",
            ])
            .env("LCTR_PANIC_GUARD_TEST_CHILD", "1")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("caught worker panic"));
        assert!(stderr.contains("unrelated panic"));
    }

    #[test]
    fn panic_output_child() {
        if std::env::var_os("LCTR_PANIC_GUARD_TEST_CHILD").is_none() {
            return;
        }
        assert!(super::catch_quiet(|| panic!("caught worker panic")).is_err());
        panic!("unrelated panic");
    }
}
