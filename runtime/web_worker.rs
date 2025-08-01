// Copyright 2018-2025 the Deno authors. MIT license.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;

use deno_broadcast_channel::InMemoryBroadcastChannel;
use deno_cache::CacheImpl;
use deno_cache::CreateCache;
use deno_cache::SqliteBackedCache;
use deno_core::CancelHandle;
use deno_core::CompiledWasmModuleStore;
use deno_core::DetachedBuffer;
use deno_core::Extension;
use deno_core::JsRuntime;
use deno_core::ModuleCodeString;
use deno_core::ModuleId;
use deno_core::ModuleLoader;
use deno_core::ModuleSpecifier;
use deno_core::PollEventLoopOptions;
use deno_core::RuntimeOptions;
use deno_core::SharedArrayBufferStore;
use deno_core::error::CoreError;
use deno_core::error::CoreErrorKind;
use deno_core::futures::channel::mpsc;
use deno_core::futures::future::poll_fn;
use deno_core::futures::stream::StreamExt;
use deno_core::futures::task::AtomicWaker;
use deno_core::located_script_name;
use deno_core::serde::Deserialize;
use deno_core::serde::Serialize;
use deno_core::serde_json::json;
use deno_core::v8;
use deno_cron::local::LocalCronHandler;
use deno_error::JsErrorClass;
use deno_fs::FileSystem;
use deno_io::Stdio;
use deno_kv::dynamic::MultiBackendDbHandler;
use deno_napi::DenoRtNativeAddonLoaderRc;
use deno_node::ExtNodeSys;
use deno_node::NodeExtInitServices;
use deno_permissions::PermissionsContainer;
use deno_process::NpmProcessStateProviderRc;
use deno_terminal::colors;
use deno_tls::RootCertStoreProvider;
use deno_tls::TlsKeys;
use deno_web::BlobStore;
use deno_web::JsMessageData;
use deno_web::MessagePort;
use deno_web::Transferable;
use deno_web::create_entangled_message_port;
use deno_web::serialize_transferables;
use log::debug;
use node_resolver::InNpmPackageChecker;
use node_resolver::NpmPackageFolderResolver;

use crate::BootstrapOptions;
use crate::FeatureChecker;
use crate::inspector_server::InspectorServer;
use crate::ops;
use crate::shared::runtime;
use crate::worker::FormatJsErrorFn;
#[cfg(target_os = "linux")]
use crate::worker::MEMORY_TRIM_HANDLER_ENABLED;
#[cfg(target_os = "linux")]
use crate::worker::SIGUSR2_RX;
use crate::worker::create_op_metrics;
use crate::worker::create_validate_import_attributes_callback;

pub struct WorkerMetadata {
  pub buffer: DetachedBuffer,
  pub transferables: Vec<Transferable>,
}

static WORKER_ID_COUNTER: AtomicU32 = AtomicU32::new(1);

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkerId(u32);
impl WorkerId {
  pub fn new() -> WorkerId {
    let id = WORKER_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    WorkerId(id)
  }
}
impl fmt::Display for WorkerId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "worker-{}", self.0)
  }
}
impl Default for WorkerId {
  fn default() -> Self {
    Self::new()
  }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerThreadType {
  // Used only for testing
  Classic,
  // Regular Web Worker
  Module,
  // `node:worker_threads` worker, technically
  // not a web worker, will be cleaned up in the future.
  Node,
}

impl<'s> WorkerThreadType {
  pub fn to_v8(
    &self,
    scope: &mut v8::HandleScope<'s>,
  ) -> v8::Local<'s, v8::String> {
    v8::String::new(
      scope,
      match self {
        WorkerThreadType::Classic => "classic",
        WorkerThreadType::Module => "module",
        WorkerThreadType::Node => "node",
      },
    )
    .unwrap()
  }
}
/// Events that are sent to host from child
/// worker.
#[allow(clippy::large_enum_variant)]
pub enum WorkerControlEvent {
  TerminalError(CoreError),
  Close,
}

use deno_core::serde::Serializer;

impl Serialize for WorkerControlEvent {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    let type_id = match &self {
      WorkerControlEvent::TerminalError(_) => 1_i32,
      WorkerControlEvent::Close => 3_i32,
    };

    match self {
      WorkerControlEvent::TerminalError(error) => {
        let value = match error.as_kind() {
          CoreErrorKind::Js(js_error) => {
            let frame = js_error.frames.iter().find(|f| match &f.file_name {
              Some(s) => !s.trim_start_matches('[').starts_with("ext:"),
              None => false,
            });
            json!({
              "message": js_error.exception_message,
              "fileName": frame.map(|f| f.file_name.as_ref()),
              "lineNumber": frame.map(|f| f.line_number.as_ref()),
              "columnNumber": frame.map(|f| f.column_number.as_ref()),
            })
          }
          _ => json!({
            "message": error.to_string(),
          }),
        };

        Serialize::serialize(&(type_id, value), serializer)
      }
      _ => Serialize::serialize(&(type_id, ()), serializer),
    }
  }
}

