//! Boa's API to create and customize `ECMAScript` jobs and job queues.
//!
//! [`Job`] is an ECMAScript [Job], or a closure that runs an `ECMAScript` computation when
//! there's no other computation running. The module defines several type of jobs:
//! - [`PromiseJob`] for Promise related jobs.
//! - [`TimeoutJob`] for jobs that run after a certain amount of time.
//! - [`NativeAsyncJob`] for jobs that support [`Future`].
//! - [`NativeJob`] for generic jobs that aren't related to Promises.
//!
//! [`JobCallback`] is an ECMAScript [`JobCallback`] record, containing an `ECMAScript` function
//! that is executed when a promise is either fulfilled or rejected.
//!
//! [`JobExecutor`] is a trait encompassing the required functionality for a job executor; this allows
//! implementing custom event loops, custom handling of Jobs or other fun things.
//! This trait is also accompanied by two implementors of the trait:
//! - [`IdleJobExecutor`], which is an executor that does nothing, and the default executor if no executor is
//!   provided. Useful for hosts that want to disable promises.
//! - [`SimpleJobExecutor`], which is a simple FIFO queue that runs all jobs to completion, bailing
//!   on the first error encountered. This simple executor will block on any async job queued.
//!
//! ## [`Trace`]?
//!
//! Most of the types defined in this module don't implement `Trace`. This is because most jobs can only
//! be run once, and putting a `JobExecutor` on a garbage collected object is not allowed.
//!
//! In addition to that, not implementing `Trace` makes it so that the garbage collector can consider
//! any captured variables inside jobs as roots, since you cannot store jobs within a [`Gc`].
//!
//! [Job]: https://tc39.es/ecma262/#sec-jobs
//! [JobCallback]: https://tc39.es/ecma262/#sec-jobcallback-records
//! [`Gc`]: boa_gc::Gc

use crate::context::time::{JsDuration, JsInstant};
use crate::sys::time;
use crate::{
    Context, JsResult, JsValue,
    object::{JsFunction, NativeObject},
    realm::Realm,
};
use boa_gc::{Finalize, Trace};
use futures_concurrency::future::FutureGroup;
use futures_lite::{Stream, StreamExt, future};
use portable_atomic::AtomicBool;
use std::any::Any;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::mem;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration as ProfileDuration, Instant as ProfileInstant};
use std::{cell::RefCell, collections::VecDeque, fmt::Debug, future::Future, pin::Pin};

/// An ECMAScript [Job Abstract Closure].
///
/// This is basically a synchronous task that needs to be run to progress [`Promise`] objects,
/// or unblock threads waiting on [`Atomics.waitAsync`].
///
/// [Job Abstract Closure]: https://tc39.es/ecma262/#sec-jobs
/// [`Promise`]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Promise
/// [`Atomics.waitAsync`]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Atomics/waitAsync
pub struct NativeJob {
    #[allow(clippy::type_complexity)]
    f: Box<dyn FnOnce(&mut Context) -> JsResult<JsValue>>,
    realm: Option<Realm>,
}

impl Debug for NativeJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeJob").finish_non_exhaustive()
    }
}

impl NativeJob {
    /// Creates a new `NativeJob` from a closure.
    pub fn new<F>(f: F) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self {
            f: Box::new(f),
            realm: None,
        }
    }

    /// Creates a new `NativeJob` from a closure and an execution realm.
    pub fn with_realm<F>(f: F, realm: Realm) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self {
            f: Box::new(f),
            realm: Some(realm),
        }
    }

    /// Gets a reference to the execution realm of the job.
    #[must_use]
    pub const fn realm(&self) -> Option<&Realm> {
        self.realm.as_ref()
    }

    /// Calls the native job with the specified [`Context`].
    ///
    /// # Note
    ///
    /// If the native job has an execution realm defined, this sets the running execution
    /// context to the realm's before calling the inner closure, and resets it after execution.
    pub fn call(self, context: &mut Context) -> JsResult<JsValue> {
        // If realm is not null, each time job is invoked the implementation must perform
        // implementation-defined steps such that execution is prepared to evaluate ECMAScript
        // code at the time of job's invocation.
        if let Some(realm) = self.realm {
            let old_realm = context.enter_realm(realm);

            // Let scriptOrModule be GetActiveScriptOrModule() at the time HostEnqueuePromiseJob is
            // invoked. If realm is not null, each time job is invoked the implementation must
            // perform implementation-defined steps such that scriptOrModule is the active script or
            // module at the time of job's invocation.
            let result = (self.f)(context);

            context.enter_realm(old_realm);

            result
        } else {
            (self.f)(context)
        }
    }
}

/// An ECMAScript [Job Abstract Closure] that can be called multiple times.
///
/// This is basically a synchronous task that needs to be run to progress [`Promise`] objects,
/// or unblock threads waiting on [`Atomics.waitAsync`].
///
/// [Job Abstract Closure]: https://tc39.es/ecma262/#sec-jobs
/// [`Promise`]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Promise
/// [`Atomics.waitAsync`]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Atomics/waitAsync
pub struct NativeJobFn {
    #[allow(clippy::type_complexity)]
    f: Box<dyn Fn(&mut Context) -> JsResult<JsValue>>,
    realm: Option<Realm>,
}

impl Debug for NativeJobFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeJobFn").finish_non_exhaustive()
    }
}

