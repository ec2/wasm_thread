pub use std::thread::{current, sleep, Result, Thread, ThreadId};
use std::{
    cell::UnsafeCell,
    fmt,
    marker::PhantomData,
    mem,
    panic::{catch_unwind, AssertUnwindSafe},
    rc::Rc,
    sync::{Arc, Mutex},
};

use scoped::ScopeData;
pub use scoped::{scope, Scope, ScopedJoinHandle};
use signal::Signal;
use utils::SpinLockMutex;
pub use utils::{available_parallelism, get_wasm_bindgen_shim_script_path, get_worker_script, is_web_worker_thread};
use wasm_bindgen::prelude::*;
use web_sys::{DedicatedWorkerGlobalScope, Worker, WorkerOptions, WorkerType};

mod scoped;
mod signal;
mod utils;

struct WebWorkerContext {
    func: Box<dyn FnOnce() + Send>,
}

/// Entry point for web workers
#[wasm_bindgen]
pub fn wasm_thread_entry_point(ptr: u32) {
    let ctx = unsafe { Box::from_raw(ptr as *mut WebWorkerContext) };
    (ctx.func)();
    WorkerMessage::ThreadComplete.post();
}

/// Used to relay spawn requests from web workers to main thread
struct BuilderRequest {
    builder: Builder,
    context: WebWorkerContext,
}

impl BuilderRequest {
    pub unsafe fn spawn(self) {
        self.builder.spawn_for_context(self.context);
    }
}

/// Web worker to main thread messages
enum WorkerMessage {
    /// Request to spawn thread
    SpawnThread(BuilderRequest),
    /// Thread has completed execution
    ThreadComplete,
}

impl WorkerMessage {
    pub fn post(self) {
        let req = Box::new(self);

        js_sys::eval("self")
            .unwrap()
            .dyn_into::<DedicatedWorkerGlobalScope>()
            .unwrap()
            .post_message(&JsValue::from(Box::into_raw(req) as u32))
            .unwrap();
    }
}

static DEFAULT_BUILDER: Mutex<Option<Builder>> = Mutex::new(None);

/// Thread factory, which can be used in order to configure the properties of a new thread.
#[derive(Debug, Clone)]
pub struct Builder {
    // A name for the thread-to-be, for identification in panic messages
    name: Option<String>,
    // A prefix for the thread-to-be, for identification in panic messages
    prefix: Option<String>,
    // The size of the stack for the spawned thread in bytes
    stack_size: Option<usize>,
    // Url of the `wasm_bindgen` generated shim `.js` script to use as web worker entry point
    wasm_bindgen_shim_url: Option<String>,
}

impl Default for Builder {
    fn default() -> Self {
        DEFAULT_BUILDER.lock_spin().unwrap().clone().unwrap_or(Self::empty())
    }
}

impl Builder {
    /// Creates a builder inheriting global configuration options set by [Self::set_default].
    pub fn new() -> Builder {
        Builder::default()
    }

    /// Creates a builder without inheriting global options set by [Self::set_default].
    pub fn empty() -> Builder {
        Self {
            name: None,
            prefix: None,
            stack_size: None,
            wasm_bindgen_shim_url: None,
        }
    }

    /// Sets current values as global default for all new builders created with [Builder::new] or [Builder::default].
    pub fn set_default(self) {
        *DEFAULT_BUILDER.lock_spin().unwrap() = Some(self);
    }

    /// Sets the prefix of the thread.
    pub fn prefix(mut self, prefix: String) -> Builder {
        self.prefix = Some(prefix);
        self
    }

    /// Sets the name of the thread.
    ///
    /// If not set, the default name is autogenerated.
    pub fn name(mut self, name: String) -> Builder {
        self.name = Some(name);
        self
    }

    /// Sets the size of the stack (in bytes) for the new thread.
    ///
    /// # Warning
    ///
    /// This is currently not supported by wasm, but provided for API consistency with [std::thread].
    pub fn stack_size(mut self, size: usize) -> Builder {
        self.stack_size = Some(size);
        self
    }

    /// Sets the URL of wasm_bindgen generated shim script.
    pub fn wasm_bindgen_shim_url(mut self, url: String) -> Builder {
        self.wasm_bindgen_shim_url = Some(url);
        self
    }

    /// Spawns a new thread by taking ownership of the `Builder`, and returns an
    /// [`io::Result`] to its [`JoinHandle`].
    pub fn spawn<F, T>(self, f: F) -> std::io::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T,
        F: Send + 'static,
        T: Send + 'static,
    {
        unsafe { self.spawn_unchecked(f) }
    }