// Channels used for communication with worker's parent
#[derive(Clone)]
pub struct WebWorkerInternalHandle {
  sender: mpsc::Sender<WorkerControlEvent>,
  pub port: Rc<MessagePort>,
  pub cancel: Rc<CancelHandle>,
  termination_signal: Arc<AtomicBool>,
  has_terminated: Arc<AtomicBool>,
  terminate_waker: Arc<AtomicWaker>,
  isolate_handle: v8::IsolateHandle,
  pub name: String,
  pub worker_type: WorkerThreadType,
}

impl WebWorkerInternalHandle {
  /// Post WorkerEvent to parent as a worker
  #[allow(clippy::result_large_err)]
  pub fn post_event(
    &self,
    event: WorkerControlEvent,
  ) -> Result<(), mpsc::TrySendError<WorkerControlEvent>> {
    let mut sender = self.sender.clone();
    // If the channel is closed,
    // the worker must have terminated but the termination message has not yet been received.
    //
    // Therefore just treat it as if the worker has terminated and return.
    if sender.is_closed() {
      self.has_terminated.store(true, Ordering::SeqCst);
      return Ok(());
    }
    sender.try_send(event)
  }

  /// Check if this worker is terminated or being terminated
  pub fn is_terminated(&self) -> bool {
    self.has_terminated.load(Ordering::SeqCst)
  }

  /// Check if this worker must terminate (because the termination signal is
  /// set), and terminates it if so. Returns whether the worker is terminated or
  /// being terminated, as with [`Self::is_terminated()`].
  pub fn terminate_if_needed(&mut self) -> bool {
    let has_terminated = self.is_terminated();

    if !has_terminated && self.termination_signal.load(Ordering::SeqCst) {
      self.terminate();
      return true;
    }

    has_terminated
  }

  /// Terminate the worker
  /// This function will set terminated to true, terminate the isolate and close the message channel
  pub fn terminate(&mut self) {
    self.cancel.cancel();
    self.terminate_waker.wake();

    // This function can be called multiple times by whomever holds
    // the handle. However only a single "termination" should occur so
    // we need a guard here.
    let already_terminated = self.has_terminated.swap(true, Ordering::SeqCst);

    if !already_terminated {
      // Stop javascript execution
      self.isolate_handle.terminate_execution();
    }

    // Wake parent by closing the channel
    self.sender.close_channel();
  }
}

pub struct SendableWebWorkerHandle {
  port: MessagePort,
  receiver: mpsc::Receiver<WorkerControlEvent>,
  termination_signal: Arc<AtomicBool>,
  has_terminated: Arc<AtomicBool>,
  terminate_waker: Arc<AtomicWaker>,
  isolate_handle: v8::IsolateHandle,
}

impl From<SendableWebWorkerHandle> for WebWorkerHandle {
  fn from(handle: SendableWebWorkerHandle) -> Self {
    WebWorkerHandle {
      receiver: Rc::new(RefCell::new(handle.receiver)),
      port: Rc::new(handle.port),
      termination_signal: handle.termination_signal,
      has_terminated: handle.has_terminated,
      terminate_waker: handle.terminate_waker,
      isolate_handle: handle.isolate_handle,
    }
  }
}

/// This is the handle to the web worker that the parent thread uses to
/// communicate with the worker. It is created from a `SendableWebWorkerHandle`
/// which is sent to the parent thread from the worker thread where it is
/// created. The reason for this separation is that the handle first needs to be
/// `Send` when transferring between threads, and then must be `Clone` when it
/// has arrived on the parent thread. It can not be both at once without large
/// amounts of Arc<Mutex> and other fun stuff.
#[derive(Clone)]
pub struct WebWorkerHandle {
  pub port: Rc<MessagePort>,
  receiver: Rc<RefCell<mpsc::Receiver<WorkerControlEvent>>>,
  termination_signal: Arc<AtomicBool>,
  has_terminated: Arc<AtomicBool>,
  terminate_waker: Arc<AtomicWaker>,
  isolate_handle: v8::IsolateHandle,
}

impl WebWorkerHandle {
  /// Get the WorkerEvent with lock
  /// Return error if more than one listener tries to get event
  #[allow(clippy::await_holding_refcell_ref)] // TODO(ry) remove!
  pub async fn get_control_event(&self) -> Option<WorkerControlEvent> {
    let mut receiver = self.receiver.borrow_mut();
    receiver.next().await
  }

