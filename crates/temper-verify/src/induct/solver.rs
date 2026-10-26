//! A z3 session over SMT-LIB: declarations stay across `push`/`pop`, so each
//! obligation is a few lines sent to a solver that already holds the
//! vocabulary.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

pub(super) struct Z3 {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    /// Everything sent, to replay a session by hand.
    pub(super) script: String,
    /// The errors the solver reported, such as a lemma naming what the
    /// vocabulary does not hold.
    pub(super) errors: Vec<String>,
}

/// What the solver says of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sat {
    Sat,
    Unsat,
    Unknown,
}

impl Z3 {
    pub(super) fn start() -> std::io::Result<Self> {
        let mut child = Command::new(std::env::var("Z3").unwrap_or_else(|_| "z3".to_string()))
            .args(["-in", "-smt2"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child.stdin.take().expect("piped stdin");
        let output = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut z3 = Self {
            child,
            input,
            output,
            script: String::new(),
            errors: Vec::new(),
        };
        let timeout = std::env::var("INDUCT_TIMEOUT_MS").unwrap_or_else(|_| "2000".to_string());
        z3.send(&format!(
            "(set-option :print-success false)\n(set-option :produce-models true)\n(set-option :timeout {timeout})\n"
        ))?;
        Ok(z3)
    }

    pub(super) fn send(&mut self, text: &str) -> std::io::Result<()> {
        self.script.push_str(text);
        self.input.write_all(text.as_bytes())
    }

    /// Check what was asserted, and how long the solver took.
    pub(super) fn check(&mut self) -> std::io::Result<(Sat, Duration)> {
        let start = Instant::now();
        self.send("(check-sat)\n")?;
        self.input.flush()?;
        let answer = self.read()?;
        let took = start.elapsed();
        let sat = match answer.trim() {
            "sat" => Sat::Sat,
            "unsat" => Sat::Unsat,
            _ => Sat::Unknown,
        };
        Ok((sat, took))
    }

    /// The values of `terms` in the model of the last check.
    pub(super) fn values(&mut self, terms: &[String]) -> std::io::Result<String> {
        if terms.is_empty() {
            return Ok(String::new());
        }
        self.send(&format!("(get-value ({}))\n", terms.join(" ")))?;
        self.input.flush()?;
        self.read()
    }

    /// One response: a line, or an s-expression across lines. Errors the
    /// solver prints before it are kept aside.
    fn read(&mut self) -> std::io::Result<String> {
        loop {
            let response = self.read_one()?;
            if response.trim_start().starts_with("(error") {
                self.errors.push(response.trim().to_string());
            } else {
                return Ok(response);
            }
        }
    }

    fn read_one(&mut self) -> std::io::Result<String> {
        let mut out = String::new();
        let mut depth: i64 = 0;
        loop {
            let mut line = String::new();
            if self.output.read_line(&mut line)? == 0 {
                return Err(std::io::Error::other(format!("z3 ended: {out}")));
            }
            for c in line.chars() {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
            }
            out.push_str(&line);
            if depth <= 0 && !out.trim().is_empty() {
                return Ok(out);
            }
        }
    }
}

impl Drop for Z3 {
    fn drop(&mut self) {
        let _ = self.input.write_all(b"(exit)\n");
        let _ = self.input.flush();
        let _ = self.child.wait();
    }
}
