//! Use Julia with support for multitasking.
//!
//! While access to the Julia C API is not thread-safe, it is possible to create and schedule new
//! tasks from the thread that has intialized Julia. To do so from Rust you must use an async
//! runtime rather than the sync runtime.
//!
//! In order to use an async runtime, you'll have to choose a backing runtime. By default, tokio
//! and async-std can be used by enabling the `tokio-rt` or `async-std-rt` feature respectively.
//! To use a custom runtime, you can implement the `AsyncRuntime` trait.
//!
//! After initialization, a handle to the runtime, [`AsyncJulia`], is returned which can be shared
//! across threads and can be used to send new tasks to the runtime. Three kinds of task exist:
//! blocking, async, and persistent tasks. Blocking tasks block the runtime, the other two kinds
//! of tasks can schedule Julia function calls and wait for them to complete. While the scheduled
//! Julia function hasn't returned the async runtime handles other tasks. Blocking tasks can be
//! expressed as closures, the other two kinds of task require implementing the [`AsyncTask`] and
//! [`PersistentTask`] traits respectively.

#[cfg(feature = "async-std-rt")]
pub mod async_std_rt;
#[cfg(feature = "tokio-rt")]
pub mod tokio_rt;

use crate::{
    async_util::{
        channel::{Channel, ChannelReceiver, ChannelSender, OneshotSender, TrySendError},
        internal::{
            BlockingTask, CallPersistentMessage, GenericBlockingTask, GenericPendingTask,
            InnerPersistentMessage, PendingTask, Persistent, RegisterPersistent, RegisterTask,
            Task,
        },
        julia_future::wake_task,
        task::{AsyncTask, PersistentTask},
    },
    call::Call,
    error::{JlrsError, JlrsResult},
    info::Info,
    memory::{frame::GcFrame, global::Global, mode::Async, stack_page::AsyncStackPage},
    runtime::{builder::AsyncRuntimeBuilder, init_jlrs, INIT},
    wrappers::ptr::{module::Module, string::JuliaString, value::Value, Wrapper},
};
use async_trait::async_trait;
use futures::Future;
use jl_sys::{
    jl_atexit_hook, jl_init, jl_init_with_image, jl_is_initialized, jl_options, jl_process_events,
};
use std::{
    collections::VecDeque,
    ffi::c_void,
    fmt,
    io::{Error as IOError, ErrorKind},
    marker::PhantomData,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

init_fn!(init_multitask, JLRS_MULTITASK_JL, "JlrsMultitask.jl");

/// Convert `Self` to a `Result`.
pub trait IntoResult<T, E> {
    /// Convert `self` to a `Result`.
    fn into_result(self) -> Result<T, E>;
}

impl<E> IntoResult<(), E> for () {
    fn into_result(self) -> Result<(), E> {
        Ok(self)
    }
}

impl<E> IntoResult<JlrsResult<()>, E> for JlrsResult<()> {
    fn into_result(self) -> Result<JlrsResult<()>, E> {
        Ok(self)
    }
}

impl<E> IntoResult<(), E> for Result<(), E> {
    fn into_result(self) -> Result<(), E> {
        self
    }
}

impl<E> IntoResult<JlrsResult<()>, E> for Result<JlrsResult<()>, E> {
    fn into_result(self) -> Result<JlrsResult<()>, E> {
        self
    }
}

/// Functionality that is necessary to use an async runtime with jlrs.
///
/// If you want to use async-std or tokio, you can use one of the implementations provided by
/// jlrs. If you want to use another crate you can implement this trait.
#[async_trait(?Send)]
pub trait AsyncRuntime: Send + Sync + 'static {
    /// Error that is returned when a task can't be joined because it has panicked.
    type JoinError;

    /// The output type of a task spawned by `AsyncRuntime::spawn_local`.
    type TaskOutput: IntoResult<(), Self::JoinError>;

    /// The output type of the runtime task spawned by `AsyncRuntime::spawn_blocking`.
    type RuntimeOutput: IntoResult<JlrsResult<()>, Self::JoinError>;

    /// The handle type of a task spawned by `AsyncRuntime::spawn_local`.
    type JoinHandle: Future<Output = Self::TaskOutput>;

    /// The handle type of the runtime task spawned by `AsyncRuntime::spawn_local`.
    type RuntimeHandle: Future<Output = Self::RuntimeOutput>;

    /// Spawn a new thread, this method called if `AsyncBuilder::start` is called.
    fn spawn_thread<F>(rt_fn: F) -> std::thread::JoinHandle<JlrsResult<()>>
    where
        F: FnOnce() -> JlrsResult<()> + Send + Sync + 'static,
    {
        std::thread::spawn(rt_fn)
    }

    /// Spawn a blocking task, this method called if `AsyncBuilder::start_async` is called.
    fn spawn_blocking<F>(rt_fn: F) -> Self::RuntimeHandle
    where
        F: FnOnce() -> JlrsResult<()> + Send + Sync + 'static;

    /// Block on a future, this method is called from the runtime task or thread.
    fn block_on<F>(loop_fn: F) -> JlrsResult<()>
    where
        F: Future<Output = JlrsResult<()>>;

    /// Spawn a local task, this method is called from the loop task when an [`AsyncTask`] or
    /// [`PersistentTask`] starts.
    fn spawn_local<F>(future: F) -> Self::JoinHandle
    where
        F: Future<Output = ()> + 'static;

    /// Wait on `future` until it resolves or `duration` has elapsed. If the future times out it
    /// must return `None`.
    async fn timeout<F>(duration: Duration, future: F) -> Option<JlrsResult<Message>>
    where
        F: Future<Output = JlrsResult<Message>>;
}

