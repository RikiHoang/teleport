// Copyright 2021 Gravitational, Inc
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This crate contains an RDP Client with the minimum functionality required
//! for Teleport's Desktop Access feature.
//!
//! Along with core RDP functionality, it contains code for:
//! - Calling functions defined in Go (these are declared in an `extern "C"` block)
//! - Functions to be called from Go (any function prefixed with the `#[no_mangle]`
//!   macro and a `pub unsafe extern "C"`).
//! - Structs for passing between the two (those prefixed with the `#[repr(C)]` macro
//!   and whose name begins with `CGO`)
//!
//! Memory management at this interface can be tricky, given the long list of rules
//! required by CGO (https://pkg.go.dev/cmd/cgo). We can simplify our job in this
//! regard by sticking to the following design principles:
//!
//! 1) Whichever side of the Rust-Go interface allocates some memory on the heap is
//!    responsible for freeing it.
//! 2) And therefore whenever one side of the Rust-Go interface is passed some memory
//!    it didn't allocate but needs to hold on to, is responsible for copying it to its
//!    own respective heap.
//!
//! In practice, this means that all the functions called from Go (those prefixed with
//! `pub unsafe extern "C"`) MUST NOT hang on to any of the pointers passed in to them after
//! they return. All pointer data that needs to persist MUST be copied into Rust-owned memory.

mod cliprdr;
mod errors;
mod piv;
mod rdpdr;
mod util;
mod vchan;

#[macro_use]
extern crate log;
#[macro_use]
extern crate num_derive;

use errors::try_error;
use libc::{fd_set, select, FD_SET};
use rand::Rng;
use rand::SeedableRng;
use rdp::core::event::*;
use rdp::core::gcc::KeyboardLayout;
use rdp::core::global;
use rdp::core::mcs;
use rdp::core::sec;
use rdp::core::tpkt;
use rdp::core::x224;
use rdp::model::error::{Error as RdpError, RdpError as RdpProtocolError, RdpErrorKind, RdpResult};
use rdp::model::link::{Link, Stream};
use rdpdr::path::UnixPath;
use rdpdr::ServerCreateDriveRequest;
use std::convert::TryFrom;
use std::ffi::CStr;
use std::io::Error as IoError;
use std::io::ErrorKind;
use std::io::{Cursor, Read, Write};
use std::net;
use std::net::{TcpStream, ToSocketAddrs};
use std::os::raw::c_char;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use std::{mem, ptr, slice, time};

#[no_mangle]
pub extern "C" fn init() {
    env_logger::try_init().unwrap_or_else(|e| println!("failed to initialize Rust logger: {}", e));
}

#[derive(Clone)]
struct SharedStream {
    tcp: Arc<TcpStream>,
}

impl SharedStream {
    fn new(tcp: TcpStream) -> Self {
        Self { tcp: Arc::new(tcp) }
    }
}

impl Read for SharedStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, IoError> {
        self.tcp.as_ref().read(buf)
    }
}

impl Write for SharedStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, IoError> {
        self.tcp.as_ref().write(buf)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.tcp.as_ref().flush()
    }
}

/// Client has an unusual lifecycle:
/// - connect_rdp creates it on the heap, grabs a raw pointer and returns in to Go
/// - most other exported rdp functions take the raw pointer, convert it to a reference for use
///   without dropping the Client
/// - free_rdp takes the raw pointer and drops it
///
/// All of the exported rdp functions could run concurrently, so the rdp_client is synchronized.
/// tcp_fd is only set in connect_rdp and used as read-only afterwards, so it does not need
/// synchronization.
pub struct Client {
    rdp_client: Arc<Mutex<RdpClient<SharedStream>>>,
    tcp_fd: usize,
    go_ref: usize,
    tcp: SharedStream,
}

impl Client {
    fn into_raw(self: Box<Self>) -> *mut Self {
        Box::into_raw(self)
    }
    unsafe fn from_ptr<'a>(ptr: *const Self) -> Result<&'a Client, CGOErrCode> {
        match ptr.as_ref() {
            Some(c) => Ok(c),
            None => {
                error!("invalid Rust client pointer");
                Err(CGOErrCode::ErrCodeFailure)
            }
        }
    }
    unsafe fn from_raw(ptr: *mut Self) -> Box<Self> {
        Box::from_raw(ptr)
    }
}

#[repr(C)]
pub struct ClientOrError {
    client: *mut Client,
    err: CGOErrCode,
}

impl From<Result<Client, ConnectError>> for ClientOrError {
    fn from(r: Result<Client, ConnectError>) -> ClientOrError {
        match r {
            Ok(client) => ClientOrError {
                client: Box::new(client).into_raw(),
                err: CGOErrCode::ErrCodeSuccess,
            },
            Err(e) => {
                error!("{:?}", e);
                ClientOrError {
                    client: ptr::null_mut(),
                    err: CGOErrCode::ErrCodeFailure,
                }
            }
        }
    }
}

/// connect_rdp establishes an RDP connection to go_addr with the provided credentials and screen
/// size. If succeeded, the client is internally registered under client_ref. When done with the
/// connection, the caller must call close_rdp.
///
/// # Safety
///
/// The caller mmust ensure that go_addr, go_username, cert_der, key_der point to valid buffers in respect
/// to their corresponding parameters.
#[no_mangle]
pub unsafe extern "C" fn connect_rdp(
    go_ref: usize,
    go_addr: *const c_char,
    go_username: *const c_char,
    cert_der_len: u32,
    cert_der: *mut u8,
    key_der_len: u32,
    key_der: *mut u8,
    screen_width: u16,
    screen_height: u16,
    allow_clipboard: bool,
    allow_directory_sharing: bool,
) -> ClientOrError {
    // Convert from C to Rust types.
    let addr = from_go_string(go_addr);
    let username = from_go_string(go_username);
    let cert_der = from_go_array(cert_der, cert_der_len);
    let key_der = from_go_array(key_der, key_der_len);

    connect_rdp_inner(
        go_ref,
        &addr,
        ConnectParams {
            username,
            cert_der,
            key_der,
            screen_width,
            screen_height,
            allow_clipboard,
            allow_directory_sharing,
        },
    )
    .into()
}

#[derive(Debug)]
enum ConnectError {
    Tcp(IoError),
    Rdp(RdpError),
    InvalidAddr(),
}

impl From<IoError> for ConnectError {
    fn from(e: IoError) -> ConnectError {
        ConnectError::Tcp(e)
    }
}

impl From<RdpError> for ConnectError {
    fn from(e: RdpError) -> ConnectError {
        ConnectError::Rdp(e)
    }
}

const RDP_CONNECT_TIMEOUT: time::Duration = time::Duration::from_secs(5);
const RDP_HANDSHAKE_TIMEOUT: time::Duration = time::Duration::from_secs(10);
const RDPSND_CHANNEL_NAME: &str = "rdpsnd";

struct ConnectParams {
    username: String,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    screen_width: u16,
    screen_height: u16,
    allow_clipboard: bool,
    allow_directory_sharing: bool,
}

