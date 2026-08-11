use std::cell::Cell;
use std::fmt;
use std::process::{Child, Command};
#[cfg(test)]
use std::sync::TryLockError;
use std::sync::{Mutex, MutexGuard};

thread_local! {
    static GATE_OWNER: Cell<bool> = const { Cell::new(false) };
}

#[derive(Debug)]
struct SpawnReceiveGateReentry;

impl fmt::Display for SpawnReceiveGateReentry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("spawn/receive gate cannot be reentered by its owning thread")
    }
}

impl std::error::Error for SpawnReceiveGateReentry {}

pub(crate) struct SpawnReceiveGate {
    lock: Mutex<()>,
}

pub(crate) static SPAWN_RECEIVE_GATE: SpawnReceiveGate = SpawnReceiveGate::new();

impl SpawnReceiveGate {
    const fn new() -> Self {
        Self {
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn spawn(&self, command: &mut Command) -> std::io::Result<Child> {
        #[cfg(test)]
        notify_spawn_test_hook(SpawnTestEvent::Attempting);
        let _guard = self.acquire_for_spawn().map_err(std::io::Error::other)?;
        #[cfg(test)]
        pause_spawn_test_hook_after_acquire();
        command.spawn()
    }

    fn acquire_for_spawn(&self) -> Result<OwnedGateGuard<'_>, SpawnReceiveGateReentry> {
        refuse_reentry()?;
        #[cfg(test)]
        let guard = match self.lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                notify_spawn_test_hook(SpawnTestEvent::Contended);
                self.lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            }
        };
        #[cfg(not(test))]
        let guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(OwnedGateGuard::new(guard))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn receive_non_atomic<T>(&self, receive_duplicate_close: impl FnOnce() -> T) -> T {
        #[cfg(test)]
        notify_spawn_test_hook(SpawnTestEvent::Attempting);
        #[cfg(test)]
        let guard = self.acquire_for_spawn();
        #[cfg(not(test))]
        let guard = self.acquire();
        let _guard = match guard {
            Ok(guard) => guard,
            Err(error) => std::panic::panic_any(error),
        };
        #[cfg(test)]
        pause_spawn_test_hook_after_acquire();
        receive_duplicate_close()
    }

    #[cfg(all(target_os = "macos", not(test)))]
    fn acquire(&self) -> Result<OwnedGateGuard<'_>, SpawnReceiveGateReentry> {
        refuse_reentry()?;
        let guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(OwnedGateGuard::new(guard))
    }
}

fn refuse_reentry() -> Result<(), SpawnReceiveGateReentry> {
    GATE_OWNER.with(|owned| {
        if owned.get() {
            Err(SpawnReceiveGateReentry)
        } else {
            Ok(())
        }
    })
}

struct OwnedGateGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

impl<'a> OwnedGateGuard<'a> {
    fn new(guard: MutexGuard<'a, ()>) -> Self {
        GATE_OWNER.with(|owned| {
            debug_assert!(!owned.replace(true));
        });
        Self { _guard: guard }
    }
}

