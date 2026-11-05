//! What follows a command for [`Jobs`]: reads what it prints, waits for
//! its shell, and watches what it left running.

use std::sync::Weak;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::{Exit, Id, Jobs, Life};
use crate::sandbox;

/// How long after the shell exits what it printed may still come, before
/// what holds its output open, or runs on in its group, counts as left
/// running.
const SETTLE: Duration = Duration::from_millis(250);

/// How often a group that outlived its shell is looked at.
const POLL: Duration = Duration::from_millis(250);

/// Reads some of `pipe` into `buf`: how much, or `None` at its end. Never
/// ready once it has ended.
async fn some(pipe: &mut Option<impl tokio::io::AsyncRead + Unpin>, buf: &mut [u8]) -> Option<usize> {
    match pipe {
        Some(pipe) => match pipe.read(buf).await {
            Ok(n @ 1..) => Some(n),
            _ => None,
        },
        None => std::future::pending().await,
    }
}

/// Reads what the command `id` prints and waits for it, telling `jobs`:
/// over once its shell has exited, nothing holds its output open, and its
/// group is empty.
pub(super) async fn watch(
    jobs: Weak<Jobs>,
    id: Id,
    mut child: tokio::process::Child,
    mut stdout: Option<tokio::process::ChildStdout>,
    mut stderr: Option<tokio::process::ChildStderr>,
) {
    let (mut out, mut err) = ([0u8; 8192], [0u8; 8192]);
    let mut exited: Option<(Exit, tokio::time::Instant)> = None;
    let mut left = false;
    let push = |bytes: &[u8]| {
        if let Some(jobs) = jobs.upgrade() {
            jobs.change(id, |job| job.output.push(bytes));
        }
    };
    loop {
        if let Some((exit, at)) = exited
            && stdout.is_none()
            && stderr.is_none()
        {
            let Some(jobs) = jobs.upgrade() else { return };
            if !jobs.group_alive(id) {
                jobs.change(id, |job| {
                    if let Some(group) = job.group.take() {
                        group.disarm();
                    }
                    job.restored = job.guard.take().map(sandbox::Guard::finish).unwrap_or_default();
                    job.life = Life::Over(job.stopping.map_or(exit, Exit::Stopped));
                    job.ended = Some(std::time::Instant::now());
                });
                return;
            }
            if at.elapsed() >= SETTLE && !std::mem::replace(&mut left, true) {
                jobs.change(id, |job| job.life = Life::Left(exit));
            }
            drop(jobs);
            tokio::time::sleep(POLL).await;
            continue;
        }
        let settle = exited.map(|(_, at)| at + SETTLE);
        tokio::select! {
            // stdout first when both have something, so what was written
            // in order reads in order: chosen at random, `echo hi; echo
            // oops >&2` came back as oops, then hi, now and then.
            biased;
            n = some(&mut stdout, &mut out) => match n {
                Some(n) => push(&out[..n]),
                None => stdout = None,
            },
            n = some(&mut stderr, &mut err) => match n {
                Some(n) => push(&err[..n]),
                None => stderr = None,
            },
            status = child.wait(), if exited.is_none() => {
                let exit = status.map_or(Exit::Signal, Exit::of);
                exited = Some((exit, tokio::time::Instant::now()));
                if let Some(jobs) = jobs.upgrade() {
                    jobs.with(id, |job| {
                        if let Some(group) = &mut job.group {
                            group.leader_done();
                        }
                    });
                }
            },
            // The shell is gone and something still holds its output: a
            // server behind `&`. It's left running.
            () = tokio::time::sleep_until(settle.unwrap_or_else(tokio::time::Instant::now)), if settle.is_some() && !left => {
                left = true;
                if let (Some(jobs), Some((exit, _))) = (jobs.upgrade(), exited) {
                    jobs.change(id, |job| job.life = Life::Left(exit));
                }
            },
        }
    }
}