    /// Spawns a new thread without any lifetime restrictions by taking ownership
    /// of the `Builder`, and returns an [`io::Result`] to its [`JoinHandle`].
    ///
    /// # Safety
    ///
    /// The caller has to ensure that no references in the supplied thread closure
    /// or its return type can outlive the spawned thread's lifetime. This can be
    /// guaranteed in two ways:
    ///
    /// - ensure that [`join`][`JoinHandle::join`] is called before any referenced
    /// data is dropped
    /// - use only types with `'static` lifetime bounds, i.e., those with no or only
    /// `'static` references (both [`Builder::spawn`]
    /// and [`spawn`] enforce this property statically)
    pub unsafe fn spawn_unchecked<'a, F, T>(self, f: F) -> std::io::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T,
        F: Send + 'a,
        T: Send + 'a,
    {
        Ok(JoinHandle(unsafe { self.spawn_unchecked_(f, None) }?))
    }

    pub(crate) unsafe fn spawn_unchecked_<'a, 'scope, F, T>(
        self,
        f: F,
        scope_data: Option<Arc<ScopeData>>,
    ) -> std::io::Result<JoinInner<'scope, T>>
    where
        F: FnOnce() -> T,
        F: Send + 'a,
        T: Send + 'a,
        'scope: 'a,
    {
        let my_signal = Arc::new(Signal::new());
        let their_signal = my_signal.clone();

        let my_packet: Arc<Packet<'scope, T>> = Arc::new(Packet {
            scope: scope_data,
            result: UnsafeCell::new(None),
            _marker: PhantomData,
        });
        let their_packet = my_packet.clone();

        // Pass `f` in `MaybeUninit` because actually that closure might *run longer than the lifetime of `F`*.
        // See <https://github.com/rust-lang/rust/issues/101983> for more details.
        // To prevent leaks we use a wrapper that drops its contents.
        #[repr(transparent)]
        struct MaybeDangling<T>(mem::MaybeUninit<T>);
        impl<T> MaybeDangling<T> {
            fn new(x: T) -> Self {
                MaybeDangling(mem::MaybeUninit::new(x))
            }
            fn into_inner(self) -> T {
                // SAFETY: we are always initiailized.
                let ret = unsafe { self.0.assume_init_read() };
                // Make sure we don't drop.
                mem::forget(self);
                ret
            }
        }
        impl<T> Drop for MaybeDangling<T> {
            fn drop(&mut self) {
                // SAFETY: we are always initiailized.
                unsafe { self.0.assume_init_drop() };
            }
        }

        let f = MaybeDangling::new(f);
        let main = Box::new(move || {
            // SAFETY: we constructed `f` initialized.
            let f = f.into_inner();
            // Execute the closure and catch any panics
            let try_result = catch_unwind(AssertUnwindSafe(|| f()));
            // SAFETY: `their_packet` as been built just above and moved by the
            // closure (it is an Arc<...>) and `my_packet` will be stored in the
            // same `JoinInner` as this closure meaning the mutation will be
            // safe (not modify it and affect a value far away).
            unsafe { *their_packet.result.get() = Some(try_result) };
            // Here `their_packet` gets dropped, and if this is the last `Arc` for that packet that
            // will call `decrement_num_running_threads` and therefore signal that this thread is
            // done.
            drop(their_packet);
            // Notify waiting handles
            their_signal.signal();
            // Here, the lifetime `'a` and even `'scope` can end. `main` keeps running for a bit
            // after that before returning itself.
        });

        // Erase lifetime
        let context = WebWorkerContext {
            func: mem::transmute::<Box<dyn FnOnce() + Send + 'a>, Box<dyn FnOnce() + Send + 'static>>(main),
        };

        if is_web_worker_thread() {
            WorkerMessage::SpawnThread(BuilderRequest { builder: self, context }).post();
        } else {
            self.spawn_for_context(context);
        }

        if let Some(scope) = &my_packet.scope {
            scope.increment_num_running_threads();
        }

        Ok(JoinInner {
            signal: my_signal,
            packet: my_packet,
        })
    }

    unsafe fn spawn_for_context(self, ctx: WebWorkerContext) {
        let Builder {
            name,
            prefix,
            wasm_bindgen_shim_url,
            ..
        } = self;

        // Get worker script as URL encoded blob
        let script = get_worker_script(wasm_bindgen_shim_url);

        // Todo: figure out how to set stack size
        let mut options = WorkerOptions::new();
        match (name, prefix) {
            (Some(name), Some(prefix)) => {
                options.name(&format!("{}:{}", prefix, name));
            }
            (Some(name), None) => {
                options.name(&name);
            }
            (None, Some(prefix)) => {
                let random = (js_sys::Math::random() * 10e10) as u64;
                options.name(&format!("{}:{}", prefix, random));
            }
            (None, None) => {}
        };

        #[cfg(feature = "es_modules")]
        {
            utils::load_module_workers_polyfill();
            options.type_(WorkerType::Module);
        }
        #[cfg(not(feature = "es_modules"))]
        {
            options.type_(WorkerType::Classic);
        }

        // Spawn the worker
        let worker = Rc::new(Worker::new_with_options(script.as_str(), &options).unwrap());

        // Make copy and keep a reference in callback handler so that GC does not despawn worker
        let mut their_worker = Some(worker.clone());

        let callback = Closure::wrap(Box::new(move |x: &web_sys::MessageEvent| {
            // All u32 bits map to f64 mantisa so it's safe to cast like that
            let req = Box::from_raw(x.data().as_f64().unwrap() as u32 as *mut WorkerMessage);

            match *req {
                WorkerMessage::SpawnThread(builder) => {
                    builder.spawn();
                }
                WorkerMessage::ThreadComplete => {
                    // Drop worker reference so it can be cleaned up by GC
                    their_worker.take();
                }
            };
        }) as Box<dyn FnMut(&web_sys::MessageEvent)>);
        worker.set_onmessage(Some(callback.as_ref().unchecked_ref()));

        // TODO: cleanup this leak somehow
        callback.forget();

        let ctx_ptr = Box::into_raw(Box::new(ctx));

        // Pack shared wasm (module and memory) and work as a single JS array
        let init = js_sys::Array::new();
        init.push(&wasm_bindgen::module());
        init.push(&wasm_bindgen::memory());
        init.push(&JsValue::from(ctx_ptr as u32));

        // Send initialization message
        match worker.post_message(&init) {
            Ok(()) => Ok(worker),
            Err(e) => {
                drop(Box::from_raw(ctx_ptr));
                Err(e)
            }
        }
        .unwrap();
    }
}

