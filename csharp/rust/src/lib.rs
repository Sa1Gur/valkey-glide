// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

mod ffi;
use ffi::{
    convert_double_pointer_to_vec, create_connection_request, create_route, ConnectionConfig,
    ResponseValue, RouteInfo,
};
use glide_core::{
    client::Client as GlideClient,
    errors::{error_message, error_type, RequestErrorType},
    request_type::RequestType,
    ConnectionRequest,
};
use std::{
    ffi::{c_char, c_void, CStr, CString},
    fmt::Debug,
    sync::Arc,
};
use tokio::runtime::{Builder, Runtime};

#[repr(C)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
    Off = 5,
}

pub struct Client {
    runtime: Runtime,
    core: Arc<CommandExecutionCore>,
}

/// Success callback that is called when a command succeeds.
///
/// The success callback needs to copy the given data synchronously, since it will be dropped by Rust once the callback returns.
/// The callback should be offloaded to a separate thread in order not to exhaust the client's thread pool.
///
/// # Arguments
/// * `index` is a baton-pass back to the caller language to uniquely identify the promise.
/// * `message` is the value returned by the command. The 'message' is managed by Rust and is freed when the callback returns control back to the caller.
///
/// # Safety
/// * The callback must copy the pointer in a sync manner and return ASAP. Any further data processing should be done in another thread to avoid
///   starving `tokio`'s thread pool.
/// * The callee is responsible to free memory by calling [`free_respose`] with the given pointer once only.
pub type SuccessCallback = unsafe extern "C-unwind" fn(usize, *const ResponseValue) -> ();

/// Failure callback that is called when a command fails.
///
/// The failure callback needs to copy the given string synchronously, since it will be dropped by Rust once the callback returns.
/// The callback should be offloaded to a separate thread in order not to exhaust the client's thread pool.
///
/// # Arguments
/// * `index` is a baton-pass back to the caller language to uniquely identify the promise.
/// * `error_message` is an UTF-8 string storing the error message returned by server for the failed command.
///   The `error_message` is managed by Rust and is freed when the callback returns control back to the caller.
/// * `error_type` is the type of error returned by glide-core, depending on the [`RedisError`](redis::RedisError) returned.
///
/// # Safety
/// * The callback must copy the data in a sync manner and return ASAP. Any further data processing should be done in another thread to avoid
///   starving `tokio`'s thread pool.
/// * The caller must free the memory allocated for [`error_message`] right after the call to avoid memory leak.
pub type FailureCallback = unsafe extern "C-unwind" fn(
    index: usize,
    error_message: *const c_char,
    error_type: RequestErrorType,
) -> ();

struct CommandExecutionCore {
    client: GlideClient,
    success_callback: SuccessCallback,
    failure_callback: FailureCallback,
}

/// This function handles Rust panics by logging them and reporting them into logs and via `failure_callback`.
/// `func` returns an [`Option<E>`], where `E` is an error type. It is supposed that `func` uses callbacks to report result or error,
/// and returns only an error if it happens.
///
/// # Safety
/// Unsafe, becase calls to an `unsafe` function [`report_error`]. Please see the safety section of [`report_error`].
unsafe fn handle_panics<E: Debug, F: std::panic::UnwindSafe + FnOnce() -> Option<E>>(
    func: F,
    ffi_func_name: &str,
    failure_callback: FailureCallback,
    callback_index: usize,
    error_type: Option<RequestErrorType>,
) {
    let error_type = match error_type {
        Some(t) => t,
        None => RequestErrorType::Unspecified,
    };
    match std::panic::catch_unwind(func) {
        Ok(None) => (), // no error
        Ok(Some(err)) => {
            // function returned an error
            let err_str = format!("Native function {} failed: {:?}", ffi_func_name, err);
            unsafe {
                report_error(failure_callback, callback_index, err_str, error_type);
            }
        }
        Err(err) => {
            // function panicked
            let err_str = format!("Native function {} panicked: {:?}", ffi_func_name, err);
            unsafe {
                report_error(failure_callback, callback_index, err_str, error_type);
            }
        }
    }
}