impl NativeJobFn {
    /// Creates a new `NativeJobFn` from a closure.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self {
            f: Box::new(f),
            realm: None,
        }
    }

    /// Creates a new `NativeJob` from a closure and an execution realm.
    pub fn with_realm<F>(f: F, realm: Realm) -> Self
    where
        F: Fn(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self {
            f: Box::new(f),
            realm: Some(realm),
        }
    }

    /// Gets a reference to the execution realm of the job.
    #[must_use]
    pub const fn realm(&self) -> Option<&Realm> {
        self.realm.as_ref()
    }

    /// Calls the native job with the specified [`Context`].
    ///
    /// # Note
    ///
    /// If the native job has an execution realm defined, this sets the running execution
    /// context to the realm's before calling the inner closure, and resets it after execution.
    pub fn call(&self, context: &mut Context) -> JsResult<JsValue> {
        // If realm is not null, each time job is invoked the implementation must perform
        // implementation-defined steps such that execution is prepared to evaluate ECMAScript
        // code at the time of job's invocation.
        if let Some(realm) = self.realm.clone() {
            let old_realm = context.enter_realm(realm);

            // Let scriptOrModule be GetActiveScriptOrModule() at the time HostEnqueuePromiseJob is
            // invoked. If realm is not null, each time job is invoked the implementation must
            // perform implementation-defined steps such that scriptOrModule is the active script or
            // module at the time of job's invocation.
            let result = (self.f)(context);

            context.enter_realm(old_realm);

            result
        } else {
            (self.f)(context)
        }
    }
}

type Callback = Box<dyn FnOnce(&mut Context)>;

/// Token to cancel a [`TimeoutJob`] and [`IntervalJob`].
#[derive(Clone)]
pub struct CancellationToken(Rc<Cell<Vec<Callback>>>);

impl Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("cancelled", &self.revoked())
            .finish_non_exhaustive()
    }
}

impl CancellationToken {
    /// Creates a new cancellation token.
    pub(crate) fn new() -> Self {
        Self(Rc::new(Cell::new(vec![Box::new(|_| {})])))
    }

    /// Sets a callback to run when the cancellation token gets used.
    ///
    /// On debug builds, this will panic if the cancellation token was already
    /// used.
    pub fn push_callback(&self, f: impl FnOnce(&mut Context) + 'static) {
        let mut vec = self.0.take();
        debug_assert!(
            !vec.is_empty(),
            "setting a callback on an already used cancellation token"
        );
        vec.push(Box::new(f));
        self.0.set(vec);
    }

    /// Cancels the [`TimeoutJob`] or [`IntervalJob`] associated with this cancellation token.
    pub fn cancel(&self, context: &mut Context) {
        for job in self.0.take() {
            job(context);
        }
    }

    /// Revokes this cancellation token, making it unusable to cancel its associated job.
    pub(crate) fn revoke(&self) {
        self.0.take();
    }

    /// Returns `true` if this cancellation token has been revoked, either because
    /// `cancel` was called or because its associated job has completed.
    #[must_use]
    pub fn revoked(&self) -> bool {
        let callbacks = self.0.take();
        let cancelled = callbacks.is_empty();
        self.0.set(callbacks);
        cancelled
    }
}

/// An ECMAScript [Job] that runs after a certain amount of time.
///
/// This represents the [HostEnqueueTimeoutJob] operation from the specification.
///
/// [HostEnqueueTimeoutJob]: https://tc39.es/ecma262/#sec-hostenqueuetimeoutjob
#[derive(Debug)]
pub struct TimeoutJob {
    /// The distance in milliseconds in the future when the job should run.
    /// This will be added to the current time when the job is enqueued.
    timeout: JsDuration,
    /// The job to run after the specified timeout.
    job: Option<NativeJob>,
    /// Signals if the timeout job was cancelled.
    cancellation_token: CancellationToken,
}

impl Drop for TimeoutJob {
    fn drop(&mut self) {
        self.cancellation_token.revoke();
    }
}

impl TimeoutJob {
    /// Create a new `TimeoutJob` with a timeout and a job.
    #[must_use]
    pub fn new(job: NativeJob, timeout_in_millis: u64) -> Self {
        Self {
            timeout: JsDuration::from_millis(timeout_in_millis),
            job: Some(job),
            cancellation_token: CancellationToken::new(),
        }
    }

    /// Creates a new `TimeoutJob` from a closure and a timeout as [`std::time::Duration`].
    #[must_use]
    pub fn from_duration<F>(f: F, timeout: impl Into<JsDuration>) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self::new(NativeJob::new(f), timeout.into().as_millis())
    }

    /// Creates a new `TimeoutJob` from a closure, a timeout, and an execution realm.
    #[must_use]
    pub fn with_realm<F>(f: F, realm: Realm, timeout: time::Duration) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self::new(NativeJob::with_realm(f, realm), timeout.as_millis() as u64)
    }

    /// Calls the native job with the specified [`Context`].
    ///
    /// # Note
    ///
    /// If the native job has an execution realm defined, this sets the running execution
    /// context to the realm's before calling the inner closure, and resets it after execution.
    pub fn call(mut self, context: &mut Context) -> JsResult<JsValue> {
        let result = self
            .job
            .take()
            .map_or_else(|| Ok(JsValue::undefined()), |job| job.call(context));
        self.cancellation_token.revoke();
        result
    }

    /// Returns the timeout value in milliseconds since epoch.
    #[inline]
    #[must_use]
    pub fn timeout(&self) -> JsDuration {
        self.timeout
    }

    /// Returns `true` if the timeout was cancelled, and its execution can be skipped.
    #[inline]
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.cancellation_token.revoked()
    }

    /// Returns the [`CancellationToken`] for this timeout job.
    #[must_use]
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }
}

/// An ECMAScript [Job] that runs at a certain interval of time.
///
/// This represents jobs enqueued by APIs such as [`setInterval`].
///
/// [`setInterval`]: https://developer.mozilla.org/en-US/docs/Web/API/Window/setInterval
#[derive(Debug)]
pub struct IntervalJob {
    /// The distance in milliseconds in the future when the job should run.
    /// This will be added to the current time when the job is enqueued.
    interval: JsDuration,
    /// The job to run after every interval of time.
    job: NativeJobFn,
    /// Signals if the timeout job was cancelled.
    cancellation_token: CancellationToken,
}

impl Drop for IntervalJob {
    fn drop(&mut self) {
        self.cancellation_token.revoke();
    }
}