  /// Terminate the worker
  /// This function will set the termination signal, close the message channel,
  /// and schedule to terminate the isolate after two seconds.
  pub fn terminate(self) {
    use std::thread::sleep;
    use std::thread::spawn;
    use std::time::Duration;

    let schedule_termination =
      !self.termination_signal.swap(true, Ordering::SeqCst);

    self.port.disentangle();

    if schedule_termination && !self.has_terminated.load(Ordering::SeqCst) {
      // Wake up the worker's event loop so it can terminate.
      self.terminate_waker.wake();

      let has_terminated = self.has_terminated.clone();

      // Schedule to terminate the isolate's execution.
      spawn(move || {
        sleep(Duration::from_secs(2));

        // A worker's isolate can only be terminated once, so we need a guard
        // here.
        let already_terminated = has_terminated.swap(true, Ordering::SeqCst);

        if !already_terminated {
          // Stop javascript execution
          self.isolate_handle.terminate_execution();
        }
      });
    }
  }
}

fn create_handles(
  isolate_handle: v8::IsolateHandle,
  name: String,
  worker_type: WorkerThreadType,
) -> (WebWorkerInternalHandle, SendableWebWorkerHandle) {
  let (parent_port, worker_port) = create_entangled_message_port();
  let (ctrl_tx, ctrl_rx) = mpsc::channel::<WorkerControlEvent>(1);
  let termination_signal = Arc::new(AtomicBool::new(false));
  let has_terminated = Arc::new(AtomicBool::new(false));
  let terminate_waker = Arc::new(AtomicWaker::new());
  let internal_handle = WebWorkerInternalHandle {
    name,
    port: Rc::new(parent_port),
    termination_signal: termination_signal.clone(),
    has_terminated: has_terminated.clone(),
    terminate_waker: terminate_waker.clone(),
    isolate_handle: isolate_handle.clone(),
    cancel: CancelHandle::new_rc(),
    sender: ctrl_tx,
    worker_type,
  };
  let external_handle = SendableWebWorkerHandle {
    receiver: ctrl_rx,
    port: worker_port,
    termination_signal,
    has_terminated,
    terminate_waker,
    isolate_handle,
  };
  (internal_handle, external_handle)
}

pub struct WebWorkerServiceOptions<
  TInNpmPackageChecker: InNpmPackageChecker + 'static,
  TNpmPackageFolderResolver: NpmPackageFolderResolver + 'static,
  TExtNodeSys: ExtNodeSys + 'static,
> {
  pub blob_store: Arc<BlobStore>,
  pub broadcast_channel: InMemoryBroadcastChannel,
  pub deno_rt_native_addon_loader: Option<DenoRtNativeAddonLoaderRc>,
  pub compiled_wasm_module_store: Option<CompiledWasmModuleStore>,
  pub feature_checker: Arc<FeatureChecker>,
  pub fs: Arc<dyn FileSystem>,
  pub maybe_inspector_server: Option<Arc<InspectorServer>>,
  pub module_loader: Rc<dyn ModuleLoader>,
  pub node_services: Option<
    NodeExtInitServices<
      TInNpmPackageChecker,
      TNpmPackageFolderResolver,
      TExtNodeSys,
    >,
  >,
  pub npm_process_state_provider: Option<NpmProcessStateProviderRc>,
  pub permissions: PermissionsContainer,
  pub root_cert_store_provider: Option<Arc<dyn RootCertStoreProvider>>,
  pub shared_array_buffer_store: Option<SharedArrayBufferStore>,
}

pub struct WebWorkerOptions {
  pub name: String,
  pub main_module: ModuleSpecifier,
  pub worker_id: WorkerId,
  pub bootstrap: BootstrapOptions,
  pub extensions: Vec<Extension>,
  pub startup_snapshot: Option<&'static [u8]>,
  pub unsafely_ignore_certificate_errors: Option<Vec<String>>,
  /// Optional isolate creation parameters, such as heap limits.
  pub create_params: Option<v8::CreateParams>,
  pub seed: Option<u64>,
  pub create_web_worker_cb: Arc<ops::worker_host::CreateWebWorkerCb>,
  pub format_js_error_fn: Option<Arc<FormatJsErrorFn>>,
  pub worker_type: WorkerThreadType,
  pub cache_storage_dir: Option<std::path::PathBuf>,
  pub stdio: Stdio,
  pub strace_ops: Option<Vec<String>>,
  pub close_on_idle: bool,
  pub maybe_worker_metadata: Option<WorkerMetadata>,
  pub enable_raw_imports: bool,
  pub enable_stack_trace_arg_in_ops: bool,
}