fn connect_rdp_inner(
    go_ref: usize,
    addr: &str,
    params: ConnectParams,
) -> Result<Client, ConnectError> {
    // Connect and authenticate.
    let addr = addr
        .to_socket_addrs()?
        .next()
        .ok_or(ConnectError::InvalidAddr())?;
    let tcp = TcpStream::connect_timeout(&addr, RDP_CONNECT_TIMEOUT)?;
    let tcp_fd = tcp.as_raw_fd() as usize;
    // Domain name "." means current domain.
    let domain = ".";

    // From rdp-rs/src/core/client.rs
    let shared_tcp = SharedStream::new(tcp);
    // Set read timeout to prevent blocking forever on the handshake if the RDP server doesn't respond.
    shared_tcp
        .tcp
        .set_read_timeout(Some(RDP_HANDSHAKE_TIMEOUT))?;
    let tcp = Link::new(Stream::Raw(shared_tcp.clone()));
    let protocols = x224::Protocols::ProtocolSSL as u32 | x224::Protocols::ProtocolRDP as u32;
    let x224 = x224::Client::connect(tpkt::Client::new(tcp), protocols, false, None, false, false)?;
    let mut mcs = mcs::Client::new(x224);

    // request the static channels we'll need:
    // rdpdr: derive redirection (smart cards)
    // rdpsnd: sound (for some reason we need to request this)
    // cliprdr: clipboard
    let mut static_channels = vec![
        rdpdr::CHANNEL_NAME.to_string(),
        RDPSND_CHANNEL_NAME.to_string(),
    ];
    if params.allow_clipboard {
        static_channels.push(cliprdr::CHANNEL_NAME.to_string())
    }
    mcs.connect(
        "rdp-rs".to_string(),
        params.screen_width,
        params.screen_height,
        KeyboardLayout::US,
        &static_channels,
    )?;
    // Generate a random 8-digit PIN for our smartcard.
    let mut rng = rand_chacha::ChaCha20Rng::from_entropy();
    let pin = format!("{:08}", rng.gen_range(0i32..=99999999i32));
    sec::connect(
        &mut mcs,
        &domain.to_string(),
        &params.username,
        &pin,
        true,
        // InfoPasswordIsScPin means that the user will not be prompted for the smartcard PIN code,
        // which is known only to Teleport and unique for each RDP session.
        Some(sec::InfoFlag::InfoPasswordIsScPin as u32 | sec::InfoFlag::InfoMouseHasWheel as u32),
        Some(
            sec::ExtendedInfoFlag::PerfDisableCursorBlink as u32
                | sec::ExtendedInfoFlag::PerfDisableFullWindowDrag as u32
                | sec::ExtendedInfoFlag::PerfDisableMenuAnimations as u32,
        ),
    )?;
    // Client for the "global" channel - video output and user input.
    let global = global::Client::new(
        mcs.get_user_id(),
        mcs.get_global_channel_id(),
        params.screen_width,
        params.screen_height,
        KeyboardLayout::US,
        "rdp-rs",
    );

    let tdp_sd_acknowledge = Box::new(
        move |mut ack: SharedDirectoryAcknowledge| -> RdpResult<()> {
            debug!("sending TDP SharedDirectoryAcknowledge: {:?}", ack);
            unsafe {
                if tdp_sd_acknowledge(go_ref, &mut ack) != CGOErrCode::ErrCodeSuccess {
                    return Err(RdpError::TryError(String::from(
                        "call to tdp_sd_acknowledge failed",
                    )));
                }
                Ok(())
            }
        },
    );

    let tdp_sd_info_request = Box::new(move |req: SharedDirectoryInfoRequest| -> RdpResult<()> {
        debug!("sending TDP SharedDirectoryInfoRequest: {:?}", req);
        // Create C compatible string from req.path
        match req.path.to_cstring() {
            Ok(c_string) => {
                unsafe {
                    let err = tdp_sd_info_request(
                        go_ref,
                        &mut CGOSharedDirectoryInfoRequest {
                            completion_id: req.completion_id,
                            directory_id: req.directory_id,
                            path: c_string.as_ptr(),
                        },
                    );
                    if err != CGOErrCode::ErrCodeSuccess {
                        return Err(RdpError::TryError(String::from(
                            "call to tdp_sd_info_request failed",
                        )));
                    };
                }
                Ok(())
            }
            Err(_) => {
                // TODO(isaiah): change TryError to TeleportError for a generic error caused by Teleport specific code.
                return Err(RdpError::TryError(format!(
                    "path contained characters that couldn't be converted to a C string: {:?}",
                    req.path
                )));
            }
        }
    });

    let tdp_sd_create_request =
        Box::new(move |req: SharedDirectoryCreateRequest| -> RdpResult<()> {
            debug!("sending TDP SharedDirectoryCreateRequest: {:?}", req);
            // Create C compatible string from req.path
            match req.path.to_cstring() {
                Ok(c_string) => {
                    unsafe {
                        let err = tdp_sd_create_request(
                            go_ref,
                            &mut CGOSharedDirectoryCreateRequest {
                                completion_id: req.completion_id,
                                directory_id: req.directory_id,
                                file_type: req.file_type,
                                path: c_string.as_ptr(),
                            },
                        );
                        if err != CGOErrCode::ErrCodeSuccess {
                            return Err(RdpError::TryError(String::from(
                                "call to tdp_sd_create_request failed",
                            )));
                        };
                    }
                    Ok(())
                }
                Err(_) => {
                    // TODO(isaiah): change TryError to TeleportError for a generic error caused by Teleport specific code.
                    return Err(RdpError::TryError(format!(
                        "path contained characters that couldn't be converted to a C string: {:?}",
                        req.path
                    )));
                }
            }
        });

    let tdp_sd_delete_request =
        Box::new(move |req: SharedDirectoryDeleteRequest| -> RdpResult<()> {
            debug!("sending TDP SharedDirectoryDeleteRequest: {:?}", req);
            // Create C compatible string from req.path
            match req.path.to_cstring() {
                Ok(c_string) => {
                    unsafe {
                        let err = tdp_sd_delete_request(
                            go_ref,
                            &mut CGOSharedDirectoryDeleteRequest {
                                completion_id: req.completion_id,
                                directory_id: req.directory_id,
                                path: c_string.as_ptr(),
                            },
                        );
                        if err != CGOErrCode::ErrCodeSuccess {
                            return Err(RdpError::TryError(String::from(
                                "call to tdp_sd_delete_request failed",
                            )));
                        };
                    }
                    Ok(())
                }
                Err(_) => {
                    // TODO(isaiah): change TryError to TeleportError for a generic error caused by Teleport specific code.
                    return Err(RdpError::TryError(format!(
                        "path contained characters that couldn't be converted to a C string: {:?}",
                        req.path
                    )));
                }
            }
        });

    let tdp_sd_list_request = Box::new(move |req: SharedDirectoryListRequest| -> RdpResult<()> {
        debug!("sending TDP SharedDirectoryListRequest: {:?}", req);
        // Create C compatible string from req.path
        match req.path.to_cstring() {
            Ok(c_string) => {
                unsafe {
                    let err = tdp_sd_list_request(
                        go_ref,
                        &mut CGOSharedDirectoryListRequest {
                            completion_id: req.completion_id,
                            directory_id: req.directory_id,
                            path: c_string.as_ptr(),
                        },
                    );
                    if err != CGOErrCode::ErrCodeSuccess {
                        return Err(RdpError::TryError(String::from(
                            "call to tdp_sd_list_request failed",
                        )));
                    };
                }
                Ok(())
            }
            Err(_) => {
                // TODO(isaiah): change TryError to TeleportError for a generic error caused by Teleport specific code.
                return Err(RdpError::TryError(format!(
                    "path contained characters that couldn't be converted to a C string: {:?}",
                    req.path
                )));
            }
        }
    });

    let tdp_sd_read_request = Box::new(move |req: SharedDirectoryReadRequest| -> RdpResult<()> {
        debug!("sending TDP SharedDirectoryReadRequest: {:?}", req);
        match req.path.to_cstring() {
            Ok(c_string) => {
                unsafe {
                    let err = tdp_sd_read_request(
                        go_ref,
                        &mut CGOSharedDirectoryReadRequest {
                            completion_id: req.completion_id,
                            directory_id: req.directory_id,
                            path: c_string.as_ptr(),
                            path_length: req.path.len(),
                            offset: req.offset,
                            length: req.length,
                        },
                    );

                    if err != CGOErrCode::ErrCodeSuccess {
                        return Err(RdpError::TryError(String::from(
                            "call to tdp_sd_read_request failed",
                        )));
                    }
                }
                Ok(())
            }
            Err(_) => {
                return Err(RdpError::TryError(format!(
                    "path contained characters that couldn't be converted to a C string: {:?}",
                    req.path
                )));
            }
        }
    });

    let tdp_sd_write_request = Box::new(move |req: SharedDirectoryWriteRequest| -> RdpResult<()> {
        debug!("sending TDP SharedDirectoryWriteRequest: {:?}", req);
        match req.path.to_cstring() {
            Ok(c_string) => {
                unsafe {
                    let err = tdp_sd_write_request(
                        go_ref,
                        &mut CGOSharedDirectoryWriteRequest {
                            completion_id: req.completion_id,
                            directory_id: req.directory_id,
                            offset: req.offset,
                            path: c_string.as_ptr(),
                            path_length: req.path.len(),
                            write_data_length: req.write_data.len() as u32,
                            write_data: req.write_data.as_ptr() as *mut u8,
                        },
                    );

                    if err != CGOErrCode::ErrCodeSuccess {
                        return Err(RdpError::TryError(String::from(
                            "call to tdp_sd_write_failed",
                        )));
                    }
                }
                Ok(())
            }
            Err(_) => {
                return Err(RdpError::TryError(format!(
                    "path contained characters that couldn't be converted to a C string: {:?}",
                    req.path
                )));
            }
        }
    });

    let tdp_sd_move_request = Box::new(move |req: SharedDirectoryMoveRequest| -> RdpResult<()> {
        debug!("sending TDP SharedDirectoryMoveRequest: {:?}", req);
        match req.original_path.to_cstring() {
            Ok(original_path) => match req.new_path.to_cstring() {
                Ok(new_path) => {
                    unsafe {
                        let err = tdp_sd_move_request(
                            go_ref,
                            &mut CGOSharedDirectoryMoveRequest {
                                completion_id: req.completion_id,
                                directory_id: req.directory_id,
                                original_path: original_path.as_ptr(),
                                new_path: new_path.as_ptr(),
                            },
                        );

                        if err != CGOErrCode::ErrCodeSuccess {
                            return Err(RdpError::TryError(String::from(
                                "call to tdp_sd_Move_failed",
                            )));
                        }
                    }
                    Ok(())
                }
                Err(_) => {
                    return Err(RdpError::TryError(format!(
                            "new_path contained characters that couldn't be converted to a C string: {:?}",
                            req.new_path
                        )));
                }
            },
            Err(_) => {
                return Err(RdpError::TryError(format!(
                    "original_path contained characters that couldn't be converted to a C string: {:?}",
                    req.original_path
                )));
            }
        }
    });

    // Client for the "rdpdr" channel - smartcard emulation and drive redirection.
    let rdpdr = rdpdr::Client::new(rdpdr::Config {
        cert_der: params.cert_der,
        key_der: params.key_der,
        pin,
        allow_directory_sharing: params.allow_directory_sharing,
        tdp_sd_acknowledge,
        tdp_sd_info_request,
        tdp_sd_create_request,
        tdp_sd_delete_request,
        tdp_sd_list_request,
        tdp_sd_read_request,
        tdp_sd_write_request,
        tdp_sd_move_request,
    });

    // Client for the "cliprdr" channel - clipboard sharing.
    let cliprdr = if params.allow_clipboard {
        Some(cliprdr::Client::new(Box::new(move |v| -> RdpResult<()> {
            unsafe {
                if handle_remote_copy(go_ref, v.as_ptr() as _, v.len() as u32)
                    != CGOErrCode::ErrCodeSuccess
                {
                    return Err(errors::try_error("failed to handle remote copy"));
                }
            }
            Ok(())
        })))
    } else {
        None
    };

    let rdp_client = RdpClient {
        mcs,
        global,
        rdpdr,
        cliprdr,
    };

    // Reset read timeout as rdp-rs isn't build to handle it internally.
    // This won't cause a lockup later since at that point the close_rdp() function will be called which
    // will terminate the connection if the websocket disconnects.
    shared_tcp.tcp.set_read_timeout(None)?;

    Ok(Client {
        rdp_client: Arc::new(Mutex::new(rdp_client)),
        tcp_fd,
        go_ref,
        tcp: shared_tcp,
    })
}