impl IntervalJob {
    /// Create a new `IntervalJob` with an interval and a job.
    #[must_use]
    pub fn new(job: NativeJobFn, interval_in_millis: u64) -> Self {
        Self {
            interval: JsDuration::from_millis(interval_in_millis),
            job,
            cancellation_token: CancellationToken::new(),
        }
    }

    /// Creates a new `IntervalJob` from a closure and an interval as [`std::time::Duration`].
    #[must_use]
    pub fn from_duration<F>(f: F, interval: impl Into<JsDuration>) -> Self
    where
        F: Fn(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self::new(NativeJobFn::new(f), interval.into().as_millis())
    }

    /// Creates a new `TimeoutJob` from a closure, an interval, and an execution realm.
    #[must_use]
    pub fn with_realm<F>(f: F, realm: Realm, interval: time::Duration) -> Self
    where
        F: Fn(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self::new(
            NativeJobFn::with_realm(f, realm),
            interval.as_millis() as u64,
        )
    }

    /// Calls the interval job with the specified [`Context`].
    ///
    /// # Note
    ///
    /// If the interval job has an execution realm defined, this sets the running execution
    /// context to the realm's before calling the inner closure, and resets it after execution.
    pub fn call(&self, context: &mut Context) -> JsResult<JsValue> {
        self.job.call(context)
    }

    /// Returns the interval value in milliseconds.
    #[inline]
    #[must_use]
    pub fn interval(&self) -> JsDuration {
        self.interval
    }

    /// Returns `true` if the interval job was cancelled, and its execution can be skipped.
    #[inline]
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.cancellation_token.revoked()
    }

    /// Returns the [`CancellationToken`] for this interval job.
    #[must_use]
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }
}

/// An ECMAScript Generic [Job].
///
/// This represents the [HostEnqueueGenericJob] operation from the specification, which
/// enqueues a job that is just like a [`PromiseJob`], but unconstrained in relation
/// to priority and ordering.
///
/// [HostEnqueueGenericJob]: https://tc39.es/ecma262/#sec-hostenqueuegenericjob
pub struct GenericJob(NativeJob);

impl Debug for GenericJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenericJob").finish_non_exhaustive()
    }
}

impl GenericJob {
    /// Creates a new `GenericJob` from a closure and an execution realm.
    pub fn new<F>(f: F, realm: Realm) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self(NativeJob::with_realm(f, realm))
    }

    /// Gets a reference to the execution realm of the job.
    #[must_use]
    pub const fn realm(&self) -> &Realm {
        self.0
            .realm
            .as_ref()
            .expect("all generic jobs must have an execution realm")
    }

    /// Calls the `GenericJob` with the specified [`Context`], setting the execution
    /// context to the job's realm before calling the inner closure, and resets it after execution.
    pub fn call(self, context: &mut Context) -> JsResult<JsValue> {
        self.0.call(context)
    }
}

/// The [`Future`] job returned by a [`NativeAsyncJob`] operation.
pub type BoxedFuture<'a> = Pin<Box<dyn Future<Output = JsResult<JsValue>> + 'a>>;

type DetachedJobFuture = Pin<Box<dyn Future<Output = NativeJob> + 'static>>;

#[allow(clippy::type_complexity)]
enum NativeAsyncJobInner {
    Contextual(Box<dyn for<'a> FnOnce(&'a RefCell<&mut Context>) -> BoxedFuture<'a>>),
    Detached(DetachedJobFuture),
}

/// An ECMAScript [Job] that can be run asynchronously.
///
/// This is an additional type of job that is not defined by the specification, enabling running `Future` tasks
/// created by ECMAScript code in an easier way.
pub struct NativeAsyncJob {
    inner: NativeAsyncJobInner,
    realm: Option<Realm>,
}

impl Debug for NativeAsyncJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeAsyncJob")
            .field("f", &"Closure")
            .finish()
    }
}

impl NativeAsyncJob {
    /// Creates a new `NativeAsyncJob` from an async closure.
    pub fn new<F>(f: F) -> Self
    where
        F: AsyncFnOnce(&RefCell<&mut Context>) -> JsResult<JsValue> + 'static,
    {
        Self {
            inner: NativeAsyncJobInner::Contextual(Box::new(move |ctx| {
                Box::pin(async move { f(ctx).await })
            })),
            realm: None,
        }
    }

    /// Creates a new `NativeAsyncJob` from an async closure and an execution realm.
    pub fn with_realm<F>(f: F, realm: Realm) -> Self
    where
        F: AsyncFnOnce(&RefCell<&mut Context>) -> JsResult<JsValue> + 'static,
    {
        Self {
            inner: NativeAsyncJobInner::Contextual(Box::new(move |ctx| {
                Box::pin(async move { f(ctx).await })
            })),
            realm: Some(realm),
        }
    }

    /// Creates an async job whose pending future does not borrow the JavaScript
    /// context.
    ///
    /// The future may be retained and polled across host event-loop turns. Its
    /// output is passed to `complete` with exclusive context access only after
    /// the future is ready.
    pub fn from_future<F, T, C>(future: F, complete: C) -> Self
    where
        F: Future<Output = T> + 'static,
        C: FnOnce(T, &mut Context) -> JsResult<JsValue> + 'static,
        T: 'static,
    {
        Self::from_future_inner(future, complete, None)
    }

    /// Creates a context-independent async job with an execution realm.
    pub fn from_future_with_realm<F, T, C>(future: F, complete: C, realm: Realm) -> Self
    where
        F: Future<Output = T> + 'static,
        C: FnOnce(T, &mut Context) -> JsResult<JsValue> + 'static,
        T: 'static,
    {
        Self::from_future_inner(future, complete, Some(realm))
    }