/// This struct is an implementation of `Worker` Web API
///
/// Each `WebWorker` is either a child of `MainWorker` or other
/// `WebWorker`.
pub struct WebWorker {
  id: WorkerId,
  pub js_runtime: JsRuntime,
  pub name: String,
  close_on_idle: bool,
  internal_handle: WebWorkerInternalHandle,
  pub worker_type: WorkerThreadType,
  pub main_module: ModuleSpecifier,
  poll_for_messages_fn: Option<v8::Global<v8::Value>>,
  has_message_event_listener_fn: Option<v8::Global<v8::Value>>,
  bootstrap_fn_global: Option<v8::Global<v8::Function>>,
  // Consumed when `bootstrap_fn` is called
  maybe_worker_metadata: Option<WorkerMetadata>,
  memory_trim_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for WebWorker {
  fn drop(&mut self) {
    // clean up the package.json thread local cache
    node_resolver::PackageJsonThreadLocalCache::clear();

    if let Some(memory_trim_handle) = self.memory_trim_handle.take() {
      memory_trim_handle.abort();
    }
  }
}

impl WebWorker {
  pub fn bootstrap_from_options<
    TInNpmPackageChecker: InNpmPackageChecker + 'static,
    TNpmPackageFolderResolver: NpmPackageFolderResolver + 'static,
    TExtNodeSys: ExtNodeSys + 'static,
  >(
    services: WebWorkerServiceOptions<
      TInNpmPackageChecker,
      TNpmPackageFolderResolver,
      TExtNodeSys,
    >,
    options: WebWorkerOptions,
  ) -> (Self, SendableWebWorkerHandle) {
    let (mut worker, handle, bootstrap_options) =
      Self::from_options(services, options);
    worker.bootstrap(&bootstrap_options);
    (worker, handle)
  }

