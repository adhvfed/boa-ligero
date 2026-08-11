use std::{
    cell::{Cell, RefCell},
    pin::pin,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Poll, Waker},
    time::Duration,
};

use futures_lite::future;

use crate::{
    Context, JsValue, Source, TestAction,
    context::{ContextBuilder, time::FixedClock},
    job::{
        GenericJob, JobExecutor, JobExecutorMetrics, JobRunStatus, NativeAsyncJob,
        SimpleJobExecutor, TimeoutJob,
    },
    run_test_actions_with,
};

#[test]
fn default_executor_profiles_jobs_only_when_enabled_and_resets_snapshots() {
    let mut context = Context::default();
    assert!(context.take_job_executor_metrics().is_none());

    context.set_job_executor_profiling(true);
    context
        .eval(Source::from_bytes(
            "Promise.resolve(1).then(value => value + 1).then(() => undefined)",
        ))
        .expect("enqueue promise reactions");
    context.run_jobs().expect("drain promise reactions");

    let metrics = context
        .take_job_executor_metrics()
        .expect("default executor supports profiling");
    assert_eq!(metrics.run_calls, 1);
    assert!(metrics.scheduler_iterations > 0);
    assert_eq!(metrics.promise_jobs, 2);
    assert!(metrics.wall_time >= metrics.promise_time);

    assert_eq!(
        context
            .take_job_executor_metrics()
            .expect("profiling remains enabled"),
        JobExecutorMetrics::default(),
        "taking a snapshot resets the counters"
    );
    context.set_job_executor_profiling(false);
    assert!(context.take_job_executor_metrics().is_none());
}