    fn from_future_inner<F, T, C>(future: F, complete: C, realm: Option<Realm>) -> Self
    where
        F: Future<Output = T> + 'static,
        C: FnOnce(T, &mut Context) -> JsResult<JsValue> + 'static,
        T: 'static,
    {
        let completion_realm = realm.clone();
        let future = Box::pin(async move {
            let output = future.await;
            if let Some(realm) = completion_realm {
                NativeJob::with_realm(move |context| complete(output, context), realm)
            } else {
                NativeJob::new(move |context| complete(output, context))
            }
        });
        Self {
            inner: NativeAsyncJobInner::Detached(future),
            realm,
        }
    }

    /// Gets a reference to the execution realm of the job.
    #[must_use]
    pub const fn realm(&self) -> Option<&Realm> {
        self.realm.as_ref()
    }

    /// Calls the native async job with the specified [`Context`].
    ///
    /// # Note
    ///
    /// If the native async job has an execution realm defined, this sets the running execution
    /// context to the realm's before calling the inner closure, and resets it after execution.
    pub fn call<'a>(self, context: &'a RefCell<&mut Context>) -> BoxedFuture<'a> {
        let NativeAsyncJob { inner, realm } = self;
        let NativeAsyncJobInner::Contextual(f) = inner else {
            let NativeAsyncJobInner::Detached(future) = inner else {
                unreachable!();
            };
            return Box::pin(async move {
                let completion = future.await;
                completion.call(&mut context.borrow_mut())
            });
        };

        // If realm is not null, each time job is invoked the implementation must perform
        // implementation-defined steps such that execution is prepared to evaluate ECMAScript
        // code at the time of job's invocation.
        let mut future = if let Some(realm) = &realm {
            let old_realm = context.borrow_mut().enter_realm(realm.clone());

            // Let scriptOrModule be GetActiveScriptOrModule() at the time HostEnqueuePromiseJob is
            // invoked. If realm is not null, each time job is invoked the implementation must
            // perform implementation-defined steps such that scriptOrModule is the active script or
            // module at the time of job's invocation.
            let result = f(context);

            context.borrow_mut().enter_realm(old_realm);
            result
        } else {
            f(context)
        };

        Box::pin(std::future::poll_fn(move |cx| {
            // We need to do the same dance again since the inner code could assume we're still
            // on the same realm.
            if let Some(realm) = &realm {
                let old_realm = context.borrow_mut().enter_realm(realm.clone());

                let poll_result = future.as_mut().poll(cx);

                context.borrow_mut().enter_realm(old_realm);
                poll_result
            } else {
                future.as_mut().poll(cx)
            }
        }))
    }

    fn into_detached(self) -> Result<DetachedJobFuture, Self> {
        match self.inner {
            NativeAsyncJobInner::Detached(future) => Ok(future),
            inner @ NativeAsyncJobInner::Contextual(_) => Err(Self {
                inner,
                realm: self.realm,
            }),
        }
    }
}

/// An ECMAScript [Job Abstract Closure] executing code related to [`Promise`] objects.
///
/// This represents the [`HostEnqueuePromiseJob`] operation from the specification.
///
/// ### [Requirements]
///
/// - If realm is not null, each time job is invoked the implementation must perform implementation-defined
///   steps such that execution is prepared to evaluate ECMAScript code at the time of job's invocation.
/// - Let `scriptOrModule` be [`GetActiveScriptOrModule()`] at the time `HostEnqueuePromiseJob` is invoked.
///   If realm is not null, each time job is invoked the implementation must perform implementation-defined steps
///   such that `scriptOrModule` is the active script or module at the time of job's invocation.
/// - Jobs must run in the same order as the `HostEnqueuePromiseJob` invocations that scheduled them.
///
/// Of all the requirements, Boa guarantees the first two by its internal implementation of `NativeJob`, meaning
/// implementations of [`JobExecutor`] must only guarantee that jobs are run in the same order as they're enqueued.
///
/// [`Promise`]: https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Promise
/// [`HostEnqueuePromiseJob`]: https://tc39.es/ecma262/#sec-hostenqueuepromisejob
/// [Job Abstract Closure]: https://tc39.es/ecma262/#sec-jobs
/// [Requirements]: https://tc39.es/ecma262/multipage/executable-code-and-execution-contexts.html#sec-hostenqueuepromisejob
/// [`GetActiveScriptOrModule()`]: https://tc39.es/ecma262/multipage/executable-code-and-execution-contexts.html#sec-getactivescriptormodule
pub struct PromiseJob(NativeJob);

impl Debug for PromiseJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromiseJob").finish_non_exhaustive()
    }
}

impl PromiseJob {
    /// Creates a new `PromiseJob` from a closure.
    pub fn new<F>(f: F) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self(NativeJob::new(f))
    }

    /// Creates a new `PromiseJob` from a closure and an execution realm.
    pub fn with_realm<F>(f: F, realm: Realm) -> Self
    where
        F: FnOnce(&mut Context) -> JsResult<JsValue> + 'static,
    {
        Self(NativeJob::with_realm(f, realm))
    }

    /// Gets a reference to the execution realm of the `PromiseJob`.
    #[must_use]
    pub const fn realm(&self) -> Option<&Realm> {
        self.0.realm()
    }

    /// Calls the `PromiseJob` with the specified [`Context`].
    ///
    /// # Note
    ///
    /// If the job has an execution realm defined, this sets the running execution
    /// context to the realm's before calling the inner closure, and resets it after execution.
    pub fn call(self, context: &mut Context) -> JsResult<JsValue> {
        self.0.call(context)
    }
}

/// [`JobCallback`][spec] records.
///
/// [spec]: https://tc39.es/ecma262/#sec-jobcallback-records
#[derive(Trace, Finalize)]
pub struct JobCallback {
    callback: JsFunction,
    host_defined: Box<dyn NativeObject>,
}

impl Debug for JobCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobCallback")
            .field("callback", &self.callback)
            .field("host_defined", &"dyn NativeObject")
            .finish()
    }
}

impl JobCallback {
    /// Creates a new `JobCallback`.
    #[inline]
    pub fn new<T: NativeObject>(callback: JsFunction, host_defined: T) -> Self {
        Self {
            callback,
            host_defined: Box::new(host_defined),
        }
    }