  fn from_options<
    TInNpmPackageChecker: InNpmPackageChecker + 'static,
    TNpmPackageFolderResolver: NpmPackageFolderResolver + 'static,
    TExtNodeSys: ExtNodeSys + 'static,
  >(
    services: WebWorkerServiceOptions<
      TInNpmPackageChecker,
      TNpmPackageFolderResolver,
      TExtNodeSys,
    >,
    mut options: WebWorkerOptions,
  ) -> (Self, SendableWebWorkerHandle, BootstrapOptions) {
    // Permissions: many ops depend on this
    let enable_testing_features = options.bootstrap.enable_testing_features;

    fn create_cache_inner(options: &WebWorkerOptions) -> Option<CreateCache> {
      if let Ok(var) = std::env::var("DENO_CACHE_LSC_ENDPOINT") {
        let elems: Vec<_> = var.split(",").collect();
        if elems.len() == 2 {
          let endpoint = elems[0];
          let token = elems[1];
          use deno_cache::CacheShard;

          let shard =
            Rc::new(CacheShard::new(endpoint.to_string(), token.to_string()));
          let create_cache_fn = move || {
            let x = deno_cache::LscBackend::default();
            x.set_shard(shard.clone());

            Ok(CacheImpl::Lsc(x))
          };
          #[allow(clippy::arc_with_non_send_sync)]
          return Some(CreateCache(Arc::new(create_cache_fn)));
        }
      }

      if let Some(storage_dir) = &options.cache_storage_dir {
        let storage_dir = storage_dir.clone();
        let create_cache_fn = move || {
          let s = SqliteBackedCache::new(storage_dir.clone())?;
          Ok(CacheImpl::Sqlite(s))
        };
        return Some(CreateCache(Arc::new(create_cache_fn)));
      }

      None
    }
    let create_cache = create_cache_inner(&options);

    // NOTE(bartlomieju): ordering is important here, keep it in sync with
    // `runtime/worker.rs`, `runtime/web_worker.rs`, `runtime/snapshot_info.rs`
    // and `runtime/snapshot.rs`!
    let mut extensions = vec![
      deno_telemetry::deno_telemetry::init(),
      // Web APIs
      deno_webidl::deno_webidl::init(),
      deno_console::deno_console::init(),
      deno_url::deno_url::init(),
      deno_web::deno_web::init::<PermissionsContainer>(
        services.blob_store,
        Some(options.main_module.clone()),
      ),
      deno_webgpu::deno_webgpu::init(),
      deno_canvas::deno_canvas::init(),
      deno_fetch::deno_fetch::init::<PermissionsContainer>(
        deno_fetch::Options {
          user_agent: options.bootstrap.user_agent.clone(),
          root_cert_store_provider: services.root_cert_store_provider.clone(),
          unsafely_ignore_certificate_errors: options
            .unsafely_ignore_certificate_errors
            .clone(),
          file_fetch_handler: Rc::new(deno_fetch::FsFetchHandler),
          ..Default::default()
        },
      ),
      deno_cache::deno_cache::init(create_cache),
      deno_websocket::deno_websocket::init::<PermissionsContainer>(
        options.bootstrap.user_agent.clone(),
        services.root_cert_store_provider.clone(),
        options.unsafely_ignore_certificate_errors.clone(),
      ),
      deno_webstorage::deno_webstorage::init(None).disable(),
      deno_crypto::deno_crypto::init(options.seed),
      deno_broadcast_channel::deno_broadcast_channel::init(
        services.broadcast_channel,
      ),
      deno_ffi::deno_ffi::init::<PermissionsContainer>(
        services.deno_rt_native_addon_loader.clone(),
      ),
      deno_net::deno_net::init::<PermissionsContainer>(
        services.root_cert_store_provider.clone(),
        options.unsafely_ignore_certificate_errors.clone(),
      ),
      deno_tls::deno_tls::init(),
      deno_kv::deno_kv::init(
        MultiBackendDbHandler::remote_or_sqlite::<PermissionsContainer>(
          None,
          options.seed,
          deno_kv::remote::HttpOptions {
            user_agent: options.bootstrap.user_agent.clone(),
            root_cert_store_provider: services.root_cert_store_provider,
            unsafely_ignore_certificate_errors: options
              .unsafely_ignore_certificate_errors
              .clone(),
            client_cert_chain_and_key: TlsKeys::Null,
            proxy: None,
          },
        ),
        deno_kv::KvConfig::builder().build(),
      ),
      deno_cron::deno_cron::init(LocalCronHandler::new()),
      deno_napi::deno_napi::init::<PermissionsContainer>(
        services.deno_rt_native_addon_loader.clone(),
      ),
      deno_http::deno_http::init(deno_http::Options {
        no_legacy_abort: options.bootstrap.no_legacy_abort,
        ..Default::default()
      }),
      deno_io::deno_io::init(Some(options.stdio)),
      deno_fs::deno_fs::init::<PermissionsContainer>(services.fs.clone()),
      deno_os::deno_os::init(None),
      deno_process::deno_process::init(services.npm_process_state_provider),
      deno_node::deno_node::init::<
        PermissionsContainer,
        TInNpmPackageChecker,
        TNpmPackageFolderResolver,
        TExtNodeSys,
      >(services.node_services, services.fs),
      // Runtime ops that are always initialized for WebWorkers
      ops::runtime::deno_runtime::init(options.main_module.clone()),
      ops::worker_host::deno_worker_host::init(
        options.create_web_worker_cb,
        options.format_js_error_fn,
      ),
      ops::fs_events::deno_fs_events::init(),
      ops::permissions::deno_permissions::init(),
      ops::tty::deno_tty::init(),
      ops::http::deno_http_runtime::init(),
      ops::bootstrap::deno_bootstrap::init(
        options.startup_snapshot.and_then(|_| Default::default()),
        false,
      ),
      runtime::init(),
      ops::web_worker::deno_web_worker::init(),
    ];

    #[cfg(feature = "hmr")]
    assert!(
      cfg!(not(feature = "only_snapshotted_js_sources")),
      "'hmr' is incompatible with 'only_snapshotted_js_sources'."
    );

    for extension in &mut extensions {
      if options.startup_snapshot.is_some() {
        extension.js_files = std::borrow::Cow::Borrowed(&[]);
        extension.esm_files = std::borrow::Cow::Borrowed(&[]);
        extension.esm_entry_point = None;
      }
    }

    extensions.extend(std::mem::take(&mut options.extensions));

    #[cfg(feature = "only_snapshotted_js_sources")]
    options.startup_snapshot.as_ref().expect("A user snapshot was not provided, even though 'only_snapshotted_js_sources' is used.");

    // Get our op metrics
    let (op_summary_metrics, op_metrics_factory_fn) = create_op_metrics(
      options.bootstrap.enable_op_summary_metrics,
      options.strace_ops,
    );

    let mut js_runtime = JsRuntime::new(RuntimeOptions {
      module_loader: Some(services.module_loader),
      startup_snapshot: options.startup_snapshot,
      create_params: options.create_params,
      shared_array_buffer_store: services.shared_array_buffer_store,
      compiled_wasm_module_store: services.compiled_wasm_module_store,
      extensions,
      #[cfg(feature = "transpile")]
      extension_transpiler: Some(Rc::new(|specifier, source| {
        crate::transpile::maybe_transpile_source(specifier, source)
      })),
      #[cfg(not(feature = "transpile"))]
      extension_transpiler: None,
      inspector: true,
      op_metrics_factory_fn,
      validate_import_attributes_cb: Some(
        create_validate_import_attributes_callback(options.enable_raw_imports),
      ),
      import_assertions_support: deno_core::ImportAssertionsSupport::Error,
      maybe_op_stack_trace_callback: options
        .enable_stack_trace_arg_in_ops
        .then(crate::worker::create_permissions_stack_trace_callback),
      extension_code_cache: None,
      skip_op_registration: false,
      v8_platform: None,
      is_main: false,
      wait_for_inspector_disconnect_callback: None,
      custom_module_evaluation_cb: None,
      eval_context_code_cache_cbs: None,
    });

    if let Some(op_summary_metrics) = op_summary_metrics {
      js_runtime.op_state().borrow_mut().put(op_summary_metrics);
    }

    {
      let state = js_runtime.op_state();
      let mut state = state.borrow_mut();

      state.put::<PermissionsContainer>(services.permissions);
      state.put(ops::TestingFeaturesEnabled(enable_testing_features));
      state.put(services.feature_checker);

      // Put inspector handle into the op state so we can put a breakpoint when
      // executing a CJS entrypoint.
      state.put(js_runtime.inspector());
    }

    if let Some(server) = services.maybe_inspector_server {
      server.register_inspector(
        options.main_module.to_string(),
        &mut js_runtime,
        false,
      );
    }

    let (internal_handle, external_handle) = {
      let handle = js_runtime.v8_isolate().thread_safe_handle();
      let (internal_handle, external_handle) =
        create_handles(handle, options.name.clone(), options.worker_type);
      let op_state = js_runtime.op_state();
      let mut op_state = op_state.borrow_mut();
      op_state.put(internal_handle.clone());
      (internal_handle, external_handle)
    };

    let bootstrap_fn_global = {
      let context = js_runtime.main_context();
      let scope = &mut js_runtime.handle_scope();
      let context_local = v8::Local::new(scope, context);
      let global_obj = context_local.global(scope);
      let bootstrap_str =
        v8::String::new_external_onebyte_static(scope, b"bootstrap").unwrap();
      let bootstrap_ns: v8::Local<v8::Object> = global_obj
        .get(scope, bootstrap_str.into())
        .unwrap()
        .try_into()
        .unwrap();
      let main_runtime_str =
        v8::String::new_external_onebyte_static(scope, b"workerRuntime")
          .unwrap();
      let bootstrap_fn =
        bootstrap_ns.get(scope, main_runtime_str.into()).unwrap();
      let bootstrap_fn =
        v8::Local::<v8::Function>::try_from(bootstrap_fn).unwrap();
      v8::Global::new(scope, bootstrap_fn)
    };

    (
      Self {
        id: options.worker_id,
        js_runtime,
        name: options.name,
        internal_handle,
        worker_type: options.worker_type,
        main_module: options.main_module,
        poll_for_messages_fn: None,
        has_message_event_listener_fn: None,
        bootstrap_fn_global: Some(bootstrap_fn_global),
        close_on_idle: options.close_on_idle,
        maybe_worker_metadata: options.maybe_worker_metadata,
        memory_trim_handle: None,
      },
      external_handle,
      options.bootstrap,
    )
  }