/// # Safety
/// Unsafe, becase calls to an FFI function. See the safety documentation of [`FailureCallback`].
unsafe fn report_error(
    failure_callback: FailureCallback,
    callback_index: usize,
    error_string: String,
    error_type: RequestErrorType,
) {
    logger_core::log(logger_core::Level::Error, "ffi", &error_string);
    let err_ptr = CString::into_raw(
        CString::new(error_string).expect("Couldn't convert error message to CString"),
    );
    unsafe { failure_callback(callback_index, err_ptr, error_type) };
    // free memory
    _ = CString::from_raw(err_ptr);
}

/// # Safety
/// Unsafe, becase calls to an FFI function. See the safety documentation of [`SuccessCallback`].
unsafe fn create_client_internal(
    request: ConnectionRequest,
    success_callback: SuccessCallback,
    failure_callback: FailureCallback,
) -> Result<(), String> {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("GLIDE C# thread")
        .build()
        .map_err(|err| error_message(&err.into()))?;

    let _runtime_handle = runtime.enter();
    let client = runtime
        .block_on(GlideClient::new(request, None))
        .map_err(|err| err.to_string())?;

    let core = Arc::new(CommandExecutionCore {
        success_callback,
        failure_callback,
        client,
    });

    let client_ptr = Arc::into_raw(Arc::new(Client { runtime, core }));
    unsafe { success_callback(0, client_ptr as *const ResponseValue) };
    Ok(())
}

/// Creates a new client with the given configuration.
/// The success callback needs to copy the given string synchronously, since it will be dropped by Rust once the callback returns.
/// All callbacks should be offloaded to separate threads in order not to exhaust the client's thread pool.
///
/// # Safety
///
/// * `config` must be a valid [`ConnectionConfig`] pointer. See the safety documentation of [`create_connection_request`].
/// * `success_callback` and `failure_callback` must be valid pointers to the corresponding FFI functions.
///   See the safety documentation of [`SuccessCallback`] and [`FailureCallback`].
#[allow(rustdoc::private_intra_doc_links)]
#[no_mangle]
pub unsafe extern "C" fn create_client(
    config: *const ConnectionConfig,
    success_callback: SuccessCallback,
    failure_callback: FailureCallback,
) {
    handle_panics(
        move || {
            let request = unsafe { create_connection_request(config) };
            let res = create_client_internal(request, success_callback, failure_callback);
            res.err()
        },
        "create_client",
        failure_callback,
        0,
        Some(RequestErrorType::Disconnect),
    );
}

/// Closes the given client, deallocating it from the heap.
/// This function should only be called once per pointer created by [`create_client`].
/// After calling this function the `client_ptr` is not in a valid state.
///
/// # Safety
///
/// * `client_ptr` must not be `null`.
/// * `client_ptr` must be able to be safely casted to a valid [`Arc<Client>`] via [`Box::from_raw`]. See the safety documentation of [`Box::from_raw`].
#[no_mangle]
pub extern "C" fn close_client(client_ptr: *const c_void) {
    assert!(!client_ptr.is_null());
    // This will bring the strong count down to 0 once all client requests are done.
    unsafe { Arc::decrement_strong_count(client_ptr as *const Client) };
}

// TODO handle panic if possible
/// Execute a command.
/// Expects that arguments will be kept valid until the callback is called.
///
/// # Safety
/// * `client_ptr` must not be `null`.
/// * `client_ptr` must be able to be safely casted to a valid [`Arc<Client>`] via [`Box::from_raw`]. See the safety documentation of [`Box::from_raw`].
/// * This function should only be called should with a pointer created by [`create_client`], before [`close_client`] was called with the pointer.
/// * Pointers to callbacks stored in [`Client`] should remain valid. See the safety documentation of [`SuccessCallback`] and [`FailureCallback`].
/// * `args` and `args_len` must not be `null`.
/// * `data` must point to `arg_count` consecutive string pointers.
/// * `args_len` must point to `arg_count` consecutive string lengths. See the safety documentation of [`convert_double_pointer_to_vec`].
/// * `route_info` could be `null`, but if it is not `null`, it must be a valid [`RouteInfo`] pointer. See the safety documentation of [`create_route`].
#[allow(rustdoc::private_intra_doc_links)]
#[no_mangle]
pub unsafe extern "C-unwind" fn command(
    client_ptr: *const c_void,
    callback_index: usize,
    request_type: RequestType,
    args: *const *mut c_char,
    arg_count: u32,
    args_len: *const u32,
    route_info: *const RouteInfo,
) {
    let client = unsafe {
        // we increment the strong count to ensure that the client is not dropped just because we turned it into an Arc.
        Arc::increment_strong_count(client_ptr);
        Arc::from_raw(client_ptr as *mut Client)
    };
    let core = client.core.clone();

    let arg_vec =
        unsafe { convert_double_pointer_to_vec(args as *const *const c_void, arg_count, args_len) };

    // Create the command outside of the task to ensure that the command arguments passed are still valid
    let Some(mut cmd) = request_type.get_command() else {
        let err_str = "Couldn't fetch command type".into();
        unsafe {
            report_error(
                core.failure_callback,
                callback_index,
                err_str,
                RequestErrorType::ExecAbort,
            );
        }
        return;
    };
    for command_arg in arg_vec {
        cmd.arg(command_arg);
    }

    let route = create_route(route_info, &cmd);

    client.runtime.spawn(async move {
        let result = core.client.clone().send_command(&cmd, route).await;
        match result {
            Ok(value) => {
                let ptr = Box::into_raw(Box::new(ResponseValue::from_value(value)));
                unsafe {
                    (core.success_callback)(callback_index, ptr);
                }
            }
            Err(err) => {
                let err_str = error_message(&err);
                unsafe {
                    report_error(
                        core.failure_callback,
                        callback_index,
                        err_str,
                        error_type(&err),
                    );
                }
            }
        };
    });
}