    /// Gets the inner callback of the job.
    #[inline]
    #[must_use]
    pub const fn callback(&self) -> &JsFunction {
        &self.callback
    }

    /// Gets a reference to the host defined additional field as an [`NativeObject`] trait object.
    #[inline]
    #[must_use]
    pub fn host_defined(&self) -> &dyn NativeObject {
        &*self.host_defined
    }

    /// Gets a mutable reference to the host defined additional field as an [`NativeObject`] trait object.
    #[inline]
    pub fn host_defined_mut(&mut self) -> &mut dyn NativeObject {
        &mut *self.host_defined
    }
}

/// A job that needs to be handled by a [`JobExecutor`].
///
/// # Requirements
///
/// The specification defines many types of jobs, but all of them must adhere to a set of requirements:
///
/// - At some future point in time, when there is no running execution context and the execution
///   context stack is empty, the implementation must:
///     - Perform any host-defined preparation steps.
///     - Invoke the Job Abstract Closure.
///     - Perform any host-defined cleanup steps, after which the execution context stack must be empty.
/// - Only one Job may be actively undergoing evaluation at any point in time.
/// - Once evaluation of a Job starts, it must run to completion before evaluation of any other Job starts.
/// - The Abstract Closure must return a normal completion, implementing its own handling of errors.
///
/// Boa is a little bit flexible on the last requirement, since it allows jobs to return either
/// values or errors, but the rest of the requirements must be followed for all conformant implementations.
///
/// Additionally, each job type can have additional requirements that must also be followed in addition
/// to the previous ones.
#[non_exhaustive]
#[derive(Debug)]
pub enum Job {
    /// A `Promise`-related job.
    ///
    /// See [`PromiseJob`] for more information.
    PromiseJob(PromiseJob),
    /// A [`Future`]-related job.
    ///
    /// See [`NativeAsyncJob`] for more information.
    AsyncJob(NativeAsyncJob),
    /// A generic job that is to be executed after a number of milliseconds.
    ///
    /// See [`TimeoutJob`] for more information.
    TimeoutJob(TimeoutJob),
    /// A generic job that is to be executed after intervals of a number of milliseconds.
    ///
    /// See [`TimeoutJob`] for more information.
    IntervalJob(IntervalJob),
    /// A generic job.
    ///
    /// See [`GenericJob`] for more information.
    GenericJob(GenericJob),
    /// A job that will eventually cleanup a `FinalizationRegistry`.
    ///
    /// This job differs slightly from the [spec]; originally it's defined
    /// as being enqueued exactly when a `FinalizationRegistry` needs to call
    /// `FinalizationRegistry::cleanup`, but here it's defined as an async
    /// job that suspends execution until it receives a signal from the engine
    /// that the `FinalizationRegistry` needs to be cleaned up.
    ///
    /// # Execution
    ///
    /// As described on the [spec's section about execution][execution],
    ///
    /// > Because calling `HostEnqueueFinalizationRegistryCleanupJob` is optional,
    /// > registered objects in a `FinalizationRegistry` do not necessarily hold
    /// > that `FinalizationRegistry` live. Implementations may omit `FinalizationRegistry`
    /// > callbacks for any reason, e.g., if the `FinalizationRegistry` itself becomes
    /// > dead, or if the application is shutting down.
    ///
    /// For this reason, it is recommended to exclude `FinalizationRegistry` cleanup
    /// jobs from any condition that exits from [`JobExecutor::run_jobs`].
    ///
    /// By the same token, it is recommended to execute `FinalizationRegistry` cleanup
    /// jobs separately from all other enqueued [`NativeAsyncJob`]s, prioritizing the
    /// execution of all other jobs if possible.
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-weakref-host-hooks
    /// [execution]: https://tc39.es/ecma262/#sec-weakref-execution
    FinalizationRegistryCleanupJob(NativeAsyncJob),
}

impl From<NativeAsyncJob> for Job {
    fn from(native_async_job: NativeAsyncJob) -> Self {
        Job::AsyncJob(native_async_job)
    }
}

impl From<PromiseJob> for Job {
    fn from(promise_job: PromiseJob) -> Self {
        Job::PromiseJob(promise_job)
    }
}

impl From<TimeoutJob> for Job {
    fn from(job: TimeoutJob) -> Self {
        Job::TimeoutJob(job)
    }
}

impl From<IntervalJob> for Job {
    fn from(job: IntervalJob) -> Self {
        Job::IntervalJob(job)
    }
}

impl From<GenericJob> for Job {
    fn from(job: GenericJob) -> Self {
        Job::GenericJob(job)
    }
}

/// Opt-in measurements for one or more [`JobExecutor::run_jobs`] calls.
///
/// The default executor records these only while profiling is enabled, keeping
/// ordinary execution free from clock reads. [`Context::take_job_executor_metrics`]
/// returns and resets the accumulated snapshot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JobExecutorMetrics {
    /// Number of executor drain calls represented by this snapshot.
    pub run_calls: usize,
    /// Total wall time spent inside those drain calls.
    pub wall_time: ProfileDuration,
    /// Number of outer scheduler iterations.
    pub scheduler_iterations: usize,
    /// Promise reaction jobs executed.
    pub promise_jobs: usize,
    /// Wall time spent invoking promise reaction jobs.
    pub promise_time: ProfileDuration,
    /// Slowest individual promise reaction job.
    pub slowest_promise_job: ProfileDuration,
    /// Native async jobs admitted to the active future group.
    pub async_jobs: usize,
    /// Number of non-blocking polls of the async future group.
    pub async_polls: usize,
    /// Native async jobs observed completing.
    pub async_completions: usize,
    /// Wall time spent polling the async future group.
    pub async_poll_time: ProfileDuration,
    /// Number of times the synchronous executor parked for async completion.
    pub async_waits: usize,
    /// Wall time the synchronous executor spent parked for async completion.
    pub async_wait_time: ProfileDuration,
    /// Generic host jobs executed.
    pub generic_jobs: usize,
    /// Wall time spent invoking generic host jobs.
    pub generic_time: ProfileDuration,
    /// Clock-backed timeout or interval jobs executed.
    pub clock_jobs: usize,
    /// Wall time spent invoking clock-backed jobs.
    pub clock_time: ProfileDuration,
}

