// Copyright 2026 Jyotiraditya Panda <jyotiraditya@aospa.co>
// SPDX-License-Identifier: Apache-2.0

//! Android 10 (API 29) service manager client.
//!
//! The Android 10 service manager is a C binary (not an AIDL service). Its
//! wire protocol differs from API 30+ in two key ways:
//!
//!   1. No stability `i32` after `flat_binder_object` in binder parcels.
//!   2. No `INTERFACE_TRANSACTION` support - the descriptor is empty.
//!   3. `LIST_SERVICES` iterates one name at a time using an index argument
//!      rather than returning the full list in a single reply.
//!
//! The `sdk_at_least(30)` runtime guard in `parcelable.rs` and `parcel.rs`
//! handles points 1 and 2 globally, so this module only needs to implement
//! the transaction-level differences.

use crate::binder_object::{flat_binder_object, raw_pointer_to_strong_binder};
use crate::sys::{BINDER_TYPE_BINDER, BINDER_TYPE_HANDLE};
use crate::*;

// These mirror the DUMP_FLAG_PRIORITY_* constants from Android's
// IServiceManager.h, reproduced here because Android 10 does not expose
// them through AIDL-generated code.
pub const DUMP_FLAG_PRIORITY_CRITICAL: i32 = 1 << 0;
pub const DUMP_FLAG_PRIORITY_HIGH: i32 = 1 << 1;
pub const DUMP_FLAG_PRIORITY_NORMAL: i32 = 1 << 2;
pub const DUMP_FLAG_PRIORITY_DEFAULT: i32 = 1 << 3;
pub const DUMP_FLAG_PRIORITY_ALL: i32 = 0x0f;
pub const DUMP_FLAG_PROTO: i32 = 1 << 4;

// Transaction codes from Android 10 service_manager.c (SVC_MGR_*).
const GET_SERVICE: TransactionCode = FIRST_CALL_TRANSACTION;
const CHECK_SERVICE: TransactionCode = FIRST_CALL_TRANSACTION + 1;
const ADD_SERVICE: TransactionCode = FIRST_CALL_TRANSACTION + 2;
const LIST_SERVICES: TransactionCode = FIRST_CALL_TRANSACTION + 3;

/// Client proxy for the Android 10 C service manager.
///
/// This is a hand-written proxy rather than an AIDL-generated one because the
/// Android 10 service manager is a C binary with a simpler, non-AIDL wire
/// format.
pub struct BpServiceManager {
    binder: SIBinder,
}

impl BpServiceManager {
    /// Construct a proxy from the context object returned by
    /// `ProcessState::context_object()`.
    ///
    /// Unlike AIDL-generated proxies, this does **not** verify the interface
    /// descriptor: the Android 10 C service manager does not implement
    /// `INTERFACE_TRANSACTION`, so the descriptor would always be an empty
    /// string rather than `"android.os.IServiceManager"`.
    pub fn from_binder(binder: SIBinder) -> Option<Self> {
        if binder.as_proxy().is_some() {
            Some(Self { binder })
        } else {
            None
        }
    }

    fn proxy(&self) -> &proxy::ProxyHandle {
        self.binder
            .as_proxy()
            .expect("BpServiceManager must wrap a proxy binder")
    }

    fn transact(&self, code: TransactionCode, data: &Parcel) -> Result<Parcel> {
        self.proxy()
            .submit_transact(code, data, FLAG_CLEAR_BUF)?
            .ok_or(StatusCode::UnexpectedNull)
    }
}

/// Read an optional binder from a reply parcel using the Android 10 wire format.
///
/// Unlike API 30+, there is no stability `i32` after the `flat_binder_object`.
/// The stability field is intentionally absent here; the global
/// `sdk_at_least(30)` guard in `parcelable.rs` handles this at the
/// `SIBinder` deserialization layer when called from the normal path. This
/// function reads the object directly so it can handle the pre-stability
/// format without triggering extra reads.
fn read_nullable_binder(reply: &mut Parcel) -> Result<Option<SIBinder>> {
    if reply.data_avail() < std::mem::size_of::<flat_binder_object>() {
        return Ok(None);
    }

    let obj = reply.read_object(false)?;

    match obj.header_type() {
        BINDER_TYPE_HANDLE => {
            // Resolve the handle through the process-wide proxy cache.
            Ok(Some(
                ProcessState::as_self().strong_proxy_for_handle(obj.handle())?,
            ))
        }

        BINDER_TYPE_BINDER => {
            let ptr = obj.pointer();
            if ptr == 0 {
                Ok(None)
            } else {
                let strong = raw_pointer_to_strong_binder((ptr, obj.cookie()));
                Ok(Some(SIBinder::clone(&strong)))
            }
        }

        _ => Ok(None),
    }
}

