use anyhow::Error;
use bytes::Bytes;
use futures::executor::block_on;
use http::{header::HeaderName, HeaderMap, HeaderValue};
use once_cell::sync::OnceCell;
use reqwest::{Client, Method};
use std::{
    collections::HashMap,
    str::FromStr,
    string::FromUtf8Error,
    sync::{Arc, PoisonError, RwLock},
};
use tokio::runtime::Handle;
use url::Url;
use wasmer::{
    Exports, Function, ImportObject, LazyInit, Memory, MemoryError, MemoryType, Store, WasmerEnv,
};
use wasmer_wasi::WasiEnv;

const MEMORY: &str = "memory";

pub type WasiHttpHandle = u32;

/// Response body for HTTP requests, consumed by guest modules.
struct Body {
    bytes: Bytes,
    pos: usize,
}

/// An HTTP response abstraction that is persisted across multiple
/// host calls.
struct Response {
    headers: HeaderMap,
    body: Body,
}

struct GlobalState {
    memory: Memory,
    state: Arc<RwLock<State>>,
    allowed_hosts: Option<Vec<String>>,
    max_concurrent_requests: Option<u32>,
}

const GLOBAL_STATE: OnceCell<GlobalState> = OnceCell::new();

/// Host state for the responses of the instance.
#[derive(Default)]
struct State {
    responses: HashMap<WasiHttpHandle, Response>,
    current_handle: WasiHttpHandle,
}