/// From rdp-rs/src/core/client.rs
struct RdpClient<S> {
    mcs: mcs::Client<S>,
    global: global::Client,
    rdpdr: rdpdr::Client,

    cliprdr: Option<cliprdr::Client>,
}

impl<S: Read + Write> RdpClient<S> {
    pub fn read<T>(&mut self, callback: T) -> RdpResult<()>
    where
        T: FnMut(RdpEvent),
    {
        let (channel_name, message) = self.mcs.read()?;
        // De-multiplex static channels. Forward messages to the correct channel client based on
        // name.
        match channel_name.as_str() {
            "global" => self.global.read(message, &mut self.mcs, callback),
            rdpdr::CHANNEL_NAME => self.rdpdr.read_and_reply(message, &mut self.mcs),
            cliprdr::CHANNEL_NAME => match self.cliprdr {
                Some(ref mut clip) => clip.read_and_reply(message, &mut self.mcs),
                None => Ok(()),
            },
            RDPSND_CHANNEL_NAME => {
                debug!("skipping RDPSND message, audio output not supported");
                Ok(())
            }
            _ => Err(RdpError::RdpError(RdpProtocolError::new(
                RdpErrorKind::UnexpectedType,
                &format!("Invalid channel name {:?}", channel_name),
            ))),
        }
    }

    pub fn write(&mut self, event: RdpEvent) -> RdpResult<()> {
        match event {
            RdpEvent::Pointer(pointer) => {
                self.global.write_input_event(pointer.into(), &mut self.mcs)
            }
            RdpEvent::Key(key) => self.global.write_input_event(key.into(), &mut self.mcs),
            _ => Err(RdpError::RdpError(RdpProtocolError::new(
                RdpErrorKind::UnexpectedType,
                "RDPCLIENT: This event can't be sent",
            ))),
        }
    }

    pub fn write_client_device_list_announce(
        &mut self,
        req: rdpdr::ClientDeviceListAnnounce,
    ) -> RdpResult<()> {
        self.rdpdr
            .write_client_device_list_announce(req, &mut self.mcs)
    }

    pub fn handle_tdp_sd_info_response(
        &mut self,
        res: SharedDirectoryInfoResponse,
    ) -> RdpResult<()> {
        self.rdpdr.handle_tdp_sd_info_response(res, &mut self.mcs)
    }