  pub fn bootstrap(&mut self, options: &BootstrapOptions) {
    let op_state = self.js_runtime.op_state();
    op_state.borrow_mut().put(options.clone());
    // Instead of using name for log we use `worker-${id}` because
    // WebWorkers can have empty string as name.
    {
      let scope = &mut self.js_runtime.handle_scope();
      let args = options.as_v8(scope);
      let bootstrap_fn = self.bootstrap_fn_global.take().unwrap();
      let bootstrap_fn = v8::Local::new(scope, bootstrap_fn);
      let undefined = v8::undefined(scope);
      let mut worker_data: v8::Local<v8::Value> = v8::undefined(scope).into();
      if let Some(data) = self.maybe_worker_metadata.take() {
        let js_transferables = serialize_transferables(
          &mut op_state.borrow_mut(),
          data.transferables,
        );
        let js_message_data = JsMessageData {
          data: data.buffer,
          transferables: js_transferables,
        };
        worker_data =
          deno_core::serde_v8::to_v8(scope, js_message_data).unwrap();
      }
      let name_str: v8::Local<v8::Value> =
        v8::String::new(scope, &self.name).unwrap().into();
      let id_str: v8::Local<v8::Value> =
        v8::String::new(scope, &format!("{}", self.id))
          .unwrap()
          .into();
      let id: v8::Local<v8::Value> =
        v8::Integer::new(scope, self.id.0 as i32).into();
      let worker_type: v8::Local<v8::Value> =
        self.worker_type.to_v8(scope).into();
      bootstrap_fn
        .call(
          scope,
          undefined.into(),
          &[args, name_str, id_str, id, worker_type, worker_data],
        )
        .unwrap();

      let context = scope.get_current_context();
      let global = context.global(scope);
      let poll_for_messages_str =
        v8::String::new_external_onebyte_static(scope, b"pollForMessages")
          .unwrap();
      let poll_for_messages_fn = global
        .get(scope, poll_for_messages_str.into())
        .expect("get globalThis.pollForMessages");
      global.delete(scope, poll_for_messages_str.into());
      self.poll_for_messages_fn =
        Some(v8::Global::new(scope, poll_for_messages_fn));

      let has_message_event_listener_str =
        v8::String::new_external_onebyte_static(
          scope,
          b"hasMessageEventListener",
        )
        .unwrap();
      let has_message_event_listener_fn = global
        .get(scope, has_message_event_listener_str.into())
        .expect("get globalThis.hasMessageEventListener");
      global.delete(scope, has_message_event_listener_str.into());
      self.has_message_event_listener_fn =
        Some(v8::Global::new(scope, has_message_event_listener_fn));
    }
  }