/// Result of a non-blocking job-queue drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobRunStatus {
    /// No runnable or pending jobs remain.
    Complete,
    /// At least one job remains pending on an external event.
    Pending,
}

/// An executor of `ECMAscript` [Jobs].
///
/// This is the main API that allows creating custom event loops.
///
/// [Jobs]: https://tc39.es/ecma262/#sec-jobs
pub trait JobExecutor: Any {
    /// Enqueues a `Job` on the executor.
    ///
    /// This method combines all the host-defined job enqueueing operations into a single method.
    /// See the [spec] for more information on the requirements that each operation must follow.
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-jobs
    fn enqueue_job(self: Rc<Self>, job: Job, context: &mut Context);

    /// Runs all jobs in the executor.
    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()>;

    /// Runs jobs until the queue is empty or only externally pending work
    /// remains.
    ///
    /// The default implementation drains to completion. Executors that retain
    /// context-independent futures can override this to yield control to the
    /// host event loop without losing pending work.
    fn run_jobs_until_stalled(self: Rc<Self>, context: &mut Context) -> JsResult<JobRunStatus> {
        self.run_jobs(context)?;
        Ok(JobRunStatus::Complete)
    }

    /// Enable or disable opt-in executor measurements.
    ///
    /// Custom executors may ignore this request.
    fn set_profiling_enabled(&self, _enabled: bool) {}

    /// Return and reset accumulated executor measurements.
    ///
    /// Returns `None` when the executor does not support profiling or profiling
    /// is disabled.
    fn take_metrics(&self) -> Option<JobExecutorMetrics> {
        None
    }

    /// Asynchronously runs all jobs in the executor.
    ///
    /// By default forwards to [`JobExecutor::run_jobs`]. Implementors using async should override this
    /// with a proper algorithm to run jobs asynchronously.
    #[expect(async_fn_in_trait, reason = "all our APIs are single-threaded")]
    async fn run_jobs_async(self: Rc<Self>, context: &RefCell<&mut Context>) -> JsResult<()>
    where
        Self: Sized,
    {
        self.run_jobs(&mut context.borrow_mut())
    }
}

/// A job executor that does nothing.
///
/// This executor is mostly useful if you want to disable the promise capabilities of the engine. This
/// can be done by passing it to the [`ContextBuilder`]:
///
/// ```
/// use boa_engine::{
///     context::ContextBuilder,
///     job::{IdleJobExecutor, JobExecutor},
/// };
/// use std::rc::Rc;
///
/// let executor = Rc::new(IdleJobExecutor);
/// let context = ContextBuilder::new().job_executor(executor).build();
/// ```
///
/// [`ContextBuilder`]: crate::context::ContextBuilder
#[derive(Debug, Clone, Copy)]
pub struct IdleJobExecutor;

impl JobExecutor for IdleJobExecutor {
    fn enqueue_job(self: Rc<Self>, _: Job, _: &mut Context) {}

    fn run_jobs(self: Rc<Self>, _: &mut Context) -> JsResult<()> {
        Ok(())
    }
}

#[derive(Debug)]
enum ClockJob {
    Timeout(TimeoutJob),
    Interval(IntervalJob),
}

impl ClockJob {
    fn cancelled(&self) -> bool {
        match self {
            ClockJob::Timeout(t) => t.cancelled(),
            ClockJob::Interval(i) => i.cancelled(),
        }
    }
}

/// A simple FIFO executor that bails on the first error.
///
/// This is the default job executor for the [`Context`], but it is mostly pretty limited
/// for a custom event loop.
///
/// To disable running promise jobs on the engine, see [`IdleJobExecutor`].
#[derive(Default)]
pub struct SimpleJobExecutor {
    promise_jobs: RefCell<VecDeque<PromiseJob>>,
    async_jobs: RefCell<VecDeque<NativeAsyncJob>>,
    detached_jobs: RefCell<FutureGroup<DetachedJobFuture>>,
    finalization_registry_jobs: RefCell<VecDeque<NativeAsyncJob>>,
    clock_jobs: RefCell<BTreeMap<JsInstant, Vec<ClockJob>>>,
    generic_jobs: RefCell<VecDeque<GenericJob>>,
    stop: Arc<AtomicBool>,
    profiling_enabled: Cell<bool>,
    metrics: RefCell<JobExecutorMetrics>,
}

impl SimpleJobExecutor {
    fn clear(&self) {
        self.promise_jobs.borrow_mut().clear();
        self.async_jobs.borrow_mut().clear();
        drop(self.detached_jobs.replace(FutureGroup::new()));
        self.finalization_registry_jobs.borrow_mut().clear();
        self.clock_jobs.borrow_mut().clear();
        self.generic_jobs.borrow_mut().clear();
    }
}

impl Debug for SimpleJobExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimpleJobExecutor").finish_non_exhaustive()
    }
}

impl SimpleJobExecutor {
    /// Creates a new `SimpleJobExecutor`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Gets the cancellation token for this executor.
    ///
    /// Setting the signal to `true` will exit the inner event loop and
    /// stop executing any pending jobs.
    pub fn get_cancellation_token(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queued_jobs_empty() && self.detached_jobs.borrow().is_empty()
    }

    fn queued_jobs_empty(&self) -> bool {
        self.immediate_jobs_empty()
            && self.finalization_registry_jobs.borrow().is_empty()
            && self.clock_jobs.borrow().is_empty()
    }