    pub fn handle_tdp_sd_create_response(
        &mut self,
        res: SharedDirectoryCreateResponse,
    ) -> RdpResult<()> {
        self.rdpdr.handle_tdp_sd_create_response(res, &mut self.mcs)
    }

    pub fn handle_tdp_sd_delete_response(
        &mut self,
        res: SharedDirectoryDeleteResponse,
    ) -> RdpResult<()> {
        self.rdpdr.handle_tdp_sd_delete_response(res, &mut self.mcs)
    }

    pub fn handle_tdp_sd_list_response(
        &mut self,
        res: SharedDirectoryListResponse,
    ) -> RdpResult<()> {
        self.rdpdr.handle_tdp_sd_list_response(res, &mut self.mcs)
    }

    pub fn handle_tdp_sd_read_response(
        &mut self,
        res: SharedDirectoryReadResponse,
    ) -> RdpResult<()> {
        self.rdpdr.handle_tdp_sd_read_response(res, &mut self.mcs)
    }

    pub fn handle_tdp_sd_write_response(
        &mut self,
        res: SharedDirectoryWriteResponse,
    ) -> RdpResult<()> {
        self.rdpdr.handle_tdp_sd_write_response(res, &mut self.mcs)
    }

    pub fn handle_tdp_sd_move_response(
        &mut self,
        res: SharedDirectoryMoveResponse,
    ) -> RdpResult<()> {
        self.rdpdr.handle_tdp_sd_move_response(res, &mut self.mcs)
    }

    pub fn shutdown(&mut self) -> RdpResult<()> {
        self.mcs.shutdown()
    }
}

/// CGOBitmap is a CGO-compatible version of BitmapEvent that we pass back to Go.
/// BitmapEvent is a video output update from the server.
#[repr(C)]
pub struct CGOBitmap {
    pub dest_left: u16,
    pub dest_top: u16,
    pub dest_right: u16,
    pub dest_bottom: u16,
    /// The memory of this field is managed by the Rust side.
    pub data_ptr: *mut u8,
    pub data_len: usize,
    pub data_cap: usize,
}

impl TryFrom<BitmapEvent> for CGOBitmap {
    type Error = RdpError;

    fn try_from(e: BitmapEvent) -> Result<Self, Self::Error> {
        let mut res = CGOBitmap {
            dest_left: e.dest_left,
            dest_top: e.dest_top,
            dest_right: e.dest_right,
            dest_bottom: e.dest_bottom,
            data_ptr: ptr::null_mut(),
            data_len: 0,
            data_cap: 0,
        };

        // e.decompress consumes e, so we need to call it separately, after populating the fields
        // above.
        let mut data = if e.is_compress {
            e.decompress()?
        } else {
            e.data
        };
        res.data_ptr = data.as_mut_ptr();
        res.data_len = data.len();
        res.data_cap = data.capacity();

        // Prevent the data field from being freed while Go handles it.
        // It will be dropped once CGOBitmap is dropped (see below).
        mem::forget(data);

        Ok(res)
    }
}

impl Drop for CGOBitmap {
    fn drop(&mut self) {
        // Reconstruct into Vec to drop the allocated buffer.
        unsafe {
            Vec::from_raw_parts(self.data_ptr, self.data_len, self.data_cap);
        }
    }
}

#[cfg(unix)]
fn wait_for_fd(fd: usize) -> bool {
    unsafe {
        let mut raw_fds: fd_set = mem::zeroed();

        FD_SET(fd as i32, &mut raw_fds);

        let result = select(
            fd as i32 + 1,
            &mut raw_fds,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        );
        result == 1
    }
}

/// `update_clipboard` is called from Go, and caches data that was copied
/// client-side while notifying the RDP server that new clipboard data is available.
///
/// # Safety
///
/// client_ptr MUST be a valid pointer.
/// (validity defined by https://doc.rust-lang.org/nightly/core/primitive.pointer.html#method.as_ref-1)
///
/// data MUST be a valid pointer.
/// (validity defined by the validity of data in https://doc.rust-lang.org/std/slice/fn.from_raw_parts_mut.html)
#[no_mangle]
pub unsafe extern "C" fn update_clipboard(
    client_ptr: *mut Client,
    data: *mut u8,
    len: u32,
) -> CGOErrCode {
    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };
    let data = from_go_array(data, len);
    let mut lock = client.rdp_client.lock().unwrap();

    match lock.cliprdr {
        Some(ref mut clip) => match clip
            .update_clipboard(String::from_utf8_lossy(&data).into_owned())
        {
            Ok(messages) => {
                for message in messages {
                    if let Err(e) = lock.mcs.write(&cliprdr::CHANNEL_NAME.to_string(), message) {
                        error!("failed writing cliprdr format list: {:?}", e);
                        return CGOErrCode::ErrCodeFailure;
                    }
                }
                CGOErrCode::ErrCodeSuccess
            }
            Err(e) => {
                error!("failed updating clipboard: {:?}", e);
                CGOErrCode::ErrCodeFailure
            }
        },
        None => CGOErrCode::ErrCodeSuccess,
    }
}

/// handle_tdp_sd_announce announces a new drive that's ready to be
/// redirected over RDP.
///
///
/// # Safety
///
/// client_ptr MUST be a valid pointer.
/// (validity defined by https://doc.rust-lang.org/nightly/core/primitive.pointer.html#method.as_ref-1)
///
/// sd_announce.name MUST be a non-null pointer to a C-style null terminated string.
#[no_mangle]
pub unsafe extern "C" fn handle_tdp_sd_announce(
    client_ptr: *mut Client,
    sd_announce: CGOSharedDirectoryAnnounce,
) -> CGOErrCode {
    let sd_announce = SharedDirectoryAnnounce::from(sd_announce);

    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };

    let new_drive =
        rdpdr::ClientDeviceListAnnounce::new_drive(sd_announce.directory_id, sd_announce.name);

    let mut rdp_client = client.rdp_client.lock().unwrap();
    match rdp_client.write_client_device_list_announce(new_drive) {
        Ok(()) => CGOErrCode::ErrCodeSuccess,
        Err(e) => {
            error!("failed to announce new drive: {:?}", e);
            CGOErrCode::ErrCodeFailure
        }
    }
}

/// handle_tdp_sd_info_response handles a TDP Shared Directory Info Response
/// message
///
/// # Safety
///
/// client_ptr MUST be a valid pointer.
/// (validity defined by https://doc.rust-lang.org/nightly/core/primitive.pointer.html#method.as_ref-1)
///
/// res.fso.path MUST be a non-null pointer to a C-style null terminated string.
#[no_mangle]
pub unsafe extern "C" fn handle_tdp_sd_info_response(
    client_ptr: *mut Client,
    res: CGOSharedDirectoryInfoResponse,
) -> CGOErrCode {
    let res = SharedDirectoryInfoResponse::from(res);

    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };

    let mut rdp_client = client.rdp_client.lock().unwrap();
    match rdp_client.handle_tdp_sd_info_response(res) {
        Ok(()) => CGOErrCode::ErrCodeSuccess,
        Err(e) => {
            error!("failed to handle Shared Directory Info Response: {:?}", e);
            CGOErrCode::ErrCodeFailure
        }
    }
}