  #[cfg(not(target_os = "linux"))]
  pub fn setup_memory_trim_handler(&mut self) {
    // Noop
  }

  /// Sets up a handler that responds to SIGUSR2 signals by trimming unused
  /// memory and notifying V8 of low memory conditions.
  /// Note that this must be called within a tokio runtime.
  /// Calling this method multiple times will be a no-op.
  #[cfg(target_os = "linux")]
  pub fn setup_memory_trim_handler(&mut self) {
    if self.memory_trim_handle.is_some() {
      return;
    }

    if !*MEMORY_TRIM_HANDLER_ENABLED {
      return;
    }

    let mut sigusr2_rx = SIGUSR2_RX.clone();

    let spawner = self
      .js_runtime
      .op_state()
      .borrow()
      .borrow::<deno_core::V8CrossThreadTaskSpawner>()
      .clone();

    let memory_trim_handle = tokio::spawn(async move {
      loop {
        if sigusr2_rx.changed().await.is_err() {
          break;
        }

        spawner.spawn(move |isolate| {
          isolate.low_memory_notification();
        });
      }
    });

    self.memory_trim_handle = Some(memory_trim_handle);
  }

  /// See [JsRuntime::execute_script](deno_core::JsRuntime::execute_script)
  #[allow(clippy::result_large_err)]
  pub fn execute_script(
    &mut self,
    name: &'static str,
    source_code: ModuleCodeString,
  ) -> Result<(), CoreError> {
    self.js_runtime.execute_script(name, source_code)?;
    Ok(())
  }

  /// Loads and instantiates specified JavaScript module as "main" module.
  pub async fn preload_main_module(
    &mut self,
    module_specifier: &ModuleSpecifier,
  ) -> Result<ModuleId, CoreError> {
    self.js_runtime.load_main_es_module(module_specifier).await
  }

  /// Loads and instantiates specified JavaScript module as "side" module.
  pub async fn preload_side_module(
    &mut self,
    module_specifier: &ModuleSpecifier,
  ) -> Result<ModuleId, CoreError> {
    self.js_runtime.load_side_es_module(module_specifier).await
  }

  /// Loads, instantiates and executes specified JavaScript module.
  ///
  /// This method assumes that worker can't be terminated when executing
  /// side module code.
  pub async fn execute_side_module(
    &mut self,
    module_specifier: &ModuleSpecifier,
  ) -> Result<(), CoreError> {
    let id = self.preload_side_module(module_specifier).await?;
    let mut receiver = self.js_runtime.mod_evaluate(id);
    tokio::select! {
      biased;

      maybe_result = &mut receiver => {
        debug!("received module evaluate {:#?}", maybe_result);
        maybe_result
      }

      event_loop_result = self.js_runtime.run_event_loop(PollEventLoopOptions::default()) => {
        event_loop_result?;
        receiver.await
      }
    }
  }

  /// Loads, instantiates and executes specified JavaScript module.
  ///
  /// This module will have "import.meta.main" equal to true.
  pub async fn execute_main_module(
    &mut self,
    id: ModuleId,
  ) -> Result<(), CoreError> {
    let mut receiver = self.js_runtime.mod_evaluate(id);
    let poll_options = PollEventLoopOptions::default();

    tokio::select! {
      biased;

      maybe_result = &mut receiver => {
        debug!("received worker module evaluate {:#?}", maybe_result);
        maybe_result
      }

      event_loop_result = self.run_event_loop(poll_options) => {
        if self.internal_handle.is_terminated() {
           return Ok(());
        }
        event_loop_result?;
        receiver.await
      }
    }
  }

  fn poll_event_loop(
    &mut self,
    cx: &mut Context,
    poll_options: PollEventLoopOptions,
  ) -> Poll<Result<(), CoreError>> {
    // If awakened because we are terminating, just return Ok
    if self.internal_handle.terminate_if_needed() {
      return Poll::Ready(Ok(()));
    }

    self.internal_handle.terminate_waker.register(cx.waker());

    match self.js_runtime.poll_event_loop(cx, poll_options) {
      Poll::Ready(r) => {
        // If js ended because we are terminating, just return Ok
        if self.internal_handle.terminate_if_needed() {
          return Poll::Ready(Ok(()));
        }

        if let Err(e) = r {
          return Poll::Ready(Err(e));
        }

        if self.close_on_idle {
          if self.has_message_event_listener() {
            return Poll::Pending;
          }
          return Poll::Ready(Ok(()));
        }

        // TODO(mmastrac): we don't want to test this w/classic workers because
        // WPT triggers a failure here. This is only exposed via --enable-testing-features-do-not-use.
        if self.worker_type == WorkerThreadType::Module {
          panic!(
            "coding error: either js is polling or the worker is terminated"
          );
        } else {
          log::error!("classic worker terminated unexpectedly");
          Poll::Ready(Ok(()))
        }
      }
      Poll::Pending => Poll::Pending,
    }
  }