// This packet is used to communicate the return value between the spawned
// thread and the rest of the program. It is shared through an `Arc` and
// there's no need for a mutex here because synchronization happens with `join()`
// (the caller will never read this packet until the thread has exited).
//
// An Arc to the packet is stored into a `JoinInner` which in turns is placed
// in `JoinHandle`.
struct Packet<'scope, T> {
    scope: Option<Arc<ScopeData>>,
    result: UnsafeCell<Option<Result<T>>>,
    _marker: PhantomData<Option<&'scope ScopeData>>,
}

// Due to the usage of `UnsafeCell` we need to manually implement Sync.
// The type `T` should already always be Send (otherwise the thread could not
// have been created) and the Packet is Sync because all access to the
// `UnsafeCell` synchronized (by the `join()` boundary), and `ScopeData` is Sync.
unsafe impl<'scope, T: Send> Sync for Packet<'scope, T> {}

impl<'scope, T> Drop for Packet<'scope, T> {
    fn drop(&mut self) {
        // If this packet was for a thread that ran in a scope, the thread
        // panicked, and nobody consumed the panic payload, we make sure
        // the scope function will panic.
        let unhandled_panic = matches!(self.result.get_mut(), Some(Err(_)));
        // Drop the result without causing unwinding.
        // This is only relevant for threads that aren't join()ed, as
        // join() will take the `result` and set it to None, such that
        // there is nothing left to drop here.
        // If this panics, we should handle that, because we're outside the
        // outermost `catch_unwind` of our thread.
        // We just abort in that case, since there's nothing else we can do.
        // (And even if we tried to handle it somehow, we'd also need to handle
        // the case where the panic payload we get out of it also panics on
        // drop, and so on. See issue #86027.)
        if let Err(_) = catch_unwind(AssertUnwindSafe(|| {
            *self.result.get_mut() = None;
        })) {
            panic!("thread result panicked on drop");
        }
        // Book-keeping so the scope knows when it's done.
        if let Some(scope) = &self.scope {
            // Now that there will be no more user code running on this thread
            // that can use 'scope, mark the thread as 'finished'.
            // It's important we only do this after the `result` has been dropped,
            // since dropping it might still use things it borrowed from 'scope.
            scope.decrement_num_running_threads(unhandled_panic);
        }
    }
}

/// Inner representation for JoinHandle
pub(crate) struct JoinInner<'scope, T> {
    packet: Arc<Packet<'scope, T>>,
    signal: Arc<Signal>,
}

impl<'scope, T> JoinInner<'scope, T> {
    pub fn join(mut self) -> Result<T> {
        self.signal.wait();
        Arc::get_mut(&mut self.packet).unwrap().result.get_mut().take().unwrap()
    }

    pub async fn join_async(mut self) -> Result<T> {
        self.signal.wait_async().await;
        Arc::get_mut(&mut self.packet).unwrap().result.get_mut().take().unwrap()
    }
}

/// An owned permission to join on a thread (block on its termination).
pub struct JoinHandle<T>(JoinInner<'static, T>);

impl<T> JoinHandle<T> {
    /// Extracts a handle to the underlying thread.
    pub fn thread(&self) -> &Thread {
        unimplemented!();
        //&self.0.thread
    }

    /// Waits for the associated thread to finish.
    pub fn join(self) -> Result<T> {
        self.0.join()
    }

    /// Waits for the associated thread to finish asynchronously.
    pub async fn join_async(self) -> Result<T> {
        self.0.join_async().await
    }
}

impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("JoinHandle { .. }")
    }
}

/// Spawns a new thread, returning a JoinHandle for it.
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T,
    F: Send + 'static,
    T: Send + 'static,
{
    Builder::new().spawn(f).expect("failed to spawn thread")
}