#[derive(Debug, thiserror::Error)]
enum HttpError {
    #[error("Invalid handle: [{0}]")]
    InvalidHandle(WasiHttpHandle),
    #[error("Memory not found")]
    MemoryNotFound,
    #[error("Memory access error")]
    MemoryError(#[from] MemoryError),
    #[error("Buffer too small")]
    BufferTooSmall,
    #[error("Header not found")]
    HeaderNotFound,
    #[error("UTF-8 error")]
    Utf8Error(#[from] std::str::Utf8Error),

    #[error("Destination not allowed")]
    DestinationNotAllowed(String),
    #[error("Invalid method")]
    InvalidMethod,
    #[error("Invalid encoding")]
    InvalidEncoding,
    #[error("Invalid URL")]
    InvalidUrl,
    #[error("HTTP error")]
    RequestError(#[from] reqwest::Error),
    #[error("Runtime error")]
    RuntimeError,
    #[error("Too many sessions")]
    TooManySessions,
    #[error("From UTF-8 error")]
    FromUtf8Error(#[from] FromUtf8Error),
}

impl From<HttpError> for u32 {
    fn from(e: HttpError) -> u32 {
        match e {
            HttpError::InvalidHandle(_) => 1,
            HttpError::MemoryNotFound => 2,
            HttpError::MemoryError(_) => 3,
            HttpError::BufferTooSmall => 4,
            HttpError::HeaderNotFound => 5,
            HttpError::Utf8Error(_) => 6,
            HttpError::DestinationNotAllowed(_) => 7,
            HttpError::InvalidMethod => 8,
            HttpError::InvalidEncoding => 9,
            HttpError::InvalidUrl => 10,
            HttpError::RequestError(_) => 11,
            HttpError::RuntimeError => 12,
            HttpError::TooManySessions => 13,
            HttpError::FromUtf8Error(_) => 14,
        }
    }
}

impl From<PoisonError<std::sync::RwLockReadGuard<'_, State>>> for HttpError {
    fn from(_: PoisonError<std::sync::RwLockReadGuard<'_, State>>) -> Self {
        HttpError::RuntimeError
    }
}

impl From<PoisonError<std::sync::RwLockWriteGuard<'_, State>>> for HttpError {
    fn from(_: PoisonError<std::sync::RwLockWriteGuard<'_, State>>) -> Self {
        HttpError::RuntimeError
    }
}

impl From<PoisonError<&mut State>> for HttpError {
    fn from(_: PoisonError<&mut State>) -> Self {
        HttpError::RuntimeError
    }
}

struct HostCalls;

impl HostCalls {
    /// Remove the current handle from the state.
    /// Depending on the implementation, guest modules might
    /// have to manually call `close`.
    // TODO (@radu-matei)
    // Fix the clippy warning.
    #[allow(clippy::unnecessary_wraps)]
    fn close(Env { state, .. }: &Env, handle: WasiHttpHandle) -> Result<(), HttpError> {
        let mut st = state.write()?;
        st.responses.remove(&handle);
        Ok(())
    }

    // TODO use a macro for this
    fn close_binding(env: &Env, handle: WasiHttpHandle) -> u32 {
        match Self::close(env, handle) {
            Ok(()) => 0,
            Err(err) => err.into(),
        }
    }
    /// Read `buf_len` bytes from the response of `handle` and
    /// write them into `buf_ptr`.
    fn body_read(
        // st: Arc<RwLock<State>>,
        // memory: &Memory,
        // mut store: impl AsContextMut,
        Env { wasi_env, state }: &Env,
        handle: WasiHttpHandle,
        buf_ptr: u32,
        buf_len: u32,
        buf_read_ptr: u32,
    ) -> Result<(), HttpError> {
        let memory = wasi_env.memory();
        let mut st = state.write()?;

        let mut body = &mut st.responses.get_mut(&handle).unwrap().body;
        // let mut context = store.as_context_mut();

        // Write at most either the remaining of the response body, or the entire
        // length requested by the guest.
        let available = std::cmp::min(buf_len as _, body.bytes.len() - body.pos);
        let memory_content = unsafe { memory.data_unchecked_mut() };
        for i in 0..available {
            memory_content[buf_ptr as usize + i] = body.bytes[body.pos + i]
        }
        // memory.write(
        //     &mut context,
        //     buf_ptr as _,
        //     &body.bytes[body.pos..body.pos + available],
        // )?;
        body.pos += available;
        for (i, b) in (available as u32).to_le_bytes().iter().enumerate() {
            memory_content[buf_read_ptr as usize + i] = *b;
        }
        // Write the number of bytes written back to the guest.
        // memory.write(
        //     &mut context,
        //     buf_read_ptr as _,
        //     &(available as u32).to_le_bytes(),
        // )?;
        Ok(())
    }

    fn body_read_binding(
        // st: Arc<RwLock<State>>,
        // memory: &Memory,
        // mut store: impl AsContextMut,
        env: &Env,
        handle: WasiHttpHandle,
        buf_ptr: u32,
        buf_len: u32,
        buf_read_ptr: u32,
    ) -> u32 {
        match Self::body_read(env, handle, buf_ptr, buf_len, buf_read_ptr) {
            Ok(()) => 0,
            Err(err) => err.into(),
        }
    }

    /// Get a response header value given a key.
    #[allow(clippy::too_many_arguments)]
    fn header_get(
        // st: Arc<RwLock<State>>,
        // mut store: impl AsContextMut,
        // memory: &Memory,
        Env { wasi_env, state }: &Env,
        handle: WasiHttpHandle,
        name_ptr: u32,
        name_len: u32,
        value_ptr: u32,
        value_len: u32,
        value_written_ptr: u32,
    ) -> Result<(), HttpError> {
        println!("header_get");
        let memory = wasi_env.memory();

        let st = state.read()?;

        // Get the current response headers.
        let headers = &st
            .responses
            .get(&handle)
            .ok_or(HttpError::InvalidHandle(handle))?
            .headers;

        // Read the header key from the module's memory.
        // let key = string_from_memory(&memory, &mut store, name_ptr, name_len)?.to_ascii_lowercase();
        let key = String::from_utf8(
            unsafe { memory.data_unchecked() }
                [name_ptr as usize..name_ptr as usize + name_len as usize]
                .to_vec(),
        )?;

        // Attempt to get the corresponding value from the resposne headers.
        let value = headers.get(key).ok_or(HttpError::HeaderNotFound)?;
        if value.len() > value_len as _ {
            return Err(HttpError::BufferTooSmall);
        }
        let memory_content = unsafe { memory.data_unchecked_mut() };
        for (i, v) in value.as_bytes().iter().enumerate() {
            memory_content[value_ptr as usize + i] = *v;
        }
        // Write the header value and its length.
        // memory.write(&mut store, value_ptr as _, value.as_bytes())?;
        for (i, v) in (value.len() as u32).to_le_bytes().iter().enumerate() {
            memory_content[value_written_ptr as usize + i] = *v;
        }
        // memory.write(
        //     &mut store,
        //     value_written_ptr as _,
        //     &(value.len() as u32).to_le_bytes(),
        // )?;
        Ok(())
    }

    fn header_get_binding(
        // st: Arc<RwLock<State>>,
        // mut store: impl AsContextMut,
        // memory: &Memory,
        env: &Env,

        handle: WasiHttpHandle,
        name_ptr: u32,
        name_len: u32,
        value_ptr: u32,
        value_len: u32,
        value_written_ptr: u32,
    ) -> u32 {
        match Self::header_get(
            env,
            handle,
            name_ptr,
            name_len,
            value_ptr,
            value_len,
            value_written_ptr,
        ) {
            Ok(()) => 0,
            Err(err) => err.into(),
        }
    }

    fn headers_get_all(
        // st: Arc<RwLock<State>>,
        // memory: &Memory,
        // mut store: impl AsContextMut,
        Env { wasi_env, state }: &Env,
        handle: WasiHttpHandle,
        buf_ptr: u32,
        buf_len: u32,
        buf_written_ptr: u32,
    ) -> Result<(), HttpError> {
        println!("header_get_all");
        let st = state.read()?;
        let memory = wasi_env.memory();

        let headers = &st
            .responses
            .get(&handle)
            .ok_or(HttpError::InvalidHandle(handle))?
            .headers;

        let headers = match header_map_to_string(headers) {
            Ok(res) => res,
            Err(_) => return Err(HttpError::RuntimeError),
        };

        if headers.len() > buf_len as _ {
            return Err(HttpError::BufferTooSmall);
        }

        // let mut store = store.as_context_mut();

        // memory.write(&mut store, buf_ptr as _, headers.as_bytes())?;
        let memory_content = unsafe { memory.data_unchecked_mut() };
        for (i, v) in headers.as_bytes().iter().enumerate() {
            memory_content[buf_ptr as usize + i] = *v;
        }

        // memory.write(
        //     &mut store,
        //     buf_written_ptr as _,
        //     &(headers.len() as u32).to_le_bytes(),
        // )?;
        for (i, v) in (headers.len() as u32).to_le_bytes().iter().enumerate() {
            memory_content[buf_written_ptr as usize + i] = *v;
        }
        Ok(())
    }

    fn headers_get_all_binding(
        // st: Arc<RwLock<State>>,
        // memory: &Memory,
        // mut store: impl AsContextMut,
        env: &Env,
        handle: WasiHttpHandle,
        buf_ptr: u32,
        buf_len: u32,
        buf_written_ptr: u32,
    ) -> u32 {
        match Self::headers_get_all(env, handle, buf_ptr, buf_len, buf_written_ptr) {
            Ok(_) => 0,
            Err(err) => err.into(),
        }
    }

    /// Execute a request for a guest module, given
    /// the request data.
    #[allow(clippy::too_many_arguments)]
    fn req(
        Env { wasi_env, state }: &Env,
        // allowed_hosts: Option<&[String]>,
        // max_concurrent_requests: Option<u32>,
        // mut store: impl AsContextMut,
        url_ptr: u32,
        url_len: u32,
        method_ptr: u32,
        method_len: u32,
        req_headers_ptr: u32,
        req_headers_len: u32,
        req_body_ptr: u32,
        req_body_len: u32,
        status_code_ptr: u32,
        res_handle_ptr: u32,
    ) -> Result<(), HttpError> {
        println!("req called");
        // let state = GLOBAL_STATE
        //     .get()
        //     .expect("cannot get global state")
        //     .state
        //     .clone();
        // let memory = GLOBAL_STATE
        //     .get()
        //     .expect("cannot get global state")
        //     .memory
        //     .clone();
        let memory = wasi_env.memory();
        let allowed_hosts: Option<Vec<String>> = None;
        let max_concurrent_requests: Option<usize> = None;

        let span = tracing::trace_span!("req");
        let _enter = span.enter();
        println!("req ===========");

        let mut st = state.write()?;

        if let Some(max) = max_concurrent_requests {
            if st.responses.len() > (max - 1) as usize {
                return Err(HttpError::TooManySessions);
            }
        };
        println!("req !!!!!!!!!!!!!!!");

        // let mut store = store.as_context_mut();

        // Read the request parts from the module's linear memory and check early if
        // the guest is allowed to make a request to the given URL.
        let url = string_from_memory(&memory, url_ptr, url_len)?;
        println!("url --- {}", url);
        if !is_allowed(url.as_str(), allowed_hosts.as_ref())? {
            return Err(HttpError::DestinationNotAllowed(url));
        }

        let method =
            Method::from_str(string_from_memory(&memory, method_ptr, method_len)?.as_str())
                .map_err(|_| HttpError::InvalidMethod)?;
        let req_body = slice_from_memory(&memory, req_body_ptr, req_body_len)?;
        let headers = string_to_header_map(
            string_from_memory(&memory, req_headers_ptr, req_headers_len)?.as_str(),
        )
        .map_err(|_| HttpError::InvalidEncoding)?;

        // Send the request.
        let (status, resp_headers, resp_body) =
            request(url.as_str(), headers, method, req_body.as_slice())?;
        tracing::debug!(
            status,
            ?resp_headers,
            body_len = resp_body.as_ref().len(),
            "got HTTP response, writing back to memory"
        );

        // Write the status code to the guest.
        // memory.write(&mut store, status_code_ptr as _, &status.to_le_bytes())?;
        let memory_content = unsafe { memory.data_unchecked_mut() };
        for (i, v) in status.to_le_bytes().iter().enumerate() {
            memory_content[status_code_ptr as usize + i] = *v;
        }

        // Construct the response, add it to the current state, and write
        // the handle to the guest.
        let response = Response {
            headers: resp_headers,
            body: Body {
                bytes: resp_body,
                pos: 0,
            },
        };

        let initial_handle = st.current_handle;
        while st.responses.get(&st.current_handle).is_some() {
            st.current_handle += 1;
            if st.current_handle == initial_handle {
                return Err(HttpError::TooManySessions);
            }
        }
        let handle = st.current_handle;
        st.responses.insert(handle, response);
        for (i, v) in handle.to_le_bytes().iter().enumerate() {
            memory_content[res_handle_ptr as usize + i] = *v;
        }

        Ok(())
    }

    fn req_binding(
        env: &Env,
        // allowed_hosts: Option<&[String]>,
        // max_concurrent_requests: Option<u32>,
        // mut store: impl AsContextMut,
        url_ptr: u32,
        url_len: u32,
        method_ptr: u32,
        method_len: u32,
        req_headers_ptr: u32,
        req_headers_len: u32,
        req_body_ptr: u32,
        req_body_len: u32,
        status_code_ptr: u32,
        res_handle_ptr: u32,
    ) -> u32 {
        match Self::req(
            env,
            url_ptr,
            url_len,
            method_ptr,
            method_len,
            req_headers_ptr,
            req_headers_len,
            req_body_ptr,
            req_body_len,
            status_code_ptr,
            res_handle_ptr,
        ) {
            Ok(_) => 0,
            Err(err) => err.into(),
        }
    }
}

/// Experimental HTTP extension object for Wasmtime.
pub struct HttpCtx {
    state: Arc<RwLock<State>>,
    allowed_hosts: Arc<Option<Vec<String>>>,
    max_concurrent_requests: Option<u32>,
}

#[derive(WasmerEnv, Clone)]
struct Env {
    state: Arc<RwLock<State>>,
    wasi_env: WasiEnv,
}

impl HttpCtx {
    /// Module the HTTP extension is going to be defined as.
    pub const MODULE: &'static str = "wasi_experimental_http";

    /// Create a new HTTP extension object.
    /// `allowed_hosts` may be `None` (no outbound connections allowed)
    /// or a list of allowed host names.
    pub fn new(
        allowed_hosts: Option<Vec<String>>,
        max_concurrent_requests: Option<u32>,
    ) -> Result<Self, Error> {
        let state = Arc::new(RwLock::new(State::default()));
        let allowed_hosts = Arc::new(allowed_hosts);
        Ok(HttpCtx {
            state,
            allowed_hosts,
            max_concurrent_requests,
        })
    }

    /// Register the module with the Wasmtime linker.
    pub fn add_to_import_object(
        &self,
        store: &Store,
        wasi_env: WasiEnv,
        import_object: &mut ImportObject,
    ) -> Result<(), Error> {
        // let mem = Memory::new(store, MemoryType::new(10, None, false))?;
        // mem.grow(1000)?;
        // println!("memory --- {:?}", mem);
        // let mem = wasi_env.memory();

        let st = self.state.clone();
        let mut env = Exports::new();

        // let close = move |handle: WasiHttpHandle| -> u32 {
        //     match HostCalls::close(st.clone(), handle) {
        //         Ok(()) => 0,
        //         Err(e) => e.into(),
        //     }
        // };
        // let memory = mem.clone();
        let func = Function::new_native_with_env(
            store,
            Env {
                wasi_env: wasi_env.clone(),
                state: st,
            },
            HostCalls::close_binding,
        );
        env.insert("close", func);
        println!("close added");

        // ----------------------------------------------------------

        let st = self.state.clone();
        // let memory = mem.clone();
        // let body_read = move |handle: WasiHttpHandle,
        //                       buf_ptr: u32,
        //                       buf_len: u32,
        //                       buf_read_ptr: u32|
        //       -> u32 {
        //     match HostCalls::body_read(st.clone(), &memory, handle, buf_ptr, buf_len, buf_read_ptr)
        //     {
        //         Ok(()) => 0,
        //         Err(e) => e.into(),
        //     }
        // };
        let func = Function::new_native_with_env(
            store,
            Env {
                wasi_env: wasi_env.clone(),
                state: st,
            },
            HostCalls::body_read_binding,
        );
        env.insert("body_read", func);

        // ----------------------------------------------------------

        let st = self.state.clone();
        // let memory = mem.clone();
        // let header_get = move |handle: WasiHttpHandle,
        //                        name_ptr: u32,
        //                        name_len: u32,
        //                        value_ptr: u32,
        //                        value_len: u32,
        //                        value_written_ptr: u32|
        //       -> u32 {
        //     match HostCalls::header_get(
        //         st.clone(),
        //         &memory,
        //         handle,
        //         name_ptr,
        //         name_len,
        //         value_ptr,
        //         value_len,
        //         value_written_ptr,
        //     ) {
        //         Ok(()) => 0,
        //         Err(e) => e.into(),
        //     }
        // };

        let func = Function::new_native_with_env(
            store,
            Env {
                wasi_env: wasi_env.clone(),
                state: st,
            },
            HostCalls::header_get_binding,
        );
        env.insert("header_get", func);

        // ----------------------------------------------------------

        let st = self.state.clone();
        // let memory = mem.clone();

        // let headers_get_all =
        //     move |handle: WasiHttpHandle, buf_ptr: u32, buf_len: u32, buf_read_ptr: u32| -> u32 {
        //         match HostCalls::headers_get_all(
        //             st.clone(),
        //             &memory,
        //             handle,
        //             buf_ptr,
        //             buf_len,
        //             buf_read_ptr,
        //         ) {
        //             Ok(()) => 0,
        //             Err(e) => e.into(),
        //         }
        //     };

        let func = Function::new_native_with_env(
            store,
            Env {
                state: st,
                wasi_env: wasi_env.clone(),
            },
            HostCalls::headers_get_all_binding,
        );
        env.insert("headers_get_all", func);

        // ----------------------------------------------------------

        let allowed_hosts = self.allowed_hosts.clone();
        let max_concurrent_requests = self.max_concurrent_requests;
        let st = self.state.clone();
        // let memory = mem.clone();
        let func = Function::new_native_with_env(
            store,
            Env {
                state: st,
                wasi_env: wasi_env.clone(),
            },
            HostCalls::req_binding,
        );
        env.insert("req", func);

        // ----------------------------------------------------------

        import_object.register(Self::MODULE, env);

        Ok(())
    }
}

#[tracing::instrument]
fn request(
    url: &str,
    headers: HeaderMap,
    method: Method,
    body: &[u8],
) -> Result<(u16, HeaderMap<HeaderValue>, Bytes), HttpError> {
    tracing::debug!(
        %url,
        ?headers,
        ?method,
        body_len = body.len(),
        "performing request"
    );
    let url: Url = url.parse().map_err(|_| HttpError::InvalidUrl)?;
    let body = body.to_vec();
    match Handle::try_current() {
        Ok(r) => {
            // If running in a Tokio runtime, spawn a new blocking executor
            // that will send the HTTP request, and block on its execution.
            // This attempts to avoid any deadlocks from other operations
            // already executing on the same executor (compared with just
            // blocking on the current one).
            //
            // This should only be a temporary workaround, until we take
            // advantage of async functions in Wasmtime.
            tracing::trace!("tokio runtime available, spawning request on tokio thread");
            block_on(r.spawn_blocking(move || {
                let client = Client::builder().build().unwrap();
                let res = block_on(
                    client
                        .request(method, url)
                        .headers(headers)
                        .body(body)
                        .send(),
                )?;
                Ok((
                    res.status().as_u16(),
                    res.headers().clone(),
                    block_on(res.bytes())?,
                ))
            }))
            .map_err(|_| HttpError::RuntimeError)?
        }
        Err(_) => {
            tracing::trace!("no tokio runtime available, using blocking request");
            let res = reqwest::blocking::Client::new()
                .request(method, url)
                .headers(headers)
                .body(body)
                .send()?;
            return Ok((res.status().as_u16(), res.headers().clone(), res.bytes()?));
        }
    }
}

/// Get a slice of length `len` from `memory`, starting at `offset`.
/// This will return an `HttpError::BufferTooSmall` if the size of the
/// requested slice is larger than the memory size.
fn slice_from_memory(memory: &Memory, offset: u32, len: u32) -> Result<Vec<u8>, HttpError> {
    println!("==== offset {} and len {}", offset, len);

    let required_memory_size = offset.checked_add(len).ok_or(HttpError::BufferTooSmall)? as usize;
    // memory.grow(10)?;
    println!(
        ":::::::::: offset {} and len {} ---- memory size {}",
        offset,
        len,
        memory.data_size()
    );

    if required_memory_size > memory.data_size() as usize {
        return Err(HttpError::BufferTooSmall);
    }
    println!("slice_from_memory {:?}", unsafe {
        memory.data_unchecked().len()
    });
    println!("slice_from_memory {:?}", unsafe {
        unsafe { &memory.data_unchecked()[(offset as usize)..(offset as usize) + len as usize] }
    });

    Ok(
        unsafe { &memory.data_unchecked()[(offset as usize)..(offset as usize) + len as usize] }
            .to_vec(),
    )
}

/// Read a string of byte length `len` from `memory`, starting at `offset`.
fn string_from_memory(memory: &Memory, offset: u32, len: u32) -> Result<String, HttpError> {
    println!("offset {} and len {}", offset, len);
    let slice = slice_from_memory(&memory, offset, len)?;
    Ok(std::str::from_utf8(&slice)?.to_string())
}

/// Check if guest module is allowed to send request to URL, based on the list of
/// allowed hosts defined by the runtime.
/// If `None` is passed, the guest module is not allowed to send the request.
fn is_allowed(url: &str, allowed_hosts: Option<&Vec<String>>) -> Result<bool, HttpError> {
    let url_host = Url::parse(url)
        .map_err(|_| HttpError::InvalidUrl)?
        .host_str()
        .ok_or(HttpError::InvalidUrl)?
        .to_owned();
    match allowed_hosts {
        Some(domains) => {
            let allowed: Result<Vec<_>, _> = domains.iter().map(|d| Url::parse(d)).collect();
            let allowed = allowed.map_err(|_| HttpError::InvalidUrl)?;
            let a: Vec<&str> = allowed.iter().map(|u| u.host_str().unwrap()).collect();
            Ok(a.contains(&url_host.as_str()))
        }
        None => Ok(false),
    }
}

// The following two functions are copied from the `wasi_experimental_http`
// crate, because the Windows linker apparently cannot handle unresolved
// symbols from a crate, even when the caller does not actually use any of the
// external symbols.
//
// https://github.com/rust-lang/rust/issues/86125

/// Decode a header map from a string.
fn string_to_header_map(s: &str) -> Result<HeaderMap, Error> {
    let mut headers = HeaderMap::new();
    for entry in s.lines() {
        let mut parts = entry.splitn(2, ':');
        #[allow(clippy::clippy::clippy::or_fun_call)]
        let k = parts.next().ok_or(anyhow::format_err!(
            "Invalid serialized header: [{}]",
            entry
        ))?;
        let v = parts.next().unwrap();
        headers.insert(HeaderName::from_str(k)?, HeaderValue::from_str(v)?);
    }
    Ok(headers)
}

/// Encode a header map as a string.
fn header_map_to_string(hm: &HeaderMap) -> Result<String, Error> {
    let mut res = String::new();
    for (name, value) in hm
        .iter()
        .map(|(name, value)| (name.as_str(), std::str::from_utf8(value.as_bytes())))
    {
        let value = value?;
        anyhow::ensure!(
            !name
                .chars()
                .any(|x| x.is_control() || "(),/:;<=>?@[\\]{}".contains(x)),
            "Invalid header name"
        );
        anyhow::ensure!(
            !value.chars().any(|x| x.is_control()),
            "Invalid header value"
        );
        res.push_str(&format!("{}:{}\n", name, value));
    }
    Ok(res)
}

#[test]
fn test_allowed_domains() {
    let allowed_domains = vec![
        "https://api.brigade.sh".to_string(),
        "https://example.com".to_string(),
        "http://192.168.0.1".to_string(),
    ];

    assert_eq!(
        true,
        is_allowed(
            "https://api.brigade.sh/healthz",
            Some(allowed_domains.as_ref())
        )
        .unwrap()
    );
    assert_eq!(
        true,
        is_allowed(
            "https://example.com/some/path/with/more/paths",
            Some(allowed_domains.as_ref())
        )
        .unwrap()
    );
    assert_eq!(
        true,
        is_allowed("http://192.168.0.1/login", Some(allowed_domains.as_ref())).unwrap()
    );
    assert_eq!(
        false,
        is_allowed("https://test.brigade.sh", Some(allowed_domains.as_ref())).unwrap()
    );
}