#[test]
fn synchronous_executor_parks_instead_of_spinning_on_async_jobs() {
    let mut context = Context::default();
    context.set_job_executor_profiling(true);

    let ready = Arc::new(AtomicBool::new(false));
    let polls = Arc::new(AtomicUsize::new(0));
    let waker = Arc::new(Mutex::new(None::<Waker>));
    let worker = {
        let ready = Arc::clone(&ready);
        let waker = Arc::clone(&waker);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            ready.store(true, Ordering::Release);
            if let Some(waker) = waker.lock().expect("delayed future waker").take() {
                waker.wake();
            }
        })
    };

    context.enqueue_job(
        NativeAsyncJob::new({
            let ready = Arc::clone(&ready);
            let polls = Arc::clone(&polls);
            let waker = Arc::clone(&waker);
            async move |_| {
                std::future::poll_fn(move |cx| {
                    polls.fetch_add(1, Ordering::Relaxed);
                    if ready.load(Ordering::Acquire) {
                        return Poll::Ready(());
                    }
                    *waker.lock().expect("delayed future waker") = Some(cx.waker().clone());
                    if ready.load(Ordering::Acquire) {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
                Ok(JsValue::undefined())
            }
        })
        .into(),
    );
    context.run_jobs().expect("complete delayed async job");
    worker.join().expect("delayed future worker");

    let metrics = context
        .take_job_executor_metrics()
        .expect("profiling snapshot");
    assert_eq!(metrics.async_jobs, 1);
    assert_eq!(metrics.async_completions, 1);
    assert_eq!(metrics.async_waits, 1);
    assert!(
        metrics.scheduler_iterations <= 3,
        "parked execution should not spin the scheduler: {metrics:?}"
    );
    assert!(
        polls.load(Ordering::Relaxed) <= 3,
        "the delayed future should be polled only around wakeup"
    );
}

#[test]
fn detached_async_jobs_survive_non_blocking_drains() {
    let mut context = Context::default();
    let ready = Arc::new(AtomicBool::new(false));
    let polls = Arc::new(AtomicUsize::new(0));
    let waker = Arc::new(Mutex::new(None::<Waker>));
    let completed = Rc::new(Cell::new(false));

    context.enqueue_job(
        NativeAsyncJob::from_future(
            {
                let ready = Arc::clone(&ready);
                let polls = Arc::clone(&polls);
                let waker = Arc::clone(&waker);
                std::future::poll_fn(move |cx| {
                    polls.fetch_add(1, Ordering::Relaxed);
                    if ready.load(Ordering::Acquire) {
                        Poll::Ready(())
                    } else {
                        *waker.lock().expect("detached future waker") = Some(cx.waker().clone());
                        Poll::Pending
                    }
                })
            },
            {
                let completed = Rc::clone(&completed);
                move |(), _| {
                    completed.set(true);
                    Ok(JsValue::undefined())
                }
            },
        )
        .into(),
    );

    assert_eq!(
        context.run_jobs_until_stalled().expect("poll detached job"),
        JobRunStatus::Pending
    );
    assert_eq!(polls.load(Ordering::Relaxed), 1);
    assert!(!completed.get());

    ready.store(true, Ordering::Release);
    if let Some(waker) = waker.lock().expect("detached future waker").take() {
        waker.wake();
    }

    assert_eq!(
        context
            .run_jobs_until_stalled()
            .expect("complete detached job"),
        JobRunStatus::Complete
    );
    assert_eq!(polls.load(Ordering::Relaxed), 2);
    assert!(completed.get());
}

#[test]
fn non_blocking_drain_starts_all_detached_jobs_before_stalling() {
    const JOBS: usize = 8;

    let mut context = Context::default();
    let polls = Arc::new((0..JOBS).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>());
    for index in 0..JOBS {
        context.enqueue_job(
            NativeAsyncJob::from_future(
                {
                    let polls = Arc::clone(&polls);
                    std::future::poll_fn(move |_| {
                        polls[index].fetch_add(1, Ordering::Relaxed);
                        Poll::<()>::Pending
                    })
                },
                |(), _| Ok(JsValue::undefined()),
            )
            .into(),
        );
    }

    assert_eq!(
        context
            .run_jobs_until_stalled()
            .expect("start detached jobs"),
        JobRunStatus::Pending
    );
    assert!(
        polls.iter().all(|count| count.load(Ordering::Relaxed) == 1),
        "a non-blocking pass must start every independent transport"
    );
}

#[test]
fn non_blocking_drain_stalls_at_future_clock_jobs() {
    let clock = Rc::new(FixedClock::default());
    let mut context = ContextBuilder::default()
        .clock(clock.clone())
        .build()
        .expect("build context");
    let completed = Rc::new(Cell::new(false));

    context.enqueue_job(
        TimeoutJob::from_duration(
            {
                let completed = Rc::clone(&completed);
                move |_| {
                    completed.set(true);
                    Ok(JsValue::undefined())
                }
            },
            Duration::from_millis(10),
        )
        .into(),
    );

    assert_eq!(
        context.run_jobs_until_stalled().expect("stall at timer"),
        JobRunStatus::Pending
    );
    assert!(!completed.get());

    clock.forward(11);
    assert_eq!(
        context.run_jobs_until_stalled().expect("run due timer"),
        JobRunStatus::Complete
    );
    assert!(completed.get());
}

#[test]
fn test_async_job_not_blocking_event_loop() {
    let clock = Rc::new(FixedClock::default());
    let context = &mut ContextBuilder::default()
        .clock(clock.clone())
        .build()
        .unwrap();

    run_test_actions_with(
        [TestAction::inspect_context_async(async move |ctx| {
            let executor = ctx.downcast_job_executor::<SimpleJobExecutor>().unwrap();
            let ctx = &RefCell::new(ctx);

            let mut event_loop = pin!(future::poll_once(executor.run_jobs_async(ctx)));

            // There are no jobs in our queue. Push
            // an async job that will consistently yield to the executor.
            ctx.borrow_mut().enqueue_job(
                NativeAsyncJob::new(async |_| {
                    loop {
                        future::yield_now().await;
                    }
                })
                .into(),
            );

            // Then, start the event loop
            assert!(event_loop.as_mut().await.is_none());

            let checker = Rc::new(Cell::new(false));
            {
                let checker = checker.clone();
                // At this point, the event loop should have yielded again to the async executor.
                // Thus, enqueue a generic job that should resolve in the next loop.
                let realm = ctx.borrow().realm().clone();
                ctx.borrow_mut().enqueue_job(
                    GenericJob::new(
                        move |_| {
                            checker.set(true);
                            Ok(JsValue::undefined())
                        },
                        realm,
                    )
                    .into(),
                );
            }

            // Next iteration of the event loop
            assert!(event_loop.as_mut().await.is_none());

            // At this point, our generic job should have been executed.
            assert!(checker.get());
        })],
        context,
    );
}
