//! A z3 session over SMT-LIB, in this process: declarations stay across
//! `push`/`pop`, so each obligation is a few lines sent to a solver that
//! already holds the vocabulary. The server image carries libz3 but no z3
//! binary, so the session runs on its own z3 context through the C API's
//! SMT-LIB command reader.

use std::ffi::{CStr, CString};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use z3_sys::{
    ErrorCode, Z3_context, Z3_del_config, Z3_del_context, Z3_eval_smtlib2_string,
    Z3_get_error_code, Z3_get_error_msg, Z3_mk_config, Z3_mk_context, Z3_set_error_handler,
};

/// How long one check may run before the solver answers unknown.
const CHECK_TIMEOUT_MS: &str = "2000";

pub(super) struct Z3 {
    context: Z3_context,
    /// Everything sent, to replay a session by hand, when `INDUCT_SCRIPT`
    /// asks for it.
    pub(super) script: Option<String>,
    /// The errors the solver reported, such as a lemma naming what the
    /// vocabulary does not hold.
    pub(super) errors: Vec<String>,
}

/// What the solver says of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Answer {
    Sat,
    Unsat,
    Unknown,
}

impl Z3 {
    pub(super) fn start() -> std::io::Result<Self> {
        // JUSTIFICATION: the z3 crate has no safe way to read SMT-LIB
        // commands, and the obligations are SMT-LIB text.
        //
        // SAFETY: the config is used once and freed; the context is owned by
        // this session alone, used only from the thread that holds it (the
        // session is not Send), and freed in Drop. With no error handler,
        // Z3 reports errors through the error code instead of exiting.
        let context = unsafe {
            let config = Z3_mk_config().ok_or_else(|| std::io::Error::other("z3: no config"))?;
            let context = Z3_mk_context(config);
            Z3_del_config(config);
            let context = context.ok_or_else(|| std::io::Error::other("z3: no context"))?;
            Z3_set_error_handler(context, None);
            context
        };
        let mut z3 = Self {
            context,
            script: std::env::var("INDUCT_SCRIPT").ok().map(|_| String::new()),
            errors: Vec::new(),
        };
        z3.send(&format!(
            "(set-option :print-success false)\n(set-option :produce-models true)\n(set-option :timeout {CHECK_TIMEOUT_MS})\n"
        ))?;
        Ok(z3)
    }

    pub(super) fn send(&mut self, text: &str) -> std::io::Result<()> {
        self.eval(text).map(|_| ())
    }

    /// Check what was asserted, and how long the solver took.
    pub(super) fn check(&mut self) -> std::io::Result<(Answer, Duration)> {
        let start = Instant::now();
        let answer = self.eval("(check-sat)\n")?;
        let took = start.elapsed();
        let sat = match answer.trim() {
            "sat" => Answer::Sat,
            "unsat" => Answer::Unsat,
            _ => Answer::Unknown,
        };
        Ok((sat, took))
    }

    /// The values of `terms` in the model of the last check.
    pub(super) fn values(&mut self, terms: &[String]) -> std::io::Result<String> {
        if terms.is_empty() {
            return Ok(String::new());
        }
        self.eval(&format!("(get-value ({}))\n", terms.join(" ")))
    }

    /// Run `text` and return what it printed, less the errors, which are
    /// kept aside.
    fn eval(&mut self, text: &str) -> std::io::Result<String> {
        if let Some(script) = &mut self.script {
            script.push_str(text);
        }
        let text = CString::new(text).map_err(std::io::Error::other)?;
        // SAFETY: the context is live (see `start`); the input is a
        // NUL-terminated string that outlives the call; the output is copied
        // before the next call on this context, which is when Z3 frees it.
        let (printed, failed) = unsafe {
            let out = Z3_eval_smtlib2_string(self.context, text.as_ptr());
            let printed = NonNull::new(out.cast_mut())
                .map(|out| CStr::from_ptr(out.as_ptr()).to_string_lossy().into_owned())
                .unwrap_or_default();
            let code = Z3_get_error_code(self.context);
            let failed = (code != ErrorCode::OK).then(|| {
                let message = Z3_get_error_msg(self.context, code);
                NonNull::new(message.cast_mut())
                    .map(|m| CStr::from_ptr(m.as_ptr()).to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("{code:?}"))
            });
            (printed, failed)
        };
        let mut kept = String::new();
        for response in responses(&printed) {
            if response.trim_start().starts_with("(error") {
                self.errors.push(response.trim().to_string());
            } else {
                kept.push_str(&response);
            }
        }
        if let Some(failed) = failed {
            self.errors.push(format!("(error \"{failed}\")"));
        }
        Ok(kept)
    }
}

/// The responses in `printed`: lines, or s-expressions across lines.
fn responses(printed: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth: i64 = 0;
    for line in printed.split_inclusive('\n') {
        for c in line.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
        }
        current.push_str(line);
        if depth <= 0 && !current.trim().is_empty() {
            out.push(std::mem::take(&mut current));
            depth = 0;
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

impl Drop for Z3 {
    fn drop(&mut self) {
        // SAFETY: the context was made in `start`, is owned by this session
        // alone, and is not used again.
        unsafe { Z3_del_context(self.context) };
    }
}