/// handle_tdp_sd_create_response handles a TDP Shared Directory Create Response
/// message
///
/// # Safety
///
/// client_ptr MUST be a valid pointer.
/// (validity defined by https://doc.rust-lang.org/nightly/core/primitive.pointer.html#method.as_ref-1)
#[no_mangle]
pub unsafe extern "C" fn handle_tdp_sd_create_response(
    client_ptr: *mut Client,
    res: CGOSharedDirectoryCreateResponse,
) -> CGOErrCode {
    let res = SharedDirectoryCreateResponse::from(res);

    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };

    let mut rdp_client = client.rdp_client.lock().unwrap();
    match rdp_client.handle_tdp_sd_create_response(res) {
        Ok(()) => CGOErrCode::ErrCodeSuccess,
        Err(e) => {
            error!("failed to handle Shared Directory Create Response: {:?}", e);
            CGOErrCode::ErrCodeFailure
        }
    }
}

/// handle_tdp_sd_delete_response handles a TDP Shared Directory Delete Response
/// message
///
/// # Safety
///
/// client_ptr MUST be a valid pointer.
/// (validity defined by https://doc.rust-lang.org/nightly/core/primitive.pointer.html#method.as_ref-1)
#[no_mangle]
pub unsafe extern "C" fn handle_tdp_sd_delete_response(
    client_ptr: *mut Client,
    res: CGOSharedDirectoryDeleteResponse,
) -> CGOErrCode {
    let res: SharedDirectoryDeleteResponse = res;

    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };

    let mut rdp_client = client.rdp_client.lock().unwrap();
    match rdp_client.handle_tdp_sd_delete_response(res) {
        Ok(()) => CGOErrCode::ErrCodeSuccess,
        Err(e) => {
            error!("failed to handle Shared Directory Create Response: {:?}", e);
            CGOErrCode::ErrCodeFailure
        }
    }
}

/// handle_tdp_sd_list_response handles a TDP Shared Directory List Response message.
///
/// # Safety
///
/// client_ptr MUST be a valid pointer.
/// (validity defined by https://doc.rust-lang.org/nightly/core/primitive.pointer.html#method.as_ref-1)
///
/// res.fso_list MUST be a valid pointer
/// (validity defined by the validity of data in https://doc.rust-lang.org/std/slice/fn.from_raw_parts_mut.html)
///
/// each res.fso_list[i].path MUST be a non-null pointer to a C-style null terminated string.
#[no_mangle]
pub unsafe extern "C" fn handle_tdp_sd_list_response(
    client_ptr: *mut Client,
    res: CGOSharedDirectoryListResponse,
) -> CGOErrCode {
    let res = SharedDirectoryListResponse::from(res);

    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };

    let mut rdp_client = client.rdp_client.lock().unwrap();
    match rdp_client.handle_tdp_sd_list_response(res) {
        Ok(()) => CGOErrCode::ErrCodeSuccess,
        Err(e) => {
            error!("failed to handle Shared Directory List Response: {:?}", e);
            CGOErrCode::ErrCodeFailure
        }
    }
}

/// handle_tdp_sd_read_response handles a TDP Shared Directory Read Response
/// message
///
/// # Safety
///
/// client_ptr must be a valid pointer
#[no_mangle]
pub unsafe extern "C" fn handle_tdp_sd_read_response(
    client_ptr: *mut Client,
    res: CGOSharedDirectoryReadResponse,
) -> CGOErrCode {
    let res = SharedDirectoryReadResponse::from(res);

    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };

    let mut rdp_client = client.rdp_client.lock().unwrap();
    match rdp_client.handle_tdp_sd_read_response(res) {
        Ok(()) => CGOErrCode::ErrCodeSuccess,
        Err(e) => {
            error!("failed to handle Shared Directory Read Response: {:?}", e);
            CGOErrCode::ErrCodeFailure
        }
    }
}

/// handle_tdp_sd_write_response handles a TDP Shared Directory Write Response
/// message
///
/// # Safety
///
/// client_ptr must be a valid pointer
#[no_mangle]
pub unsafe extern "C" fn handle_tdp_sd_write_response(
    client_ptr: *mut Client,
    res: CGOSharedDirectoryWriteResponse,
) -> CGOErrCode {
    let res: SharedDirectoryWriteResponse = res;

    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };

    let mut rdp_client = client.rdp_client.lock().unwrap();

    match rdp_client.handle_tdp_sd_write_response(res) {
        Ok(()) => CGOErrCode::ErrCodeSuccess,
        Err(e) => {
            error!("failed to handle Shared Directory Write Response: {:?}", e);
            CGOErrCode::ErrCodeFailure
        }
    }
}

/// handle_tdp_sd_move_response handles a TDP Shared Directory Move Response
/// message
///
/// # Safety
///
/// client_ptr MUST be a valid pointer.
/// (validity defined by https://doc.rust-lang.org/nightly/core/primitive.pointer.html#method.as_ref-1)
#[no_mangle]
pub unsafe extern "C" fn handle_tdp_sd_move_response(
    client_ptr: *mut Client,
    res: CGOSharedDirectoryMoveResponse,
) -> CGOErrCode {
    let res: SharedDirectoryMoveResponse = res;

    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };

    let mut rdp_client = client.rdp_client.lock().unwrap();
    match rdp_client.handle_tdp_sd_move_response(res) {
        Ok(()) => CGOErrCode::ErrCodeSuccess,
        Err(e) => {
            error!("failed to handle Shared Directory Move Response: {:?}", e);
            CGOErrCode::ErrCodeFailure
        }
    }
}

/// `read_rdp_output` reads incoming RDP bitmap frames from client at client_ref and forwards them to
/// handle_bitmap.
///
/// # Safety
///
/// `client_ptr` must be a valid pointer to a Client.
/// `handle_bitmap` *must not* free the memory of CGOBitmap.
#[no_mangle]
pub unsafe extern "C" fn read_rdp_output(client_ptr: *mut Client) -> CGOErrCode {
    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };
    if let Some(err) = read_rdp_output_inner(client) {
        error!("{}", err);
        CGOErrCode::ErrCodeFailure
    } else {
        CGOErrCode::ErrCodeSuccess
    }
}

fn read_rdp_output_inner(client: &Client) -> Option<String> {
    let tcp_fd = client.tcp_fd;
    let client_ref = client.go_ref;

    // Read incoming events.
    //
    // Wait for some data to be available on the TCP socket FD before consuming it. This prevents
    // us from locking the mutex in Client permanently while no data is available.
    while wait_for_fd(tcp_fd as usize) {
        let mut err = CGOErrCode::ErrCodeSuccess;
        let res = client.rdp_client.lock().unwrap().read(|rdp_event| {
            // This callback can be called multiple times per rdp_client.read()
            // (if multiple messages were received since the last call). Therefore,
            // we check that the previous call to handle_bitmap succeeded, so we don't
            // have a situation where handle_bitmap fails repeatedly and creates a
            // bunch of repetitive error messages in the logs. If it fails once,
            // we assume the connection is broken and stop trying to send bitmaps.
            if err == CGOErrCode::ErrCodeSuccess {
                match rdp_event {
                    RdpEvent::Bitmap(bitmap) => {
                        let mut cbitmap = match CGOBitmap::try_from(bitmap) {
                            Ok(cb) => cb,
                            Err(e) => {
                                error!(
                                    "failed to convert RDP bitmap to CGO representation: {:?}",
                                    e
                                );
                                return;
                            }
                        };
                        unsafe {
                            err = handle_bitmap(client_ref, &mut cbitmap) as CGOErrCode;
                        };
                    }
                    // No other events should be sent by the server to us.
                    _ => {
                        debug!("got unexpected pointer event from RDP server, ignoring");
                    }
                }
            }
        });
        match res {
            Err(RdpError::Io(io_err)) if io_err.kind() == ErrorKind::UnexpectedEof => return None,
            Err(e) => {
                return Some(format!("RDP read failed: {:?}", e));
            }
            _ => {}
        }
        if err != CGOErrCode::ErrCodeSuccess {
            return Some("failed forwarding RDP bitmap frame".to_string());
        }
    }
    None
}

