use std::{cmp::Reverse, collections::BinaryHeap, time::Duration};
use anyhow::Result;
use crossbeam::channel::{Receiver, Sender, after, never, select_biased, unbounded};
use time_provider::{DefaultTimeProvider, TimeProvider};
use self::{job::Job, repeated_job::RepeatedJob};
use crate::shared::{
    id::{self, Id},
    macros::{try_cont, try_skip},
};
mod job;
mod repeated_job;
pub(crate) mod time_provider;
/// Scheduler can run jobs after a specified duration or at a specified
/// interval. These jobs run on a separate thread and are guaranteed to run
/// after at least the specified time. Scheduled jobs must never block the
/// thread as that will stop other jobs from running. Stopping the scheduler
/// will block until the current job is finished and all other jobs in the queue
/// are discarded.
#[derive(Debug)]
pub(crate) struct Scheduler<T, P>
where
    T: Clone + Send + 'static + std::fmt::Debug,
    P: TimeProvider + Clone + Send + 'static + std::fmt::Debug,
{
    add_job_tx: Sender<SchedulerCommand<T>>,
    add_job_rx: Receiver<SchedulerCommand<T>>,
    args: T,
    handle: Option<std::thread::JoinHandle<()>>,
    time_provider: P,
}
impl<T> Scheduler<T, DefaultTimeProvider>
where
    T: Clone + Send + 'static + std::fmt::Debug,
{
    /// Create a new scheduler. The [`args`] will be available to all the jobs
    /// as a parameter by reference. Args must be [`Clone`] because it is cloned
    /// every time the scheduler is started in order to move it to the jobs
    /// thread.
    pub(crate) fn new(args: T) -> Self {
        Scheduler::new_with_provider(args, DefaultTimeProvider)
    }
}
impl<T, P> Scheduler<T, P>
where
    T: Clone + Send + 'static + std::fmt::Debug,
    P: TimeProvider + Clone + Send + 'static + std::fmt::Debug,
{
    /// Create a new scheduler. The [`args`] will be available to all the jobs
    /// as a parameter by reference. Args must be [`Clone`] because it is cloned
    /// every time the scheduler is started in order to move it to the jobs
    /// thread.
    pub(crate) fn new_with_provider(args: T, time_provider: P) -> Self {
        let (add_job_tx, add_job_rx) = unbounded::<SchedulerCommand<T>>();
        Self {
            add_job_tx,
            add_job_rx,
            args,
            handle: None,
            time_provider,
        }
    }
    /// Starts the scheduler if not running already.
    pub(crate) fn start(&mut self) {
        if self.handle.is_some() {
            return;
        }
        let add_job_rx = self.add_job_rx.clone();
        let args = self.args.clone();
        let time_provider = self.time_provider.clone();
        self.handle = Some(
            std::thread::spawn(move || {
                let mut duration = None;
                let mut jobs = BinaryHeap::new();
                loop {
                    let timeout = duration.map_or_else(never, after);
                    log::trace!(duration:?; "Starting new schedule loop");
                    select_biased!(
                        recv(add_job_rx) -> job => { let job = try_cont!(job,
                        "Failed to process scheduler command"); match job {
                        SchedulerCommand::AddJob(job) => { jobs.retain(|
                        Reverse(existing_job) | match & existing_job {
                        JobOrRepeatedJob::Job(existing_job) => existing_job.id != job.id,
                        JobOrRepeatedJob::RepeatedJob(existing_job) => existing_job.id !=
                        job.id, }); jobs.push(Reverse(JobOrRepeatedJob::Job(job))); }
                        SchedulerCommand::AddRepeatedJob(job) => jobs
                        .push(Reverse(JobOrRepeatedJob::RepeatedJob(job))),
                        SchedulerCommand::CancelJob(id) => jobs.retain(| job | match &
                        job.0 { JobOrRepeatedJob::Job(job) => job.id != id,
                        JobOrRepeatedJob::RepeatedJob(job) => job.id != id, }),
                        SchedulerCommand::StopScheduler => break, } } recv(timeout) -> _
                        => log::trace!(jobs:?;
                        "Scheduler timed out, trying to run a job"),
                    );
                    let now = time_provider.now();
                    match jobs.peek() {
                        Some(Reverse(job)) if job.run_at() <= now => {
                            match jobs.pop() {
                                Some(Reverse(JobOrRepeatedJob::Job(job))) => job.run(&args),
                                Some(Reverse(JobOrRepeatedJob::RepeatedJob(mut job))) => {
                                    job.run(&args, now);
                                    jobs.push(Reverse(JobOrRepeatedJob::RepeatedJob(job)));
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                    duration = jobs
                        .peek()
                        .map(|Reverse(job)| job.run_at().saturating_duration_since(now));
                    log::trace!(
                        duration:?, jobs:? = jobs; "Schedule loop finished, waiting"
                    );
                }
            }),
        );
    }
    /// Stops the scheduler. Unprocessed jobs still in the queue are discarded.
    /// Job that is currently running will run to the end and will block the
    /// current thread.
    pub(crate) fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            try_skip!(
                self.add_job_tx.send(SchedulerCommand::StopScheduler),
                "Failed to send stop scheduler command"
            );
            handle.join().expect("Scheduler thread to stop properly");
        }
    }
    /// Schedules a job to run after the specified duration.
    /// A job must guarantee that it will not block the scheduler.
    /// If job with given ID already exists, it will be replaced with the new
    /// one.
    pub(crate) fn schedule_replace(
        &self,
        id: Id,
        timeout: Duration,
        callback: impl FnOnce(&T) -> Result<()> + Send + 'static,
    ) {
        try_skip!(
            self.add_job_tx.send(SchedulerCommand::AddJob(Job::new(id, timeout, self
            .time_provider.now(), callback))), "Failed to schedule a job"
        );
    }
    /// Cancel a scheduled job.
    pub(crate) fn cancel(&self, id: Id) {
        try_skip!(
            self.add_job_tx.send(SchedulerCommand::CancelJob(id)), "Failed to cancel job"
        );
    }
    /// Schedules a job to run after the specified duration.
    /// A job must guarantee that it will not block the scheduler.
    pub(crate) fn schedule(
        &self,
        timeout: Duration,
        callback: impl FnOnce(&T) -> Result<()> + Send + 'static,
    ) {
        let id = id::new();
        self.schedule_replace(id, timeout, callback);
    }
    /// Schedule a repeated job to run at the specified interval.
    /// First run will happen after the specified interval.
    /// A job must guarantee that it will not block the scheduler.
    /// Returns a guard that cancels the job when dropped. Caencellation does
    /// not guarantee that the job will not run at least once more.
    #[must_use = "When the return value is dropped the job is cancelled"]
    pub(crate) fn repeated(
        &self,
        interval: Duration,
        callback: impl FnMut(&T) -> Result<()> + Send + 'static,
    ) -> TaskGuard<T> {
        let id = id::new();
        try_skip!(
            self.add_job_tx.send(SchedulerCommand::AddRepeatedJob(RepeatedJob::new(id,
            interval, self.time_provider.now(), callback))),
            "Failed to schedule a repeated job"
        );
        TaskGuard {
            id,
            job_tx: self.add_job_tx.clone(),
        }
    }
}
impl<T, P> Drop for Scheduler<T, P>
where
    T: Clone + Send + 'static + std::fmt::Debug,
    P: TimeProvider + Clone + Send + 'static + std::fmt::Debug,
{
    fn drop(&mut self) {
        self.stop();
    }
}
/// Cancels the task when dropped
pub(crate) struct TaskGuard<T> {
    id: Id,
    job_tx: Sender<SchedulerCommand<T>>,
}
impl<T> Drop for TaskGuard<T> {
    fn drop(&mut self) {
        try_skip!(
            self.job_tx.send(SchedulerCommand::CancelJob(self.id)),
            "Failed to cancel job"
        );
    }
}
#[derive(Debug)]
enum SchedulerCommand<T> {
    AddJob(Job<T>),
    AddRepeatedJob(RepeatedJob<T>),
    CancelJob(Id),
    StopScheduler,
}
#[derive(Debug)]
enum JobOrRepeatedJob<T> {
    Job(Job<T>),
    RepeatedJob(RepeatedJob<T>),
}
impl<T> JobOrRepeatedJob<T> {
    fn run_at(&self) -> std::time::Instant {
        match self {
            JobOrRepeatedJob::Job(job) => job.run_at,
            JobOrRepeatedJob::RepeatedJob(job) => job.run_at,
        }
    }
}
impl<T> PartialEq for JobOrRepeatedJob<T> {
    fn eq(&self, other: &Self) -> bool {
        self.run_at() == other.run_at()
    }
}
impl<T> Eq for JobOrRepeatedJob<T> {}
impl<T> PartialOrd for JobOrRepeatedJob<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for JobOrRepeatedJob<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.run_at().cmp(&other.run_at())
    }
}