/// A handle to the async runtime.
///
/// This handle can be used to include files and send new tasks to the runtime. The runtime shuts
/// down when the last handle is dropped.
pub struct AsyncJulia<R>
where
    R: AsyncRuntime,
{
    sender: Arc<dyn ChannelSender<Message>>,
    _runtime: PhantomData<R>,
}

impl<R> AsyncJulia<R>
where
    R: AsyncRuntime,
{
    /// Send a new async task to the runtime.
    ///
    /// This method waits if there's no room in the channel. It takes two arguments, the task and
    /// the sending half of a channel which is used to send the result back after the task has
    /// completed.
    pub async fn task<AT, RS>(&self, task: AT, res_sender: RS) -> JlrsResult<()>
    where
        AT: AsyncTask,
        RS: OneshotSender<JlrsResult<AT::Output>>,
    {
        let sender = self.sender.clone();
        let msg = PendingTask::<_, _, Task>::new(task, res_sender);
        let boxed = Box::new(msg);
        self.sender
            .send(MessageInner::Task(boxed, sender).wrap())
            .await
            .map_err(|_| JlrsError::ChannelClosed)?;

        Ok(())
    }

    /// Try to send a new async task to the runtime.
    ///
    /// If there's no room in the backing channel an error is returned immediately. This method
    /// takes two arguments, the task and the sending half of a channel which is used to send the
    /// result back after the task has completed.
    pub fn try_task<AT, RS>(&self, task: AT, res_sender: RS) -> JlrsResult<()>
    where
        AT: AsyncTask,
        RS: OneshotSender<JlrsResult<AT::Output>>,
    {
        let sender = self.sender.clone();
        let msg = PendingTask::<_, _, Task>::new(task, res_sender);
        let boxed = Box::new(msg);
        self.sender
            .try_send(MessageInner::Task(boxed, sender).wrap())
            .map_err(|e| match e {
                TrySendError::Full(_) => JlrsError::ChannelFull,
                TrySendError::Closed(_) => JlrsError::ChannelClosed,
            })?;

        Ok(())
    }

    /// Register an async task.
    ///
    /// This method waits if there's no room in the channel. It takes one argument, the sending
    /// half of a channel which is used to send the result back after the registration has
    /// completed.
    pub async fn register_task<AT, RS>(&self, res_sender: RS) -> JlrsResult<()>
    where
        AT: AsyncTask,
        RS: OneshotSender<JlrsResult<()>>,
    {
        let sender = self.sender.clone();
        let msg = PendingTask::<_, AT, RegisterTask>::new(res_sender);
        let boxed = Box::new(msg);
        self.sender
            .send(MessageInner::Task(boxed, sender).wrap())
            .await
            .map_err(|_| JlrsError::ChannelClosed)?;

        Ok(())
    }

    /// Try to register an async task.
    ///
    /// If there's no room in the channel an error is returned immediately. This method takes one
    /// argument, the sending half of a channel which is used to send the result back after the
    /// registration has completed.
    pub fn try_register_task<AT, RS>(&self, res_sender: RS) -> JlrsResult<()>
    where
        AT: AsyncTask,
        RS: OneshotSender<JlrsResult<()>>,
    {
        let sender = self.sender.clone();
        let msg = PendingTask::<_, AT, RegisterTask>::new(res_sender);
        let boxed = Box::new(msg);
        self.sender
            .try_send(MessageInner::Task(boxed, sender).wrap())
            .map_err(|e| match e {
                TrySendError::Full(_) => JlrsError::ChannelFull,
                TrySendError::Closed(_) => JlrsError::ChannelClosed,
            })?;

        Ok(())
    }

    /// Send a new blocking task to the runtime.
    ///
    /// This method waits if there's no room in the channel. It takes two arguments, the first is
    /// a closure that takes two arguments, a `Global` and mutable reference to a `GcFrame`, and
    /// must return a `JlrsResult` whose inner type is both `Send` and `Sync`. The second is the
    /// sending half of a channel which is used to send the result back after the task has
    /// completed. This task is executed as soon as possible and can't call async methods, so it
    /// blocks the runtime.
    pub async fn blocking_task<T, RS, F>(&self, task: F, res_sender: RS) -> JlrsResult<()>
    where
        for<'base> F: 'static
            + Send
            + Sync
            + FnOnce(Global<'base>, &mut GcFrame<'base, Async<'base>>) -> JlrsResult<T>,
        RS: OneshotSender<JlrsResult<T>>,
        T: Send + Sync + 'static,
    {
        let msg = BlockingTask::<_, _, R, _>::new(task, res_sender, 0);
        let boxed = Box::new(msg);
        self.sender
            .send(MessageInner::BlockingTask(boxed).wrap())
            .await
            .map_err(|_| JlrsError::ChannelClosed)?;

        Ok(())
    }

    /// Try to send a new blocking task to the runtime.
    ///
    /// If there's no room in the backing channel an error is returned immediately. This method
    /// takes two arguments, the first is a closure that takes two arguments, a `Global` and
    /// mutable reference to a `GcFrame`, and must return a `JlrsResult` whose inner type is both
    /// `Send` and `Sync`. The second is the sending half of a channel which is used to send the
    /// result back after the task has completed. This task is executed as soon as possible and
    /// can't call async methods, so it blocks the runtime.
    pub fn try_blocking_task<T, RS, F>(&self, task: F, res_sender: RS) -> JlrsResult<()>
    where
        for<'base> F: 'static
            + Send
            + Sync
            + FnOnce(Global<'base>, &mut GcFrame<'base, Async<'base>>) -> JlrsResult<T>,
        RS: OneshotSender<JlrsResult<T>>,
        T: Send + Sync + 'static,
    {
        let msg = BlockingTask::<_, _, R, _>::new(task, res_sender, 0);
        let boxed = Box::new(msg);
        self.sender
            .try_send(MessageInner::BlockingTask(boxed).wrap())
            .map_err(|e| match e {
                TrySendError::Full(_) => JlrsError::ChannelFull,
                TrySendError::Closed(_) => JlrsError::ChannelClosed,
            })?;

        Ok(())
    }

    /// Send a new blocking task to the runtime, the frame the task can use can root at least
    /// `capacity` values.
    ///
    /// This method is equivalent to `AsyncJulia::blocking_task` but takes an additional
    /// argument, the capacity of the task's frame.
    pub async fn blocking_task_with_capacity<T, RS, F>(
        &self,
        task: F,
        res_sender: RS,
        capacity: usize,
    ) -> JlrsResult<()>
    where
        for<'base> F: 'static
            + Send
            + Sync
            + FnOnce(Global<'base>, &mut GcFrame<'base, Async<'base>>) -> JlrsResult<T>,
        RS: OneshotSender<JlrsResult<T>>,
        T: Send + Sync + 'static,
    {
        let msg = BlockingTask::<_, _, R, _>::new(task, res_sender, capacity);
        let boxed = Box::new(msg);
        self.sender
            .send(MessageInner::BlockingTask(boxed).wrap())
            .await
            .map_err(|_| JlrsError::ChannelClosed)?;

        Ok(())
    }

    /// Try to send a new blocking task to the runtime, the frame the task can use can root at
    /// least `capacity` values.
    ///
    /// This method is equivalent to `AsyncJulia::try_blocking_task` but takes an additional
    /// argument, the capacity of the task's frame.
    pub fn try_blocking_task_with_capacity<T, RS, F>(
        &self,
        task: F,
        res_sender: RS,
        capacity: usize,
    ) -> JlrsResult<()>
    where
        for<'base> F: 'static
            + Send
            + Sync
            + FnOnce(Global<'base>, &mut GcFrame<'base, Async<'base>>) -> JlrsResult<T>,
        RS: OneshotSender<JlrsResult<T>>,
        T: Send + Sync + 'static,
    {
        let msg = BlockingTask::<_, _, R, _>::new(task, res_sender, capacity);
        let boxed = Box::new(msg);
        self.sender
            .try_send(MessageInner::BlockingTask(boxed).wrap())
            .map_err(|e| match e {
                TrySendError::Full(_) => JlrsError::ChannelFull,
                TrySendError::Closed(_) => JlrsError::ChannelClosed,
            })?;

        Ok(())
    }

    /// Send a new persistent task to the runtime.
    ///
    /// This method waits if there's no room in the channel. It takes a single argument, the task,
    /// you must also provide an implementation of [`Channel`] as a type parameter. This channel
    /// is used by the returned [`PersistentHandle`] to communicate with the persistent task.
    pub async fn persistent<C, PT>(&self, task: PT) -> JlrsResult<PersistentHandle<PT>>
    where
        C: Channel<PersistentMessage<PT>>,
        PT: PersistentTask,
    {
        let (sender, receiver) = C::channel(NonZeroUsize::new(PT::CHANNEL_CAPACITY));
        let rt_sender = self.sender.clone();
        let msg = PendingTask::<_, _, Persistent>::new(task, receiver);
        let boxed = Box::new(msg);

        self.sender
            .send(MessageInner::Task(boxed, rt_sender).wrap())
            .await
            .map_err(|_| JlrsError::ChannelClosed)?;

        Ok(PersistentHandle::new(Arc::new(sender)))
    }

    /// Try to send a new persistent task to the runtime.
    ///
    /// If there's no room in the backing channel an error is returned immediately. This method
    /// takes a single argument, the task, you must also provide an implementation of [`Channel`]
    /// as a type parameter. This channel is used by the returned [`PersistentHandle`] to
    /// communicate with the persistent task.
    pub fn try_persistent<C, PT>(&self, task: PT) -> JlrsResult<PersistentHandle<PT>>
    where
        C: Channel<PersistentMessage<PT>>,
        PT: PersistentTask,
    {
        let (sender, recv) = C::channel(NonZeroUsize::new(PT::CHANNEL_CAPACITY));

        let rt_sender = self.sender.clone();
        let msg = PendingTask::<_, _, Persistent>::new(task, recv);
        let boxed = Box::new(msg);
        self.sender
            .try_send(MessageInner::Task(boxed, rt_sender).wrap())
            .map_err(|e| match e {
                TrySendError::Full(_) => JlrsError::ChannelFull,
                TrySendError::Closed(_) => JlrsError::ChannelClosed,
            })?;

        Ok(PersistentHandle::new(Arc::new(sender)))
    }

    /// Register a persistent task.
    ///
    /// This method waits if there's no room in the channel. It takes one argument, the sending
    /// half of a channel which is used to send the result back after the registration has
    /// completed.
    pub async fn register_persistent<PT, RS>(&self, res_sender: RS) -> JlrsResult<()>
    where
        PT: PersistentTask,
        RS: OneshotSender<JlrsResult<()>>,
    {
        let sender = self.sender.clone();
        let msg = PendingTask::<_, PT, RegisterPersistent>::new(res_sender);
        let boxed = Box::new(msg);
        self.sender
            .send(MessageInner::Task(boxed, sender).wrap())
            .await
            .map_err(|_| JlrsError::ChannelClosed)?;

        Ok(())
    }

    /// Try to register a persistent task.
    ///
    /// If there's no room in the channel an error is returned immediately. This method takes one
    /// argument, the sending half of a channel which is used to send the result back after the
    /// registration has completed.
    pub fn try_register_persistent<PT, RS>(&self, res_sender: RS) -> JlrsResult<()>
    where
        PT: PersistentTask,
        RS: OneshotSender<JlrsResult<()>>,
    {
        let sender = self.sender.clone();
        let msg = PendingTask::<_, PT, RegisterPersistent>::new(res_sender);
        let boxed = Box::new(msg);
        self.sender
            .try_send(MessageInner::Task(boxed, sender).wrap())
            .map_err(|e| match e {
                TrySendError::Full(_) => JlrsError::ChannelFull,
                TrySendError::Closed(_) => JlrsError::ChannelClosed,
            })?;

        Ok(())
    }

    /// Include a Julia file by calling `Main.include` as a blocking task.
    ///
    /// This method waits if there's no room in the channel. It takes two arguments, the path to
    /// the file and the sending half of a channel which is used to send the result back after the
    /// file has been included.
    ///
    /// Safety: this method evaluates the contents of the file if it exists, which can't be
    /// checked for correctness.
    pub async unsafe fn include<P, RS>(&self, path: P, res_sender: RS) -> JlrsResult<()>
    where
        P: AsRef<Path>,
        RS: OneshotSender<JlrsResult<()>>,
    {
        if !path.as_ref().exists() {
            Err(JlrsError::IncludeNotFound {
                path: path.as_ref().to_string_lossy().into(),
            })?
        }

        self.sender
            .send(MessageInner::Include(path.as_ref().to_path_buf(), Box::new(res_sender)).wrap())
            .await
            .map_err(|_| JlrsError::ChannelClosed)?;

        Ok(())
    }

    /// Try to include a Julia file by calling `Main.include` as a blocking task.
    ///
    /// If there's no room in the channel an error is returned immediately. This method takes two
    /// arguments, the path to the file and the sending half of a channel which is used to send
    /// the result back after the file has been included.
    ///
    /// Safety: this method evaluates the contents of the file if it exists, which can't be
    /// checked for correctness.
    pub unsafe fn try_include<P, RS>(&self, path: P, res_sender: RS) -> JlrsResult<()>
    where
        P: AsRef<Path>,
        RS: OneshotSender<JlrsResult<()>>,
    {
        if !path.as_ref().exists() {
            Err(JlrsError::IncludeNotFound {
                path: path.as_ref().to_string_lossy().into(),
            })?
        }

        self.sender
            .try_send(
                MessageInner::Include(path.as_ref().to_path_buf(), Box::new(res_sender)).wrap(),
            )
            .map_err(|e| match e {
                TrySendError::Full(_) => JlrsError::ChannelFull,
                TrySendError::Closed(_) => JlrsError::ChannelClosed,
            })?;

        Ok(())
    }

    /// Enable or disable colored error messages originating from Julia as a blocking task.
    ///
    /// This method waits if there's no room in the channel. It takes two arguments, a `bool` to
    /// enable or disable colored error messages and the sending half of a channel which is used
    /// to send the result back after the option is set.
    ///
    /// This feature is disabled by default.
    pub async fn error_color<RS>(&self, enable: bool, res_sender: RS) -> JlrsResult<()>
    where
        RS: OneshotSender<JlrsResult<()>>,
    {
        self.sender
            .send(MessageInner::ErrorColor(enable, Box::new(res_sender)).wrap())
            .await
            .map_err(|_| JlrsError::ChannelClosed)?;

        Ok(())
    }

    /// Try to enable or disable colored error messages originating from Julia as a blocking task.
    ///
    /// If there's no room in the channel an error is returned immediately. This method takes two
    /// arguments, a `bool` to enable or disable colored error messages and the sending half of a
    /// channel which is used to send the result back after the option is set.
    ///
    /// This feature is disabled by default.
    pub fn try_error_color<RS>(&self, enable: bool, res_sender: RS) -> JlrsResult<()>
    where
        RS: OneshotSender<JlrsResult<()>>,
    {
        self.sender
            .try_send(MessageInner::ErrorColor(enable, Box::new(res_sender)).wrap())
            .map_err(|e| match e {
                TrySendError::Full(_) => JlrsError::ChannelFull,
                TrySendError::Closed(_) => JlrsError::ChannelClosed,
            })?;

        Ok(())
    }

    pub(crate) unsafe fn init<C>(
        builder: AsyncRuntimeBuilder<R, C>,
    ) -> JlrsResult<(Self, std::thread::JoinHandle<JlrsResult<()>>)>
    where
        C: Channel<Message>,
    {
        let (sender, receiver) = C::channel(NonZeroUsize::new(builder.channel_capacity));
        let handle = R::spawn_thread(move || Self::run_async(builder, Box::new(receiver)));

        let julia = AsyncJulia {
            sender: Arc::new(sender),
            _runtime: PhantomData,
        };

        Ok((julia, handle))
    }

    pub(crate) async unsafe fn init_async<C>(
        builder: AsyncRuntimeBuilder<R, C>,
    ) -> JlrsResult<(Self, R::RuntimeHandle)>
    where
        C: Channel<Message>,
    {
        let (sender, receiver) = C::channel(NonZeroUsize::new(builder.channel_capacity));
        let handle = R::spawn_blocking(move || Self::run_async(builder, Box::new(receiver)));

        let julia = AsyncJulia {
            sender: Arc::new(sender),
            _runtime: PhantomData,
        };

        Ok((julia, handle))
    }

    fn run_async<C>(
        builder: AsyncRuntimeBuilder<R, C>,
        receiver: Box<dyn ChannelReceiver<Message>>,
    ) -> JlrsResult<()>
    where
        C: Channel<Message>,
    {
        R::block_on(async {
            unsafe {
                if jl_is_initialized() != 0 || INIT.swap(true, Ordering::SeqCst) {
                    return Err(JlrsError::AlreadyInitialized.into());
                }

                if builder.n_threads == 0 {
                    let n = num_cpus::get();
                    jl_options.nthreads = n as _;
                } else {
                    jl_options.nthreads = builder.n_threads as _;
                }

                if let Some((ref julia_bindir, ref image_path)) = builder.builder.image {
                    let julia_bindir_str = julia_bindir.to_string_lossy().to_string();
                    let image_path_str = image_path.to_string_lossy().to_string();

                    if !julia_bindir.exists() {
                        let io_err = IOError::new(ErrorKind::NotFound, julia_bindir_str);
                        return Err(JlrsError::other(io_err))?;
                    }

                    if !image_path.exists() {
                        let io_err = IOError::new(ErrorKind::NotFound, image_path_str);
                        return Err(JlrsError::other(io_err))?;
                    }

                    let bindir = std::ffi::CString::new(julia_bindir_str).unwrap();
                    let im_rel_path = std::ffi::CString::new(image_path_str).unwrap();

                    jl_init_with_image(bindir.as_ptr(), im_rel_path.as_ptr());
                } else {
                    jl_init();
                }

                Self::run_inner(builder, receiver).await?;
            }

            Ok(())
        })
    }

    async unsafe fn run_inner<C>(
        builder: AsyncRuntimeBuilder<R, C>,
        mut receiver: Box<dyn ChannelReceiver<Message>>,
    ) -> Result<(), Box<JlrsError>>
    where
        C: Channel<Message>,
    {
        if Info::new().n_threads() < 3 {
            Err(JlrsError::MoreThreadsRequired)?;
        }

        let max_n_tasks = if builder.n_tasks == 0 {
            num_cpus::get() as _
        } else {
            builder.n_tasks
        };
        let recv_timeout = builder.recv_timeout;

        let mut free_stacks = VecDeque::with_capacity(max_n_tasks);
        for i in 1..max_n_tasks {
            free_stacks.push_back(i);
        }

        let mut stacks = {
            let mut stacks = Vec::with_capacity(max_n_tasks);
            for _ in 0..max_n_tasks {
                stacks.push(Some(AsyncStackPage::new()));
            }
            AsyncStackPage::link_stacks(&mut stacks);
            stacks.into_boxed_slice()
        };

        let mut running_tasks = Vec::with_capacity(max_n_tasks);
        for _ in 0..max_n_tasks {
            running_tasks.push(None);
        }

        let mut running_tasks = running_tasks.into_boxed_slice();
        let mut pending_tasks = VecDeque::new();
        let mut n_running = 0usize;

        {
            let stack = stacks[0].as_mut().expect("Async stack corrupted");
            set_custom_fns(stack)?;
        }

        loop {
            let wait_time = if n_running > 0 {
                recv_timeout
            } else {
                Duration::from_millis(u32::MAX as u64)
            };

            match R::timeout(wait_time, receiver.as_mut().recv()).await {
                None => {
                    jl_process_events();
                    jl_sys::jl_yield();
                }
                Some(Ok(msg)) => match msg.inner {
                    MessageInner::Task(task, sender) => {
                        if let Some(idx) = free_stacks.pop_front() {
                            let mut stack = stacks[idx].take().expect("Async stack corrupted");
                            let task = R::spawn_local(async move {
                                task.call(&mut stack).await;
                                sender
                                    .send(MessageInner::Complete(idx, stack).wrap())
                                    .await
                                    .ok();
                            });
                            n_running += 1;
                            running_tasks[idx] = Some(task);
                        } else {
                            pending_tasks.push_back((task, sender));
                        }
                    }
                    MessageInner::Complete(idx, mut stack) => {
                        if let Some((jl_task, sender)) = pending_tasks.pop_front() {
                            let task = R::spawn_local(async move {
                                jl_task.call(&mut stack).await;
                                sender
                                    .send(MessageInner::Complete(idx, stack).wrap())
                                    .await
                                    .ok();
                            });
                            running_tasks[idx] = Some(task);
                        } else {
                            stacks[idx] = Some(stack);
                            n_running -= 1;
                            free_stacks.push_front(idx);
                            running_tasks[idx] = None;
                        }
                    }
                    MessageInner::BlockingTask(task) => {
                        let stack = stacks[0].as_mut().expect("Async stack corrupted");
                        task.call(stack)
                    }
                    MessageInner::Include(path, sender) => {
                        let stack = stacks[0].as_mut().expect("Async stack corrupted");
                        let res = call_include(stack, path);
                        sender.send(res).await;
                    }
                    MessageInner::ErrorColor(enable, sender) => {
                        let res = call_error_color(enable);
                        sender.send(res).await;
                    }
                },
                Some(Err(_)) => break,
            }
        }

        for running in running_tasks.iter_mut() {
            if let Some(handle) = running.take() {
                handle.await.into_result().ok();
            }
        }

        jl_atexit_hook(0);
        Ok(())
    }
}

/// The message type used by the async runtime for communication.
pub struct Message {
    inner: MessageInner,
}

pub(crate) enum MessageInner {
    Task(Box<dyn GenericPendingTask>, Arc<dyn ChannelSender<Message>>),
    BlockingTask(Box<dyn GenericBlockingTask>),
    Include(PathBuf, Box<dyn OneshotSender<JlrsResult<()>>>),
    ErrorColor(bool, Box<dyn OneshotSender<JlrsResult<()>>>),
    Complete(usize, Pin<Box<AsyncStackPage>>),
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Message")
    }
}

impl MessageInner {
    pub(crate) fn wrap(self) -> Message {
        Message { inner: self }
    }
}

unsafe fn call_include(stack: &mut AsyncStackPage, path: PathBuf) -> JlrsResult<()> {
    let global = Global::new();
    let mode = Async(&stack.top[1]);
    let raw = stack.page.as_mut();
    let mut frame = GcFrame::new(raw, mode);

    match path.to_str() {
        Some(path) => {
            let path = JuliaString::new(&mut frame, path)?;
            Module::main(global)
                .function_ref("include")?
                .wrapper_unchecked()
                .call1_unrooted(global, path.as_value())
                .map_err(|e| JlrsError::Exception {
                    msg: format!("Include error: {:?}", e.value_unchecked()),
                })?;
        }
        None => {}
    }

    Ok(())
}

fn call_error_color(enable: bool) -> JlrsResult<()> {
    unsafe {
        let global = Global::new();

        let enable = if enable {
            Value::true_v(global)
        } else {
            Value::false_v(global)
        };

        Module::main(global)
            .submodule_ref("Jlrs")?
            .wrapper_unchecked()
            .global_ref("color")?
            .value_unchecked()
            .set_nth_field_unchecked(0, enable);

        Ok(())
    }
}

fn set_custom_fns(stack: &mut AsyncStackPage) -> JlrsResult<()> {
    unsafe {
        let global = Global::new();
        let mode = Async(&stack.top[1]);
        let raw = stack.page.as_mut();
        let mut frame = GcFrame::new(raw, mode);

        init_jlrs(&mut frame);
        init_multitask(&mut frame);

        let jlrs_mod = Module::main(global)
            .submodule_ref("JlrsMultitask")?
            .wrapper_unchecked();

        let wake_rust = Value::new(&mut frame, wake_task as *mut c_void)?;
        jlrs_mod
            .global_ref("wakerust")?
            .wrapper_unchecked()
            .set_nth_field_unchecked(0, wake_rust);

        #[cfg(feature = "pyplot")]
        crate::pyplot::init_jlrs_py_plot(&mut frame);

        Ok(())
    }
}

/// The message type used by persistent handles for communication with persistent tasks.
pub struct PersistentMessage<PT>
where
    PT: PersistentTask,
{
    pub(crate) msg: InnerPersistentMessage<PT>,
}

impl<PT> fmt::Debug for PersistentMessage<PT>
where
    PT: PersistentTask,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PersistentMessage")
    }
}

/// A handle to a [`PersistentTask`].
///
/// This handle can be used to call the task and shared across threads. The `PersistentTask` is
/// dropped when its final handle has been dropped and all remaining pending calls have completed.
#[derive(Clone)]
pub struct PersistentHandle<PT>
where
    PT: PersistentTask,
{
    sender: Arc<dyn ChannelSender<PersistentMessage<PT>>>,
}

impl<PT> PersistentHandle<PT>
where
    PT: PersistentTask,
{
    pub(crate) fn new(sender: Arc<dyn ChannelSender<PersistentMessage<PT>>>) -> Self {
        PersistentHandle { sender }
    }

    /// Call the persistent task with the provided input.
    ///
    /// This method waits until there's room available in the channel. In addition to the input
    /// data, it also takes the sending half of a channel which is used to send the result back
    /// after the call has completed.
    pub async fn call<R>(&self, input: PT::Input, sender: R) -> JlrsResult<()>
    where
        R: OneshotSender<JlrsResult<PT::Output>>,
    {
        self.sender
            .send(PersistentMessage {
                msg: Box::new(CallPersistentMessage {
                    input: Some(input),
                    sender,
                    _marker: PhantomData,
                }),
            })
            .await
            .map_err(|_| JlrsError::ChannelClosed)?;

        Ok(())
    }

    /// Try to call the persistent task with the provided input.
    ///
    /// If there's no room in the backing channel an error is returned immediately. In addition to
    /// the input data, it also takes the sending half of a channel which is used to send the
    /// result back after the call has completed.
    pub fn try_call<R>(&self, input: PT::Input, sender: R) -> JlrsResult<()>
    where
        R: OneshotSender<JlrsResult<PT::Output>>,
    {
        self.sender
            .try_send(PersistentMessage {
                msg: Box::new(CallPersistentMessage {
                    input: Some(input),
                    sender,
                    _marker: PhantomData,
                }),
            })
            .map_err(|e| match e {
                TrySendError::Full(_) => JlrsError::ChannelFull,
                TrySendError::Closed(_) => JlrsError::ChannelClosed,
            })?;

        Ok(())
    }
}

trait RequireSendSync: 'static + Send + Sync {}

// Ensure the handle can be shared across threads
impl<PT: PersistentTask> RequireSendSync for PersistentHandle<PT> {}
