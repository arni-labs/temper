//! Waiting for the stop signal, and saying which process sent it.

/// Resolves when the process is asked to stop: Ctrl-C, or SIGTERM from the
/// platform it runs on. A signal that cannot be watched never resolves.
/// Says which process sent it, so a stop nobody meant to send can be traced
/// to its sender.
pub(super) async fn stop_signal() {
    #[cfg(unix)]
    stop_sender::watch();
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    let signal = tokio::select! {
        () = interrupt => "SIGINT",
        () = terminate => "SIGTERM",
    };
    #[cfg(unix)]
    {
        let (pid, sender) = stop_sender::told();
        tracing::warn!(signal, sender_pid = pid, sender = %sender, "stopping: {signal} from {sender}");
    }
    #[cfg(not(unix))]
    tracing::warn!(signal, "stopping: {signal}");
}

/// The process that sent the last stop signal. Only a signal's own `siginfo`
/// names its sender, and only a `sigaction` handler sees that, so a handler
/// beside tokio's records the sender's pid and the stop reads it afterwards.
#[cfg(unix)]
mod stop_sender {
    use std::sync::atomic::{AtomicI32, Ordering};

    static SENDER: AtomicI32 = AtomicI32::new(0);

    /// Record the sender of every SIGTERM and SIGINT from now on. Tokio
    /// registers its own handlers through the same registry, which runs both.
    pub(crate) fn watch() {
        for signal in [libc::SIGTERM, libc::SIGINT] {
            // JUSTIFICATION: a signal's sender is only in its siginfo, which only
            // a sigaction handler is given; signal-hook-registry is how tokio
            // installs its own, so registering there keeps tokio's working.
            //
            // SAFETY: the action is async-signal-safe: it reads one field of the
            // siginfo the kernel filled for this signal and stores it in an
            // atomic. It allocates, locks and panics nothing.
            let registered = unsafe {
                signal_hook_registry::register_sigaction(signal, |info: &libc::siginfo_t| {
                    // SAFETY: SIGTERM and SIGINT carry a sender pid in their siginfo.
                    SENDER.store(info.si_pid(), Ordering::Relaxed);
                })
            };
            if let Err(error) = registered {
                tracing::warn!(signal, %error, "cannot watch who sends this stop signal");
            }
        }
    }

    /// The sender's pid (0 when the kernel named none) and what it is: its
    /// command line and its parent's, read now, while it most likely still runs.
    pub(crate) fn told() -> (i32, String) {
        let pid = SENDER.load(Ordering::Relaxed);
        if pid <= 0 {
            return (pid, "no process the kernel named".to_string());
        }
        let parent = parent_of(pid);
        let parent = match parent {
            Some(parent) => format!(
                ", whose parent is process {parent} ({})",
                command_of(parent)
            ),
            None => String::new(),
        };
        (pid, format!("process {pid} ({}){parent}", command_of(pid)))
    }

    /// A process's command line, each argument cut to 80 characters, so a
    /// script given as an argument shows its start and the arguments after it
    /// show whole: what tells one caller from another.
    fn command_of(pid: i32) -> String {
        // determinism-ok: read once, at the stop
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            return "gone, or no /proc".to_string();
        };
        let args: Vec<String> = raw
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| {
                let arg = String::from_utf8_lossy(arg).replace(['\n', '\t'], " ");
                if arg.chars().count() > 80 {
                    format!("{}…", arg.chars().take(80).collect::<String>())
                } else {
                    arg
                }
            })
            .collect();
        if args.is_empty() {
            "no command line".to_string()
        } else {
            args.join(" ")
        }
    }

    /// A process's parent, from `/proc/<pid>/stat`: the field after the state,
    /// which follows the last `)` because the command name may hold one.
    fn parent_of(pid: i32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?; // determinism-ok: read once, at the stop
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }
}