/// Retrieve an existing service, blocking briefly if it does not yet exist.
pub fn get_service(sm: &BpServiceManager, name: &str) -> Option<SIBinder> {
    let result = (|| -> Result<Option<SIBinder>> {
        let mut data = sm.proxy().prepare_transact(true)?;
        data.write(name)?;

        let mut reply = sm.transact(GET_SERVICE, &data)?;
        read_nullable_binder(&mut reply)
    })();

    match result {
        Ok(binder) => binder,
        Err(err) => {
            log::error!("Failed to get service {name}: {err:?}");
            None
        }
    }
}

/// Retrieve an existing service without blocking. Returns `None` if absent.
pub fn check_service(sm: &BpServiceManager, name: &str) -> Option<SIBinder> {
    let result = (|| -> Result<Option<SIBinder>> {
        let mut data = sm.proxy().prepare_transact(true)?;
        data.write(name)?;

        let mut reply = sm.transact(CHECK_SERVICE, &data)?;
        read_nullable_binder(&mut reply)
    })();

    match result {
        Ok(binder) => binder,
        Err(err) => {
            log::error!("Failed to check service {name}: {err}");
            None
        }
    }
}

/// Register a service with the Android 10 service manager.
///
/// Writes the binder as a raw `flat_binder_object` without the stability
/// `i32` that API 30+ appends. The global `sdk_at_least(30)` guard in
/// `parcelable.rs` already suppresses the stability field when running on
/// API 29, but we write the object directly here for clarity and to avoid
/// relying on that implicit behaviour.
pub fn add_service(
    sm: &BpServiceManager,
    identifier: &str,
    binder: SIBinder,
) -> std::result::Result<(), Status> {
    let result = (|| -> Result<()> {
        let mut data = sm.proxy().prepare_transact(true)?;
        data.write(identifier)?;

        // Write the flat object directly - no stability int32 on API 29.
        let flat: flat_binder_object = (&binder).into();
        data.write_object(&flat, false)?;
        data.write::<i32>(&0)?; // allowIsolated = false
        data.write::<i32>(&DUMP_FLAG_PRIORITY_DEFAULT)?;

        let mut reply = sm.transact(ADD_SERVICE, &data)?;
        let status = reply.read::<Status>()?;
        if !status.is_ok() {
            return Err(StatusCode::from(status));
        }

        Ok(())
    })();

    result.map_err(Status::from)
}

/// Return a list of all currently running services.
///
/// The Android 10 service manager has no bulk-list transaction. This
/// function iterates by sending `LIST_SERVICES` with an incrementing index
/// until the transaction fails (indicating no more entries).
pub fn list_services(sm: &BpServiceManager, dump_priority: i32) -> Vec<String> {
    let mut services = Vec::new();
    let mut n: i32 = 0;

    loop {
        let result = (|| -> Result<String> {
            let mut data = sm.proxy().prepare_transact(true)?;
            data.write::<i32>(&n)?;
            data.write::<i32>(&dump_priority)?;

            sm.transact(LIST_SERVICES, &data)?.read::<String>()
        })();

        match result {
            Ok(name) => {
                services.push(name);
                n += 1;
            }
            Err(_) => break,
        }
    }

    services
}

/// Retrieve an existing service and attempt to cast it to the specified
/// interface type.
pub fn get_interface<T: FromIBinder + ?Sized>(
    sm: &BpServiceManager,
    name: &str,
) -> Result<Strong<T>> {
    match get_service(sm, name) {
        Some(service) => FromIBinder::try_from(service),
        None => {
            log::error!("Service {name} not found");
            Err(StatusCode::NameNotFound)
        }
    }
}
