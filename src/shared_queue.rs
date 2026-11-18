//! Coordinates execution order among repository workers in one OS account.

use std::collections::BTreeSet;
use std::ffi::{CStr, OsStr};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::model::JobId;
use crate::paths::{self, FileLock, Paths};
use crate::{broker, process};

pub(crate) enum Candidate {
    Idle,
    Pending,
    Ready { job: JobId, order: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub(crate) struct Holder {
    pub(crate) root: PathBuf,
    pub(crate) job: JobId,
    boot: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    schema: u32,
    last_order: u64,
    repositories: BTreeSet<PathBuf>,
    holder: Option<Holder>,
}

pub(crate) struct Queue {
    root: PathBuf,
}

pub(crate) struct Slot {
    queue: Queue,
    holder: Holder,
    _execution: FileLock,
}

impl Queue {
    pub(crate) fn open(create: bool) -> Result<Option<Self>> {
        Self::at(
            account_home()?.join("Library/Application Support/Telete/shared-queue"),
            create,
        )
    }

    pub(crate) fn at(root: PathBuf, create: bool) -> Result<Option<Self>> {
        if create {
            paths::ensure_private(&root)?;
        } else if !paths::private_directory_exists(&root)? {
            return Ok(None);
        }
        Ok(Some(Self { root }))
    }

    fn lock(&self) -> Result<FileLock> {
        let lock = paths::lock(&self.root.join("scheduler.lock"), true)?;
        lock.close_on_exec()?;
        Ok(lock)
    }

    fn state(&self) -> Result<State> {
        let open = || {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(self.root.join("queue.json"))
        };
        let file = match open() {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if self.root.join("execution.lock").try_exists()? {
                    // Observation can race the first submission. Retry after
                    // seeing the lock before reporting missing state.
                    open().context("shared queue state is missing; restore it before continuing")?
                } else {
                    return Ok(State {
                        schema: 1,
                        last_order: 0,
                        repositories: BTreeSet::new(),
                        holder: None,
                    });
                }
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == rustix::process::getuid().as_raw()
                && metadata.mode() & 0o777 == 0o600,
            "shared queue state must be a private user-owned regular file"
        );
        let state: State = serde_json::from_reader(file).context("decode shared queue state")?;
        ensure!(
            state.schema == 1
                && state
                    .repositories
                    .iter()
                    .all(|root| paths::normalized_absolute(root))
                && state.holder.as_ref().is_none_or(|holder| {
                    state.repositories.contains(&holder.root) && !holder.boot.is_empty()
                }),
            "invalid Telete shared queue state"
        );
        Ok(state)
    }

    fn save(&self, state: &State) -> Result<()> {
        paths::atomic_json(&self.root.join("queue.json"), state)
    }

    pub(crate) fn holder(&self) -> Result<Option<Holder>> {
        let Some(holder) = self.state()?.holder else {
            return Ok(None);
        };
        Ok((holder.boot == process::boot_id()?).then_some(holder))
    }

    /// Publish a repository and its next order together with the repository's
    /// local transaction. A crash can leave a gap in ordering.
    pub(crate) fn enqueue<T>(
        &self,
        paths: &Paths,
        apply: impl FnOnce(u64) -> Result<T>,
    ) -> Result<T> {
        let _scheduler = self.lock()?;
        let mut state = self.state()?;
        state.last_order = state
            .last_order
            .checked_add(1)
            .context("Telete shared queue order exhausted")?;
        state.repositories.insert(paths.root.clone());
        self.save(&state)?;
        apply(state.last_order)
    }

    /// Discard a repository registration once its worker settles. Recheck
    /// under the same lock as enqueue so a concurrent submission cannot be
    /// missed. Abandoned registrations are also pruned during claim.
    pub(crate) fn forget_idle(
        &self,
        paths: &Paths,
        idle: impl FnOnce() -> Result<bool>,
    ) -> Result<()> {
        let _scheduler = self.lock()?;
        let mut state = self.state()?;
        if state.repositories.contains(&paths.root)
            && state
                .holder
                .as_ref()
                .is_none_or(|holder| holder.root != paths.root)
            && idle()?
        {
            state.repositories.remove(&paths.root);
            self.save(&state)?;
        }
        Ok(())
    }

    /// The caller owns its worker lock and supplies read-only readiness checks.
    /// The slot flock is inheritable through command execution. Its durable
    /// holder covers process groups that outlive a runner or close descriptors.
    pub(crate) fn try_claim(
        &self,
        paths: &Paths,
        job: &JobId,
        mut candidate: impl FnMut(&Paths) -> Result<Candidate>,
    ) -> Result<Option<Slot>> {
        let _scheduler = self.lock()?;
        let Some(execution) = paths::try_lock(&self.root.join("execution.lock"))? else {
            return Ok(None);
        };
        let mut state = self.state()?;
        ensure!(
            state.repositories.contains(&paths.root),
            "repository has no shared queue registration"
        );
        let boot = process::boot_id()?;
        if let Some(holder) = &state.holder
            && holder.boot == boot
            && !stopped(holder, paths)?
        {
            return Ok(None);
        }

        let mut idle = Vec::new();
        let mut oldest: Option<(u64, &PathBuf, JobId)> = None;
        for root in &state.repositories {
            let repository = Paths::discovery(root.clone());
            match candidate(&repository)? {
                Candidate::Idle => idle.push(root.clone()),
                Candidate::Pending => (),
                Candidate::Ready { job, order } => {
                    ensure!(
                        order > 0 && order <= state.last_order,
                        "repository has invalid shared queue order"
                    );
                    if oldest.as_ref().is_none_or(|(first, ..)| order < *first) {
                        oldest = Some((order, root, job));
                    }
                }
            }
        }
        let holder = oldest.and_then(|(_, root, next)| {
            (root == &paths.root && next == *job).then(|| Holder {
                root: root.clone(),
                job: next,
                boot,
            })
        });
        if !idle.is_empty() || state.holder != holder {
            for root in idle {
                state.repositories.remove(&root);
            }
            state.holder.clone_from(&holder);
            self.save(&state)?;
        }
        Ok(holder.map(|holder| Slot {
            queue: Self {
                root: self.root.clone(),
            },
            holder,
            _execution: execution,
        }))
    }
}

impl Slot {
    pub(crate) fn job(&self) -> &JobId {
        &self.holder.job
    }

    /// Release an idle slot. The caller still owns the repository worker lock.
    /// Leave behind ownership evidence if the command group may still exist.
    pub(crate) fn release(self) -> Result<()> {
        let _scheduler = self.queue.lock()?;
        let mut state = self.queue.state()?;
        ensure!(
            state.holder.as_ref() == Some(&self.holder),
            "shared queue holder changed during execution"
        );
        if broker::slot_stopped(&Paths::discovery(self.holder.root.clone()))? {
            state.holder = None;
            self.queue.save(&state)?;
        }
        Ok(())
    }
}

fn stopped(holder: &Holder, caller: &Paths) -> Result<bool> {
    if holder.root == caller.root {
        return broker::slot_stopped(caller);
    }
    if !paths::private_directory_exists(&holder.root)? {
        return Ok(false);
    }
    // Another worker may be preparing to check the shared slot. Avoid waiting
    // on its worker lock while holding the scheduler lock.
    let Some(worker) = paths::try_lock(&holder.root.join("worker.lock"))? else {
        return Ok(false);
    };
    worker.close_on_exec()?;
    broker::slot_stopped(&Paths::discovery(holder.root.clone()))
}

#[allow(unsafe_code)]
fn account_home() -> Result<PathBuf> {
    let uid = rustix::process::getuid().as_raw();
    let mut buffer = vec![0u8; 4096];
    loop {
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: all output pointers refer to writable storage for the entire
        // call. On success the returned strings borrow the supplied buffer.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &raw mut result,
            )
        };
        if status == libc::ERANGE {
            let size = buffer
                .len()
                .checked_mul(2)
                .context("account data too large")?;
            buffer.resize(size, 0);
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status)).context("look up Telete account");
        }
        ensure!(!result.is_null(), "Telete account has no user record");
        // SAFETY: a successful non-null result points to the initialized record.
        let home = unsafe { (*result).pw_dir };
        ensure!(!home.is_null(), "Telete account has no home directory");
        // SAFETY: getpwuid_r returned a NUL-terminated string in `buffer`.
        let home = Path::new(OsStr::from_bytes(
            unsafe { CStr::from_ptr(home) }.to_bytes(),
        ));
        ensure!(home.is_absolute(), "Telete account home must be absolute");
        return fs::canonicalize(home).context("resolve Telete account home");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn oldest_eligible_repository_has_the_shared_slot() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let queue = Queue::at(root.join("shared"), true).unwrap().unwrap();
        let a = Paths::for_test(root.join("a")).unwrap();
        let b = Paths::for_test(root.join("b")).unwrap();
        let a_job = JobId::new("a-job").unwrap();
        let b_job = JobId::new("b-job").unwrap();
        let a_order = queue.enqueue(&a, Ok).unwrap();
        let b_order = queue.enqueue(&b, Ok).unwrap();
        assert!(a_order < b_order);
        let candidates = |paths: &Paths| {
            let (job, order) = if paths.root == a.root {
                (a_job.clone(), a_order)
            } else {
                (b_job.clone(), b_order)
            };
            Ok(Candidate::Ready { job, order })
        };

        let b_worker = paths::lock(&b.root.join("worker.lock"), true).unwrap();
        assert!(queue.try_claim(&b, &b_job, candidates).unwrap().is_none());
        let slot = queue
            .try_claim(&b, &b_job, |paths| {
                if paths.root == a.root {
                    Ok(Candidate::Pending)
                } else {
                    candidates(paths)
                }
            })
            .unwrap()
            .unwrap();
        assert_eq!(queue.holder().unwrap().unwrap().job, b_job);

        let _a_worker = paths::lock(&a.root.join("worker.lock"), true).unwrap();
        assert!(queue.try_claim(&a, &a_job, candidates).unwrap().is_none());
        // Dropping without release simulates worker loss. The orphaned owner
        // remains recorded until its runtime proves there is no live command.
        drop(slot);
        assert_eq!(queue.holder().unwrap().unwrap().job, b_job);
        assert!(queue.try_claim(&a, &a_job, candidates).unwrap().is_none());
        drop(b_worker);
        let slot = queue.try_claim(&a, &a_job, candidates).unwrap().unwrap();
        slot.release().unwrap();
        assert!(queue.holder().unwrap().is_none());

        queue.forget_idle(&a, || Ok(true)).unwrap();
        assert!(!queue.state().unwrap().repositories.contains(&a.root));
        fs::remove_file(queue.root.join("queue.json")).unwrap();
        assert!(queue.holder().is_err());
        assert!(queue.enqueue(&a, Ok).is_err());
    }

    #[test]
    fn observation_leaves_missing_shared_state_uncreated() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("shared");
        assert!(Queue::at(root.clone(), false).unwrap().is_none());
        assert!(!root.exists());
        let queue = Queue::at(root.clone(), true).unwrap().unwrap();
        assert!(queue.holder().unwrap().is_none());
        assert!(!root.join("scheduler.lock").exists());
    }
}