    fn immediate_jobs_empty(&self) -> bool {
        self.promise_jobs.borrow().is_empty()
            && self.async_jobs.borrow().is_empty()
            && self.generic_jobs.borrow().is_empty()
    }

    fn admit_detached_jobs(&self) {
        let jobs = mem::take(&mut *self.async_jobs.borrow_mut());
        let mut contextual = self.async_jobs.borrow_mut();
        for job in jobs {
            match job.into_detached() {
                Ok(future) => {
                    self.detached_jobs.borrow_mut().insert(future);
                }
                Err(job) => contextual.push_back(job),
            }
        }
    }

    async fn next_detached_job(&self) -> Option<NativeJob> {
        future::poll_fn(|cx| {
            let mut jobs = self.detached_jobs.borrow_mut();
            Stream::poll_next(Pin::new(&mut *jobs), cx)
        })
        .await
    }
}

impl JobExecutor for SimpleJobExecutor {
    fn enqueue_job(self: Rc<Self>, job: Job, context: &mut Context) {
        match job {
            Job::PromiseJob(p) => self.promise_jobs.borrow_mut().push_back(p),
            Job::AsyncJob(a) => self.async_jobs.borrow_mut().push_back(a),
            Job::TimeoutJob(t) => {
                let now = context.clock().now();
                self.clock_jobs
                    .borrow_mut()
                    .entry(now + t.timeout())
                    .or_default()
                    .push(ClockJob::Timeout(t));
            }
            Job::IntervalJob(i) => {
                let now = context.clock().now();
                self.clock_jobs
                    .borrow_mut()
                    .entry(now + i.interval())
                    .or_default()
                    .push(ClockJob::Interval(i));
            }
            Job::GenericJob(g) => self.generic_jobs.borrow_mut().push_back(g),
            Job::FinalizationRegistryCleanupJob(fr) => {
                self.finalization_registry_jobs.borrow_mut().push_back(fr);
            }
        }
    }

    fn run_jobs(self: Rc<Self>, context: &mut Context) -> JsResult<()> {
        future::block_on(
            self.run_jobs_async_impl(&RefCell::new(context), AsyncWaitMode::ParkWhenIdle),
        )
    }

    fn run_jobs_until_stalled(self: Rc<Self>, context: &mut Context) -> JsResult<JobRunStatus> {
        self.admit_detached_jobs();
        if !self.async_jobs.borrow().is_empty()
            || !self.finalization_registry_jobs.borrow().is_empty()
        {
            self.run_jobs(context)?;
            return Ok(JobRunStatus::Complete);
        }

        let context = RefCell::new(context);
        if let Some(result) = future::block_on(future::poll_once(
            self.clone()
                .run_jobs_async_impl(&context, AsyncWaitMode::UntilStalled),
        )) {
            result?;
        }

        Ok(if self.is_empty() {
            JobRunStatus::Complete
        } else {
            JobRunStatus::Pending
        })
    }

    fn set_profiling_enabled(&self, enabled: bool) {
        self.profiling_enabled.set(enabled);
        if !enabled {
            self.metrics.replace(JobExecutorMetrics::default());
        }
    }

    fn take_metrics(&self) -> Option<JobExecutorMetrics> {
        self.profiling_enabled
            .get()
            .then(|| self.metrics.replace(JobExecutorMetrics::default()))
    }