/// CGOMousePointerEvent is a CGO-compatible version of PointerEvent that we pass back to Go.
/// PointerEvent is a mouse move or click update from the user.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct CGOMousePointerEvent {
    pub x: u16,
    pub y: u16,
    pub button: CGOPointerButton,
    pub down: bool,
    pub wheel: CGOPointerWheel,
    pub wheel_delta: i16,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub enum CGOPointerButton {
    PointerButtonNone,
    PointerButtonLeft,
    PointerButtonRight,
    PointerButtonMiddle,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub enum CGOPointerWheel {
    PointerWheelNone,
    PointerWheelVertical,
    PointerWheelHorizontal,
}

impl From<CGOMousePointerEvent> for PointerEvent {
    fn from(p: CGOMousePointerEvent) -> PointerEvent {
        // # Safety
        //
        // This function MUST NOT hang on to any of the pointers passed in to it after it returns.
        // In other words, all pointer data that needs to persist after this function returns MUST
        // be copied into Rust-owned memory.
        PointerEvent {
            x: p.x,
            y: p.y,
            button: match p.button {
                CGOPointerButton::PointerButtonNone => PointerButton::None,
                CGOPointerButton::PointerButtonLeft => PointerButton::Left,
                CGOPointerButton::PointerButtonRight => PointerButton::Right,
                CGOPointerButton::PointerButtonMiddle => PointerButton::Middle,
            },
            down: p.down,
            wheel: match p.wheel {
                CGOPointerWheel::PointerWheelNone => PointerWheel::None,
                CGOPointerWheel::PointerWheelVertical => PointerWheel::Vertical,
                CGOPointerWheel::PointerWheelHorizontal => PointerWheel::Horizontal,
            },
            wheel_delta: p.wheel_delta,
        }
    }
}

/// # Safety
///
/// client_ptr MUST be a valid pointer.
/// (validity defined by https://doc.rust-lang.org/nightly/core/primitive.pointer.html#method.as_ref-1)
#[no_mangle]
pub unsafe extern "C" fn write_rdp_pointer(
    client_ptr: *mut Client,
    pointer: CGOMousePointerEvent,
) -> CGOErrCode {
    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };
    let res = client
        .rdp_client
        .lock()
        .unwrap()
        .write(RdpEvent::Pointer(pointer.into()));

    if let Err(e) = res {
        error!("failed writing RDP pointer event: {:?}", e);
        CGOErrCode::ErrCodeFailure
    } else {
        CGOErrCode::ErrCodeSuccess
    }
}

/// CGOKeyboardEvent is a CGO-compatible version of KeyboardEvent that we pass back to Go.
/// KeyboardEvent is a keyboard update from the user.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct CGOKeyboardEvent {
    // Note: there's only one key code sent at a time. A key combo is sent as a sequence of
    // KeyboardEvent messages, one key at a time in the "down" state. The RDP server takes care of
    // interpreting those.
    pub code: u16,
    pub down: bool,
}

impl From<CGOKeyboardEvent> for KeyboardEvent {
    fn from(k: CGOKeyboardEvent) -> KeyboardEvent {
        // # Safety
        //
        // This function MUST NOT hang on to any of the pointers passed in to it after it returns.
        // In other words, all pointer data that needs to persist after this function returns MUST
        // be copied into Rust-owned memory.
        KeyboardEvent {
            code: k.code,
            down: k.down,
        }
    }
}

/// # Safety
///
/// client_ptr MUST be a valid pointer.
/// (validity defined by https://doc.rust-lang.org/nightly/core/primitive.pointer.html#method.as_ref-1)
#[no_mangle]
pub unsafe extern "C" fn write_rdp_keyboard(
    client_ptr: *mut Client,
    key: CGOKeyboardEvent,
) -> CGOErrCode {
    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };
    let res = client
        .rdp_client
        .lock()
        .unwrap()
        .write(RdpEvent::Key(key.into()));
    if let Err(e) = res {
        error!("failed writing RDP keyboard event: {:?}", e);
        CGOErrCode::ErrCodeFailure
    } else {
        CGOErrCode::ErrCodeSuccess
    }
}

/// # Safety
///
/// client_ptr must be a valid pointer to a Client.
#[no_mangle]
pub unsafe extern "C" fn close_rdp(client_ptr: *mut Client) -> CGOErrCode {
    let client = match Client::from_ptr(client_ptr) {
        Ok(client) => client,
        Err(cgo_error) => {
            return cgo_error;
        }
    };

    let res = match client.rdp_client.lock().unwrap().shutdown() {
        Err(_) => CGOErrCode::ErrCodeFailure,
        Ok(_) => CGOErrCode::ErrCodeSuccess,
    };

    if let Err(err) = client.tcp.tcp.shutdown(net::Shutdown::Both) {
        error!("failed shutting down TCP socket: {:?}", err);
        return CGOErrCode::ErrCodeFailure;
    }

    res
}

/// free_rdp lets the Go side inform us when it's done with Client and it can be dropped.
///
/// # Safety
///
/// client_ptr MUST be a valid pointer.
/// (validity defined by https://doc.rust-lang.org/nightly/core/primitive.pointer.html#method.as_ref-1)
#[no_mangle]
pub unsafe extern "C" fn free_rdp(client_ptr: *mut Client) {
    drop(Client::from_raw(client_ptr))
}

/// # Safety
///
/// s must be a C-style null terminated string.
/// s is cloned here, and the caller is responsible for
/// ensuring its memory is freed.
unsafe fn from_go_string(s: *const c_char) -> String {
    // # Safety
    //
    // This function MUST NOT hang on to any of the pointers passed in to it after it returns.
    // In other words, all pointer data that needs to persist after this function returns MUST
    // be copied into Rust-owned memory.
    CStr::from_ptr(s).to_string_lossy().into_owned()
}

/// # Safety
///
/// See https://doc.rust-lang.org/std/slice/fn.from_raw_parts_mut.html
unsafe fn from_go_array<T: Clone>(data: *mut T, len: u32) -> Vec<T> {
    // # Safety
    //
    // This function MUST NOT hang on to any of the pointers passed in to it after it returns.
    // In other words, all pointer data that needs to persist after this function returns MUST
    // be copied into Rust-owned memory.
    slice::from_raw_parts(data, len as usize).to_vec()
}

#[repr(C)]
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum CGOErrCode {
    ErrCodeSuccess = 0,
    ErrCodeFailure = 1,
}

#[repr(C)]
pub struct CGOSharedDirectoryAnnounce {
    pub directory_id: u32,
    pub name: *const c_char,
}

/// SharedDirectoryAnnounce is sent by the TDP client to the server
/// to announce a new directory to be shared over TDP.
pub struct SharedDirectoryAnnounce {
    directory_id: u32,
    name: String,
}

