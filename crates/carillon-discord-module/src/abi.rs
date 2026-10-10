//! The byte-exchange ABI expected by `carillon-wasm`: the provider writes the
//! invocation into `input`, calls `invoke`, and reads the result from
//! `output`; the module reaches native operations through `carillon.call`
//! and copies each reply with `carillon.take`. [`export!`](crate::export)
//! defines a module's entry points over these functions.

use std::cell::RefCell;

use crate::{
    HostCalls,
    contract::{HostReply, HostRequest, Invocation, Outcome},
};

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static EXCHANGE: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

#[expect(
    unsafe_code,
    reason = "wasm import declarations; the host validates every pointer and length"
)]
#[link(wasm_import_module = "carillon")]
unsafe extern "C" {
    safe fn call(pointer: u32, length: u32) -> u32;
    safe fn take(pointer: u32, length: u32);
}

/// Room for an invocation of `length` bytes, at the address returned.
#[must_use]
pub fn input(length: u32) -> u32 {
    INPUT.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        buffer.clear();
        buffer.resize(length as usize, 0);
        buffer.as_ptr() as u32
    })
}

/// The address of the last outcome.
#[must_use]
pub fn output() -> u32 {
    OUTPUT.with(|buffer| buffer.borrow().as_ptr() as u32)
}

/// Handles the invocation in `input` with `handle`, returning the length of
/// the outcome at `output`.
pub fn invoke(handle: impl FnOnce(&mut Provider, Invocation) -> Outcome) -> u32 {
    let outcome =
        INPUT.with(
            |buffer| match serde_json::from_slice::<Invocation>(&buffer.borrow()) {
                Ok(invocation) => handle(&mut Provider, invocation),
                Err(error) => Outcome::Failure(format!("invalid invocation: {error}")),
            },
        );
    OUTPUT.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        *buffer = serde_json::to_vec(&outcome).unwrap_or_default();
        u32::try_from(buffer.len()).unwrap_or(u32::MAX)
    })
}

/// Native operations performed by the provider process.
pub struct Provider;

impl HostCalls for Provider {
    fn call(&mut self, request: HostRequest) -> Result<HostReply, String> {
        let request = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        let length = call(
            request.as_ptr() as u32,
            u32::try_from(request.len()).unwrap_or(u32::MAX),
        );
        let reply = EXCHANGE.with(|buffer| {
            let mut buffer = buffer.borrow_mut();
            buffer.clear();
            buffer.resize(length as usize, 0);
            take(buffer.as_ptr() as u32, length);
            serde_json::from_slice::<HostReply>(&buffer)
        });
        match reply {
            Ok(HostReply::Error(message)) => Err(message),
            Ok(reply) => Ok(reply),
            Err(error) => Err(format!("invalid host reply: {error}")),
        }
    }
}