    async fn run_jobs_async(self: Rc<Self>, context: &RefCell<&mut Context>) -> JsResult<()>
    where
        Self: Sized,
    {
        self.run_jobs_async_impl(context, AsyncWaitMode::Cooperative)
            .await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AsyncWaitMode {
    /// Yield to an embedding-owned executor after every scheduler pass.
    Cooperative,
    /// Continue through immediately runnable work, then yield while detached
    /// futures are waiting on an external event.
    UntilStalled,
    /// Park the calling thread on the async future group's registered waker
    /// when no newly queued JavaScript work can run.
    ParkWhenIdle,
}

impl SimpleJobExecutor {
    async fn run_jobs_async_impl(
        self: Rc<Self>,
        context: &RefCell<&mut Context>,
        wait_mode: AsyncWaitMode,
    ) -> JsResult<()> {
        let profiling = self.profiling_enabled.get();
        let run_start = profiling.then(ProfileInstant::now);
        if profiling {
            self.metrics.borrow_mut().run_calls += 1;
        }
        let mut group = FutureGroup::new();
        let mut fr_group = FutureGroup::new();
        loop {
            let mut contextual_deferred = false;
            if profiling {
                self.metrics.borrow_mut().scheduler_iterations += 1;
            }
            if self.stop.load(Ordering::Relaxed) {
                self.stop.store(false, Ordering::Relaxed);
                self.clear();
                return Ok(());
            }

            let async_jobs = mem::take(&mut *self.async_jobs.borrow_mut());
            if profiling {
                self.metrics.borrow_mut().async_jobs += async_jobs.len();
            }
            for job in async_jobs {
                match job.into_detached() {
                    Ok(future) => {
                        self.detached_jobs.borrow_mut().insert(future);
                    }
                    Err(job) => {
                        if wait_mode == AsyncWaitMode::UntilStalled {
                            self.async_jobs.borrow_mut().push_back(job);
                            contextual_deferred = true;
                        } else {
                            group.insert(job.call(context));
                        }
                    }
                }
            }

            if wait_mode == AsyncWaitMode::UntilStalled
                && !self.finalization_registry_jobs.borrow().is_empty()
            {
                contextual_deferred = true;
            } else {
                for job in mem::take(&mut *self.finalization_registry_jobs.borrow_mut()) {
                    fr_group.insert(job.call(context));
                }
            }

            // Dispatch all past-due timeout jobs before the termination check.
            {
                let now = context.borrow().clock().now();
                let jobs_to_run = {
                    let mut timeout_jobs = self.clock_jobs.borrow_mut();
                    let mut jobs_to_keep = timeout_jobs.split_off(&now);
                    jobs_to_keep.retain(|_, jobs| {
                        jobs.retain(|job| !job.cancelled());
                        !jobs.is_empty()
                    });
                    mem::replace(&mut *timeout_jobs, jobs_to_keep)
                };

                for jobs in jobs_to_run.into_values() {
                    for job in jobs {
                        if !job.cancelled() {
                            match job {
                                ClockJob::Timeout(job) => {
                                    let started = profiling.then(ProfileInstant::now);
                                    let result = job.call(&mut context.borrow_mut());
                                    if let Some(started) = started {
                                        let mut metrics = self.metrics.borrow_mut();
                                        metrics.clock_jobs += 1;
                                        metrics.clock_time += started.elapsed();
                                    }
                                    if let Err(err) = result {
                                        self.clear();
                                        return Err(err);
                                    }
                                }
                                ClockJob::Interval(job) => {
                                    let context = &mut context.borrow_mut();
                                    let now = context.clock().now();
                                    let started = profiling.then(ProfileInstant::now);
                                    let result = job.call(context);
                                    if let Some(started) = started {
                                        let mut metrics = self.metrics.borrow_mut();
                                        metrics.clock_jobs += 1;
                                        metrics.clock_time += started.elapsed();
                                    }
                                    if let Err(err) = result {
                                        self.clear();
                                        return Err(err);
                                    }
                                    self.clock_jobs
                                        .borrow_mut()
                                        .entry(now + job.interval())
                                        .or_default()
                                        .push(ClockJob::Interval(job));
                                }
                            }
                        }
                    }
                }
            }

            let detached_jobs_pending = !self.detached_jobs.borrow().is_empty();
            let detached_poll_start =
                (profiling && detached_jobs_pending).then(ProfileInstant::now);
            let detached_result = if detached_jobs_pending {
                let completion = future::poll_once(self.next_detached_job()).await.flatten();
                completion.map(|completion| completion.call(&mut context.borrow_mut()))
            } else {
                None
            };
            if let Some(started) = detached_poll_start {
                let mut metrics = self.metrics.borrow_mut();
                metrics.async_polls += 1;
                metrics.async_poll_time += started.elapsed();
                metrics.async_completions += usize::from(detached_result.is_some());
            }
            if let Some(Err(err)) = detached_result {
                self.clear();
                return Err(err);
            }

            if self.queued_jobs_empty()
                && group.is_empty()
                && self.detached_jobs.borrow().is_empty()
            {
                match future::poll_once(fr_group.next()).await.flatten() {
                    Some(Err(err)) => {
                        self.clear();
                        return Err(err);
                    }
                    _ if !self.is_empty() => {}
                    _ => break,
                }
            }

            let async_poll_start = profiling.then(ProfileInstant::now);
            let async_result = future::poll_once(group.next()).await.flatten();
            if let Some(started) = async_poll_start {
                let mut metrics = self.metrics.borrow_mut();
                metrics.async_polls += 1;
                metrics.async_poll_time += started.elapsed();
                metrics.async_completions += usize::from(async_result.is_some());
            }
            if let Some(Err(err)) = async_result {
                self.clear();
                return Err(err);
            }

            let jobs = mem::take(&mut *self.promise_jobs.borrow_mut());
            for job in jobs {
                let started = profiling.then(ProfileInstant::now);
                let result = job.call(&mut context.borrow_mut());
                if let Some(started) = started {
                    let elapsed = started.elapsed();
                    let mut metrics = self.metrics.borrow_mut();
                    metrics.promise_jobs += 1;
                    metrics.promise_time += elapsed;
                    metrics.slowest_promise_job = metrics.slowest_promise_job.max(elapsed);
                }
                if let Err(err) = result {
                    self.clear();
                    return Err(err);
                }
            }

            let jobs = mem::take(&mut *self.generic_jobs.borrow_mut());
            for job in jobs {
                let started = profiling.then(ProfileInstant::now);
                let result = job.call(&mut context.borrow_mut());
                if let Some(started) = started {
                    let mut metrics = self.metrics.borrow_mut();
                    metrics.generic_jobs += 1;
                    metrics.generic_time += started.elapsed();
                }
                if let Err(err) = result {
                    self.clear();
                    return Err(err);
                }
            }
            context.borrow_mut().clear_kept_objects();

            let detached_jobs_pending = !self.detached_jobs.borrow().is_empty();
            if wait_mode == AsyncWaitMode::ParkWhenIdle
                && self.queued_jobs_empty()
                && (!group.is_empty() || detached_jobs_pending)
            {
                let wait_start = profiling.then(ProfileInstant::now);
                let async_result = match (group.is_empty(), detached_jobs_pending) {
                    (false, true) => {
                        let detached = async {
                            let completion = self.next_detached_job().await?;
                            Some(completion.call(&mut context.borrow_mut()))
                        };
                        future::or(group.next(), detached).await
                    }
                    (false, false) => group.next().await,
                    (true, true) => {
                        let completion = self.next_detached_job().await;
                        completion.map(|job| job.call(&mut context.borrow_mut()))
                    }
                    (true, false) => None,
                };
                if let Some(started) = wait_start {
                    let mut metrics = self.metrics.borrow_mut();
                    metrics.async_waits += 1;
                    metrics.async_wait_time += started.elapsed();
                    metrics.async_completions += usize::from(async_result.is_some());
                }
                if let Some(Err(err)) = async_result {
                    self.clear();
                    return Err(err);
                }
            } else if wait_mode == AsyncWaitMode::UntilStalled {
                if contextual_deferred || self.immediate_jobs_empty() {
                    future::yield_now().await;
                }
            } else if wait_mode == AsyncWaitMode::Cooperative
                && self.queued_jobs_empty()
                && group.is_empty()
                && self.detached_jobs.borrow().is_empty()
            {
            } else {
                future::yield_now().await;
            }
        }

        if let Some(started) = run_start {
            self.metrics.borrow_mut().wall_time += started.elapsed();
        }
        Ok(())
    }
}