impl From<CGOSharedDirectoryAnnounce> for SharedDirectoryAnnounce {
    fn from(cgo: CGOSharedDirectoryAnnounce) -> SharedDirectoryAnnounce {
        // # Safety
        //
        // This function MUST NOT hang on to any of the pointers passed in to it after it returns.
        // In other words, all pointer data that needs to persist after this function returns MUST
        // be copied into Rust-owned memory.
        unsafe {
            SharedDirectoryAnnounce {
                directory_id: cgo.directory_id,
                name: from_go_string(cgo.name),
            }
        }
    }
}

/// SharedDirectoryAcknowledge is sent by the TDP server to the client
/// to acknowledge that a SharedDirectoryAnnounce was received.
#[derive(Debug)]
#[repr(C)]
pub struct SharedDirectoryAcknowledge {
    pub err_code: TdpErrCode,
    pub directory_id: u32,
}

pub type CGOSharedDirectoryAcknowledge = SharedDirectoryAcknowledge;

/// SharedDirectoryInfoRequest is sent from the TDP server to the client
/// to request information about a file or directory at a given path.
#[derive(Debug)]
pub struct SharedDirectoryInfoRequest {
    completion_id: u32,
    directory_id: u32,
    path: UnixPath,
}

#[repr(C)]
pub struct CGOSharedDirectoryInfoRequest {
    pub completion_id: u32,
    pub directory_id: u32,
    pub path: *const c_char,
}

impl From<ServerCreateDriveRequest> for SharedDirectoryInfoRequest {
    fn from(req: ServerCreateDriveRequest) -> SharedDirectoryInfoRequest {
        SharedDirectoryInfoRequest {
            completion_id: req.device_io_request.completion_id,
            directory_id: req.device_io_request.device_id,
            path: UnixPath::from(&req.path),
        }
    }
}

/// SharedDirectoryInfoResponse is sent by the TDP client to the server
/// in response to a `Shared Directory Info Request`.
#[derive(Debug)]
pub struct SharedDirectoryInfoResponse {
    completion_id: u32,
    err_code: TdpErrCode,
    fso: FileSystemObject,
}

#[repr(C)]
pub struct CGOSharedDirectoryInfoResponse {
    pub completion_id: u32,
    pub err_code: TdpErrCode,
    pub fso: CGOFileSystemObject,
}

impl From<CGOSharedDirectoryInfoResponse> for SharedDirectoryInfoResponse {
    fn from(cgo_res: CGOSharedDirectoryInfoResponse) -> SharedDirectoryInfoResponse {
        // # Safety
        //
        // This function MUST NOT hang on to any of the pointers passed in to it after it returns.
        // In other words, all pointer data that needs to persist after this function returns MUST
        // be copied into Rust-owned memory.
        SharedDirectoryInfoResponse {
            completion_id: cgo_res.completion_id,
            err_code: cgo_res.err_code,
            fso: FileSystemObject::from(cgo_res.fso),
        }
    }
}

#[derive(Debug, Clone)]
/// FileSystemObject is a TDP structure containing the metadata
/// of a file or directory.
pub struct FileSystemObject {
    last_modified: u64,
    size: u64,
    file_type: FileType,
    path: UnixPath,
}

impl FileSystemObject {
    fn name(&self) -> RdpResult<String> {
        if let Some(name) = self.path.last() {
            Ok(name.to_string())
        } else {
            Err(try_error(&format!(
                "failed to extract name from path: {:?}",
                self.path
            )))
        }
    }
}

#[repr(C)]
#[derive(Clone)]
pub struct CGOFileSystemObject {
    pub last_modified: u64,
    pub size: u64,
    pub file_type: FileType,
    pub path: *const c_char,
}