/// Free the memory allocated for a [`ResponseValue`] and nested structure.
///
/// # Safety
/// * `ptr` must not be `null`.
/// * `ptr` must be able to be safely casted to a valid [`Box<ResponseValue>`] via [`Box::from_raw`]. See the safety documentation of [`Box::from_raw`].
#[allow(rustdoc::private_intra_doc_links)]
#[no_mangle]
pub unsafe extern "C" fn free_respose(ptr: *mut ResponseValue) {
    unsafe {
        let val = Box::leak(Box::from_raw(ptr));
        val.free_memory();
    }
}

impl From<logger_core::Level> for Level {
    fn from(level: logger_core::Level) -> Self {
        match level {
            logger_core::Level::Error => Level::Error,
            logger_core::Level::Warn => Level::Warn,
            logger_core::Level::Info => Level::Info,
            logger_core::Level::Debug => Level::Debug,
            logger_core::Level::Trace => Level::Trace,
            logger_core::Level::Off => Level::Off,
        }
    }
}

impl From<Level> for logger_core::Level {
    fn from(level: Level) -> logger_core::Level {
        match level {
            Level::Error => logger_core::Level::Error,
            Level::Warn => logger_core::Level::Warn,
            Level::Info => logger_core::Level::Info,
            Level::Debug => logger_core::Level::Debug,
            Level::Trace => logger_core::Level::Trace,
            Level::Off => logger_core::Level::Off,
        }
    }
}

/// Unsafe function because creating string from pointer.
///
/// # Safety
///
/// * `message` and `log_identifier` must not be `null`.
/// * `message` and `log_identifier` must be able to be safely casted to a valid [`CStr`] via [`CStr::from_ptr`]. See the safety documentation of [`std::ffi::CStr::from_ptr`].
#[no_mangle]
#[allow(improper_ctypes_definitions)]
pub unsafe extern "C" fn log(
    log_level: Level,
    log_identifier: *const c_char,
    message: *const c_char,
) {
    unsafe {
        logger_core::log(
            log_level.into(),
            CStr::from_ptr(log_identifier)
                .to_str()
                .expect("Can not read log_identifier argument."),
            CStr::from_ptr(message)
                .to_str()
                .expect("Can not read message argument."),
        );
    }
}

/// Unsafe function because creating string from pointer.
///
/// # Safety
///
/// * `file_name` must not be `null`.
/// * `file_name` must be able to be safely casted to a valid [`CStr`] via [`CStr::from_ptr`]. See the safety documentation of [`std::ffi::CStr::from_ptr`].
#[no_mangle]
#[allow(improper_ctypes_definitions)]
pub unsafe extern "C" fn init(level: Option<Level>, file_name: *const c_char) -> Level {
    let file_name_as_str;
    unsafe {
        file_name_as_str = if file_name.is_null() {
            None
        } else {
            Some(
                CStr::from_ptr(file_name)
                    .to_str()
                    .expect("Can not read string argument."),
            )
        };

        let logger_level = logger_core::init(level.map(|level| level.into()), file_name_as_str);
        logger_level.into()
    }
}