  pub async fn run_event_loop(
    &mut self,
    poll_options: PollEventLoopOptions,
  ) -> Result<(), CoreError> {
    poll_fn(|cx| self.poll_event_loop(cx, poll_options)).await
  }

  // Starts polling for messages from worker host from JavaScript.
  fn start_polling_for_messages(&mut self) {
    let poll_for_messages_fn = self.poll_for_messages_fn.take().unwrap();
    let scope = &mut self.js_runtime.handle_scope();
    let poll_for_messages =
      v8::Local::<v8::Value>::new(scope, poll_for_messages_fn);
    let fn_ = v8::Local::<v8::Function>::try_from(poll_for_messages).unwrap();
    let undefined = v8::undefined(scope);
    // This call may return `None` if worker is terminated.
    fn_.call(scope, undefined.into(), &[]);
  }

  fn has_message_event_listener(&mut self) -> bool {
    let has_message_event_listener_fn =
      self.has_message_event_listener_fn.as_ref().unwrap();
    let scope = &mut self.js_runtime.handle_scope();
    let has_message_event_listener =
      v8::Local::<v8::Value>::new(scope, has_message_event_listener_fn);
    let fn_ =
      v8::Local::<v8::Function>::try_from(has_message_event_listener).unwrap();
    let undefined = v8::undefined(scope);
    // This call may return `None` if worker is terminated.
    match fn_.call(scope, undefined.into(), &[]) {
      Some(result) => result.is_true(),
      None => false,
    }
  }
}

fn print_worker_error(
  error: &CoreError,
  name: &str,
  format_js_error_fn: Option<&FormatJsErrorFn>,
) {
  let error_str = format_js_error_fn
    .as_ref()
    .and_then(|format_js_error_fn| {
      let err = match error.as_kind() {
        CoreErrorKind::Js(js_error) => js_error,
        CoreErrorKind::JsBox(err) => {
          err.get_ref().downcast_ref::<deno_core::error::JsError>()?
        }
        _ => return None,
      };
      Some(format_js_error_fn(err))
    })
    .unwrap_or_else(|| error.to_string());
  log::error!(
    "{}: Uncaught (in worker \"{}\") {}",
    colors::red_bold("error"),
    name,
    error_str.trim_start_matches("Uncaught "),
  );
}

/// This function should be called from a thread dedicated to this worker.
// TODO(bartlomieju): check if order of actions is aligned to Worker spec
// TODO(bartlomieju): run following block using "select!"
// with terminate
pub async fn run_web_worker(
  mut worker: WebWorker,
  specifier: ModuleSpecifier,
  mut maybe_source_code: Option<String>,
  format_js_error_fn: Option<Arc<FormatJsErrorFn>>,
) -> Result<(), CoreError> {
  worker.setup_memory_trim_handler();

  let name = worker.name.to_string();
  let internal_handle = worker.internal_handle.clone();

  // Execute provided source code immediately
  let result = if let Some(source_code) = maybe_source_code.take() {
    let r = worker.execute_script(located_script_name!(), source_code.into());
    worker.start_polling_for_messages();
    r
  } else {
    // TODO(bartlomieju): add "type": "classic", ie. ability to load
    // script instead of module
    match worker.preload_main_module(&specifier).await {
      Ok(id) => {
        worker.start_polling_for_messages();
        worker.execute_main_module(id).await
      }
      Err(e) => Err(e),
    }
  };

  // If sender is closed it means that worker has already been closed from
  // within using "globalThis.close()"
  if internal_handle.is_terminated() {
    return Ok(());
  }

  let result = if result.is_ok() {
    worker
      .run_event_loop(PollEventLoopOptions {
        wait_for_inspector: true,
        ..Default::default()
      })
      .await
  } else {
    result
  };

  if let Err(e) = result {
    print_worker_error(&e, &name, format_js_error_fn.as_deref());
    internal_handle
      .post_event(WorkerControlEvent::TerminalError(e))
      .expect("Failed to post message to host");

    // Failure to execute script is a terminal error, bye, bye.
    return Ok(());
  }

  debug!("Worker thread shuts down {}", &name);
  result
}