impl From<CGOFileSystemObject> for FileSystemObject {
    fn from(cgo_fso: CGOFileSystemObject) -> FileSystemObject {
        // # Safety
        //
        // This function MUST NOT hang on to any of the pointers passed in to it after it returns.
        // In other words, all pointer data that needs to persist after this function returns MUST
        // be copied into Rust-owned memory.
        unsafe {
            FileSystemObject {
                last_modified: cgo_fso.last_modified,
                size: cgo_fso.size,
                file_type: cgo_fso.file_type,
                path: UnixPath::from(from_go_string(cgo_fso.path)),
            }
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum FileType {
    File = 0,
    Directory = 1,
}

#[repr(C)]
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum TdpErrCode {
    /// nil (no error, operation succeeded)
    Nil = 0,
    /// operation failed
    Failed = 1,
    /// resource does not exist
    DoesNotExist = 2,
    /// resource already exists
    AlreadyExists = 3,
}

/// SharedDirectoryWriteRequest is sent by the TDP server to the client
/// to write to a file.
#[derive(Clone)]
pub struct SharedDirectoryWriteRequest {
    completion_id: u32,
    directory_id: u32,
    offset: u64,
    path: UnixPath,
    write_data: Vec<u8>,
}

impl std::fmt::Debug for SharedDirectoryWriteRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedDirectoryWriteRequest")
            .field("completion_id", &self.completion_id)
            .field("directory_id", &self.directory_id)
            .field("offset", &self.offset)
            .field("path", &self.path)
            .field("write_data", &util::vec_u8_debug(&self.write_data))
            .finish()
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct CGOSharedDirectoryWriteRequest {
    pub completion_id: u32,
    pub directory_id: u32,
    pub offset: u64,
    pub path_length: u32,
    pub path: *const c_char,
    pub write_data_length: u32,
    pub write_data: *mut u8,
}

/// SharedDirectoryReadRequest is sent by the TDP server to the client
/// to request the contents of a file.
#[derive(Debug)]
pub struct SharedDirectoryReadRequest {
    completion_id: u32,
    directory_id: u32,
    path: UnixPath,
    offset: u64,
    length: u32,
}

#[repr(C)]
pub struct CGOSharedDirectoryReadRequest {
    pub completion_id: u32,
    pub directory_id: u32,
    pub path_length: u32,
    pub path: *const c_char,
    pub offset: u64,
    pub length: u32,
}

/// SharedDirectoryReadResponse is sent by the TDP client to the server
/// with the data as requested by a SharedDirectoryReadRequest.
#[repr(C)]
pub struct SharedDirectoryReadResponse {
    pub completion_id: u32,
    pub err_code: TdpErrCode,
    pub read_data: Vec<u8>,
}

impl std::fmt::Debug for SharedDirectoryReadResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedDirectoryReadResponse")
            .field("completion_id", &self.completion_id)
            .field("err_code", &self.err_code)
            .field("read_data", &util::vec_u8_debug(&self.read_data))
            .finish()
    }
}

impl From<CGOSharedDirectoryReadResponse> for SharedDirectoryReadResponse {
    fn from(cgo_response: CGOSharedDirectoryReadResponse) -> SharedDirectoryReadResponse {
        unsafe {
            SharedDirectoryReadResponse {
                completion_id: cgo_response.completion_id,
                err_code: cgo_response.err_code,
                read_data: from_go_array(cgo_response.read_data, cgo_response.read_data_length),
            }
        }
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct CGOSharedDirectoryReadResponse {
    pub completion_id: u32,
    pub err_code: TdpErrCode,
    pub read_data_length: u32,
    pub read_data: *mut u8,
}

/// SharedDirectoryWriteResponse is sent by the TDP client to the server
/// to acknowledge the completion of a SharedDirectoryWriteRequest.
#[derive(Debug)]
#[repr(C)]
pub struct SharedDirectoryWriteResponse {
    pub completion_id: u32,
    pub err_code: TdpErrCode,
    pub bytes_written: u32,
}

pub type CGOSharedDirectoryWriteResponse = SharedDirectoryWriteResponse;

/// SharedDirectoryCreateRequest is sent by the TDP server to
/// the client to request the creation of a new file or directory.
#[derive(Debug)]
pub struct SharedDirectoryCreateRequest {
    completion_id: u32,
    directory_id: u32,
    file_type: FileType,
    path: UnixPath,
}

#[repr(C)]
pub struct CGOSharedDirectoryCreateRequest {
    pub completion_id: u32,
    pub directory_id: u32,
    pub file_type: FileType,
    pub path: *const c_char,
}

/// SharedDirectoryListResponse is sent by the TDP client to the server
/// in response to a SharedDirectoryInfoRequest.
#[derive(Debug)]
pub struct SharedDirectoryListResponse {
    completion_id: u32,
    err_code: TdpErrCode,
    fso_list: Vec<FileSystemObject>,
}

impl From<CGOSharedDirectoryListResponse> for SharedDirectoryListResponse {
    fn from(cgo: CGOSharedDirectoryListResponse) -> SharedDirectoryListResponse {
        // # Safety
        //
        // This function MUST NOT hang on to any of the pointers passed in to it after it returns.
        // In other words, all pointer data that needs to persist after this function returns MUST
        // be copied into Rust-owned memory.
        unsafe {
            let cgo_fso_list = from_go_array(cgo.fso_list, cgo.fso_list_length);
            let mut fso_list = vec![];
            for cgo_fso in cgo_fso_list.into_iter() {
                fso_list.push(FileSystemObject::from(cgo_fso));
            }

            SharedDirectoryListResponse {
                completion_id: cgo.completion_id,
                err_code: cgo.err_code,
                fso_list,
            }
        }
    }
}

#[repr(C)]
pub struct CGOSharedDirectoryListResponse {
    completion_id: u32,
    err_code: TdpErrCode,
    fso_list_length: u32,
    fso_list: *mut CGOFileSystemObject,
}

/// SharedDirectoryMoveRequest is sent from the TDP server to the client
/// to request a file at original_path be moved to new_path.
#[derive(Debug)]
pub struct SharedDirectoryMoveRequest {
    completion_id: u32,
    directory_id: u32,
    original_path: UnixPath,
    new_path: UnixPath,
}

#[repr(C)]
pub struct CGOSharedDirectoryMoveRequest {
    pub completion_id: u32,
    pub directory_id: u32,
    pub original_path: *const c_char,
    pub new_path: *const c_char,
}

/// SharedDirectoryCreateResponse is sent by the TDP client to the server
/// to acknowledge a SharedDirectoryCreateRequest was received and executed.
#[derive(Debug)]
pub struct SharedDirectoryCreateResponse {
    completion_id: u32,
    err_code: TdpErrCode,
    fso: FileSystemObject,
}

#[repr(C)]
pub struct CGOSharedDirectoryCreateResponse {
    pub completion_id: u32,
    pub err_code: TdpErrCode,
    pub fso: CGOFileSystemObject,
}

impl From<CGOSharedDirectoryCreateResponse> for SharedDirectoryCreateResponse {
    fn from(cgo_res: CGOSharedDirectoryCreateResponse) -> SharedDirectoryCreateResponse {
        // # Safety
        //
        // This function MUST NOT hang on to any of the pointers passed in to it after it returns.
        // In other words, all pointer data that needs to persist after this function returns MUST
        // be copied into Rust-owned memory.
        SharedDirectoryCreateResponse {
            completion_id: cgo_res.completion_id,
            err_code: cgo_res.err_code,
            fso: FileSystemObject::from(cgo_res.fso),
        }
    }
}

/// SharedDirectoryDeleteRequest is sent by the TDP server to the client
/// to request the deletion of a file or directory at path.
#[derive(Debug)]
pub struct SharedDirectoryDeleteRequest {
    completion_id: u32,
    directory_id: u32,
    path: UnixPath,
}

#[repr(C)]
pub struct CGOSharedDirectoryDeleteRequest {
    pub completion_id: u32,
    pub directory_id: u32,
    pub path: *const c_char,
}

/// SharedDirectoryDeleteResponse is sent by the TDP client to the server
/// to acknowledge a SharedDirectoryDeleteRequest was received and executed.
#[derive(Debug)]
#[repr(C)]
pub struct SharedDirectoryDeleteResponse {
    completion_id: u32,
    err_code: TdpErrCode,
}

pub type CGOSharedDirectoryDeleteResponse = SharedDirectoryDeleteResponse;

/// SharedDirectoryMoveResponse is sent by the TDP client to the server
/// to acknowledge a SharedDirectoryMoveRequest was received and expected.
#[derive(Debug)]
#[repr(C)]
pub struct SharedDirectoryMoveResponse {
    completion_id: u32,
    err_code: TdpErrCode,
}

pub type CGOSharedDirectoryMoveResponse = SharedDirectoryMoveResponse;

/// SharedDirectoryListRequest is sent by the TDP server to the client
/// to request the contents of a directory.
#[derive(Debug)]
pub struct SharedDirectoryListRequest {
    completion_id: u32,
    directory_id: u32,
    path: UnixPath,
}

#[repr(C)]
pub struct CGOSharedDirectoryListRequest {
    pub completion_id: u32,
    pub directory_id: u32,
    pub path: *const c_char,
}

// These functions are defined on the Go side. Look for functions with '//export funcname'
// comments.
extern "C" {
    fn handle_bitmap(client_ref: usize, b: *mut CGOBitmap) -> CGOErrCode;
    fn handle_remote_copy(client_ref: usize, data: *mut u8, len: u32) -> CGOErrCode;

    fn tdp_sd_acknowledge(client_ref: usize, ack: *mut CGOSharedDirectoryAcknowledge)
        -> CGOErrCode;
    fn tdp_sd_info_request(
        client_ref: usize,
        req: *mut CGOSharedDirectoryInfoRequest,
    ) -> CGOErrCode;
    fn tdp_sd_create_request(
        client_ref: usize,
        req: *mut CGOSharedDirectoryCreateRequest,
    ) -> CGOErrCode;
    fn tdp_sd_delete_request(
        client_ref: usize,
        req: *mut CGOSharedDirectoryDeleteRequest,
    ) -> CGOErrCode;
    fn tdp_sd_list_request(
        client_ref: usize,
        req: *mut CGOSharedDirectoryListRequest,
    ) -> CGOErrCode;
    fn tdp_sd_read_request(
        client_ref: usize,
        req: *mut CGOSharedDirectoryReadRequest,
    ) -> CGOErrCode;
    fn tdp_sd_write_request(
        client_ref: usize,
        req: *mut CGOSharedDirectoryWriteRequest,
    ) -> CGOErrCode;
    fn tdp_sd_move_request(
        client_ref: usize,
        req: *mut CGOSharedDirectoryMoveRequest,
    ) -> CGOErrCode;
}

/// Payload is a generic type used to represent raw incoming RDP messages for parsing.
pub(crate) type Payload = Cursor<Vec<u8>>;
