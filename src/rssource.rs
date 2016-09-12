//  Copyright (C) 2016 Sebastian Dröge <sebastian@centricular.com>
//
//  This library is free software; you can redistribute it and/or
//  modify it under the terms of the GNU Library General Public
//  License as published by the Free Software Foundation; either
//  version 2 of the License, or (at your option) any later version.
//
//  This library is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
//  Library General Public License for more details.
//
//  You should have received a copy of the GNU Library General Public
//  License along with this library; if not, write to the
//  Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
//  Boston, MA 02110-1301, USA.

use libc::c_char;
use std::os::raw::c_void;
use std::ffi::{CStr, CString};
use std::ptr;
use std::u64;

use std::panic::{self, AssertUnwindSafe};

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use url::Url;

use utils::*;
use error::*;
use buffer::*;

#[derive(Debug)]
pub enum SourceError {
    Failure,
    OpenFailed,
    NotFound,
    ReadFailed,
    SeekFailed,
}

impl ToGError for SourceError {
    fn to_gerror(&self) -> (u32, i32) {
        match *self {
            SourceError::Failure => (gst_library_error_domain(), 1),
            SourceError::OpenFailed => (gst_resource_error_domain(), 5),
            SourceError::NotFound => (gst_resource_error_domain(), 3),
            SourceError::ReadFailed => (gst_resource_error_domain(), 9),
            SourceError::SeekFailed => (gst_resource_error_domain(), 11),
        }
    }
}

pub struct SourceWrapper {
    raw: *mut c_void,
    uri: Mutex<(Option<Url>, bool)>,
    uri_validator: Box<UriValidator>,
    source: Mutex<Box<Source>>,
    panicked: AtomicBool,
}

pub trait Source {
    fn uri_validator(&self) -> Box<UriValidator>;

    fn is_seekable(&self) -> bool;
    fn get_size(&self) -> Option<u64>;

    fn start(&mut self, uri: Url) -> Result<(), ErrorMessage>;
    fn stop(&mut self) -> Result<(), ErrorMessage>;
    fn fill(&mut self, offset: u64, length: u32, buffer: &mut Buffer) -> Result<(), FlowError>;
    fn seek(&mut self, start: u64, stop: Option<u64>) -> Result<(), ErrorMessage>;
}

impl SourceWrapper {
    fn new(raw: *mut c_void, source: Box<Source>) -> SourceWrapper {
        SourceWrapper {
            raw: raw,
            uri: Mutex::new((None, false)),
            uri_validator: source.uri_validator(),
            source: Mutex::new(source),
            panicked: AtomicBool::new(false),
        }
    }
}

#[no_mangle]
pub extern "C" fn source_new(source: *mut c_void,
                             create_instance: fn() -> Box<Source>)
                             -> *mut SourceWrapper {
    Box::into_raw(Box::new(SourceWrapper::new(source, create_instance())))
}

#[no_mangle]
pub unsafe extern "C" fn source_drop(ptr: *mut SourceWrapper) {
    Box::from_raw(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn source_set_uri(ptr: *const SourceWrapper,
                                        uri_ptr: *const c_char,
                                        cerr: *mut c_void)
                                        -> GBoolean {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        let uri_storage = &mut wrap.uri.lock().unwrap();

        if uri_storage.1 {
            UriError::new(UriErrorKind::BadState, Some("Already started".to_string()))
                .into_gerror(cerr);
            return GBoolean::False;
        }

        uri_storage.0 = None;
        if uri_ptr.is_null() {
            GBoolean::True
        } else {
            let uri_str = CStr::from_ptr(uri_ptr).to_str().unwrap();

            match Url::parse(uri_str) {
                Ok(uri) => {
                    if let Err(err) = (*wrap.uri_validator)(&uri) {
                        err.into_gerror(cerr);

                        GBoolean::False
                    } else {
                        uri_storage.0 = Some(uri);

                        GBoolean::True
                    }
                }
                Err(err) => {
                    UriError::new(UriErrorKind::BadUri,
                                  Some(format!("Failed to parse URI '{}': {}", uri_str, err)))
                        .into_gerror(cerr);

                    GBoolean::False
                }
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_get_uri(ptr: *const SourceWrapper) -> *mut c_char {
    let wrap: &SourceWrapper = &*ptr;
    panic_to_error!(wrap, ptr::null_mut(), {
        let uri_storage = &mut wrap.uri.lock().unwrap();

        match uri_storage.0 {
            Some(ref uri) => CString::new(uri.as_ref().as_bytes()).unwrap().into_raw(),
            None => ptr::null_mut(),
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_is_seekable(ptr: *const SourceWrapper) -> GBoolean {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        let source = &wrap.source.lock().unwrap();

        GBoolean::from_bool(source.is_seekable())
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_get_size(ptr: *const SourceWrapper) -> u64 {
    let wrap: &SourceWrapper = &*ptr;
    panic_to_error!(wrap, u64::MAX, {

        let source = &wrap.source.lock().unwrap();

        match source.get_size() {
            Some(size) => size,
            None => u64::MAX,
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_start(ptr: *const SourceWrapper) -> GBoolean {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        let source = &mut wrap.source.lock().unwrap();

        let uri = match *wrap.uri.lock().unwrap() {
            (Some(ref uri), ref mut started) => {
                *started = true;

                uri.clone()
            }
            (None, _) => {
                error_msg!(SourceError::OpenFailed, ["No URI given"]).post(wrap.raw);
                return GBoolean::False;
            }
        };

        match source.start(uri) {
            Ok(..) => GBoolean::True,
            Err(ref msg) => {
                wrap.uri.lock().unwrap().1 = false;
                msg.post(wrap.raw);
                GBoolean::False
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_stop(ptr: *const SourceWrapper) -> GBoolean {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        let source = &mut wrap.source.lock().unwrap();

        match source.stop() {
            Ok(..) => {
                wrap.uri.lock().unwrap().1 = false;
                GBoolean::True
            }
            Err(ref msg) => {
                msg.post(wrap.raw);
                GBoolean::False
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_fill(ptr: *const SourceWrapper,
                                     offset: u64,
                                     length: u32,
                                     buffer: ScopedBufferPtr)
                                     -> GstFlowReturn {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, GstFlowReturn::Error, {
        let source = &mut wrap.source.lock().unwrap();
        let mut buffer = ScopedBuffer::new(&buffer);

        match source.fill(offset, length, &mut buffer) {
            Ok(()) => GstFlowReturn::Ok,
            Err(flow_error) => {
                match flow_error {
                    FlowError::NotNegotiated(ref msg) |
                    FlowError::Error(ref msg) => msg.post(wrap.raw),
                    _ => (),
                }
                flow_error.to_native()
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn source_seek(ptr: *const SourceWrapper, start: u64, stop: u64) -> GBoolean {
    let wrap: &SourceWrapper = &*ptr;

    panic_to_error!(wrap, GBoolean::False, {
        let source = &mut wrap.source.lock().unwrap();

        match source.seek(start, if stop == u64::MAX { None } else { Some(stop) }) {
            Ok(..) => GBoolean::True,
            Err(ref msg) => {
                msg.post(wrap.raw);
                GBoolean::False
            }
        }
    })
}