impl Drop for OwnedGateGuard<'_> {
    fn drop(&mut self) {
        GATE_OWNER.with(|owned| {
            debug_assert!(owned.replace(false));
        });
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpawnTestEvent {
    Attempting,
    Contended,
    Acquired,
}

#[cfg(test)]
struct SpawnTestEndpoint {
    events: std::sync::mpsc::SyncSender<SpawnTestEvent>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
thread_local! {
    static SPAWN_TEST_HOOK: std::cell::RefCell<Option<SpawnTestEndpoint>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
pub(crate) struct SpawnTestControl {
    events: std::sync::mpsc::Receiver<SpawnTestEvent>,
    resume: std::sync::mpsc::SyncSender<()>,
}

#[cfg(test)]
impl SpawnTestControl {
    fn event(&self) -> SpawnTestEvent {
        self.events.recv().expect("spawn hook event")
    }

    pub(crate) fn event_result(&self) -> Result<SpawnTestEvent, ()> {
        self.events.recv().map_err(|_| ())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn event_timeout(&self, timeout: std::time::Duration) -> Result<SpawnTestEvent, ()> {
        self.events.recv_timeout(timeout).map_err(|_| ())
    }

    pub(crate) fn resume(&self) {
        self.resume.send(()).expect("resume gated spawn")
    }
}

#[cfg(test)]
pub(crate) struct SpawnTestInstaller(Option<SpawnTestEndpoint>);

#[cfg(test)]
impl SpawnTestInstaller {
    pub(crate) fn install(mut self) -> InstalledSpawnTestHook {
        let endpoint = self.0.take().expect("spawn hook installed once");
        SPAWN_TEST_HOOK.with(|hook| {
            assert!(hook.borrow_mut().replace(endpoint).is_none());
        });
        InstalledSpawnTestHook
    }
}

#[cfg(test)]
pub(crate) struct InstalledSpawnTestHook;

#[cfg(test)]
impl Drop for InstalledSpawnTestHook {
    fn drop(&mut self) {
        SPAWN_TEST_HOOK.with(|hook| {
            hook.borrow_mut().take();
        });
    }
}

#[cfg(test)]
pub(crate) fn spawn_test_hook() -> (SpawnTestControl, SpawnTestInstaller) {
    let (event_tx, event_rx) = std::sync::mpsc::sync_channel(0);
    let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
    (
        SpawnTestControl {
            events: event_rx,
            resume: resume_tx,
        },
        SpawnTestInstaller(Some(SpawnTestEndpoint {
            events: event_tx,
            resume: resume_rx,
        })),
    )
}

#[cfg(test)]
fn notify_spawn_test_hook(event: SpawnTestEvent) {
    SPAWN_TEST_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook.events.send(event).expect("observe gated spawn");
        }
    });
}

#[cfg(test)]
fn pause_spawn_test_hook_after_acquire() {
    notify_spawn_test_hook(SpawnTestEvent::Acquired);
    SPAWN_TEST_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook.resume.recv().expect("resume gated spawn");
        }
    });
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::SpawnReceiveGateReentry;
    use super::{SpawnReceiveGate, SpawnTestEvent, spawn_test_hook};
    #[cfg(target_os = "macos")]
    use std::io::Write;
    #[cfg(target_os = "macos")]
    use std::process::Stdio;
    use std::sync::Arc;

    fn child_command(test: &str) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", test]);
        command
    }

    #[test]
    fn spawn_uses_the_process_wide_gate() {
        let gate = SpawnReceiveGate::new();
        let mut command = child_command("spawn_receive_gate::tests::child_probe");

        let status = gate.spawn(&mut command).unwrap().wait().unwrap();

        assert!(status.success());
    }

    #[test]
    fn concurrent_spawn_callers_serialize_the_creation_call() {
        let gate = Arc::new(SpawnReceiveGate::new());
        let (first_control, first_install) = spawn_test_hook();
        let first = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                let _hook = first_install.install();
                gate.spawn(&mut child_command("spawn_receive_gate::tests::child_probe"))
            })
        };
        assert_eq!(first_control.event(), SpawnTestEvent::Attempting);
        assert_eq!(first_control.event(), SpawnTestEvent::Acquired);

        let (second_control, second_install) = spawn_test_hook();
        let second = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                let _hook = second_install.install();
                gate.spawn(&mut child_command("spawn_receive_gate::tests::child_probe"))
            })
        };
        assert_eq!(second_control.event(), SpawnTestEvent::Attempting);
        assert_eq!(second_control.event(), SpawnTestEvent::Contended);

        first_control.resume();
        let mut first_child = first.join().unwrap().unwrap();
        assert_eq!(second_control.event(), SpawnTestEvent::Acquired);
        second_control.resume();
        let mut second_child = second.join().unwrap().unwrap();
        assert!(first_child.wait().unwrap().success());
        assert!(second_child.wait().unwrap().success());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn same_thread_reentry_is_a_typed_refusal() {
        let gate = SpawnReceiveGate::new();
        gate.receive_non_atomic(|| {
            let error = gate
                .spawn(&mut child_command("spawn_receive_gate::tests::child_probe"))
                .unwrap_err();
            assert!(
                error
                    .get_ref()
                    .and_then(|source| source.downcast_ref::<SpawnReceiveGateReentry>())
                    .is_some()
            );
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn poisoned_gate_is_recovered_after_receive_unwinds() {
        let gate = SpawnReceiveGate::new();
        let unwind = std::panic::catch_unwind(|| gate.receive_non_atomic(|| panic!("poison")));
        assert!(unwind.is_err());

        let status = gate
            .spawn(&mut child_command("spawn_receive_gate::tests::child_probe"))
            .unwrap()
            .wait()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn child_wait_is_outside_the_gate() {
        let gate = Arc::new(SpawnReceiveGate::new());
        let mut command = child_command("spawn_receive_gate::tests::stdin_child_probe");
        command
            .env("MARION_SPAWN_GATE_STDIN_PROBE", "1")
            .stdin(Stdio::piped());
        let mut child = gate.spawn(&mut command).unwrap();
        assert!(child.try_wait().unwrap().is_none());

        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
        let receiver = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || gate.receive_non_atomic(|| entered_tx.send(()).unwrap()))
        };
        entered_rx.recv().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        child.stdin.take().unwrap().write_all(b"done").unwrap();
        receiver.join().unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn child_probe() {}

    #[test]
    fn stdin_child_probe() {
        if std::env::var_os("MARION_SPAWN_GATE_STDIN_PROBE").is_none() {
            return;
        }
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).unwrap();
        assert_eq!(input, "done");
    }
}
