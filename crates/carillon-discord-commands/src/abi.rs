//! The byte-exchange ABI expected by `carillon-wasm`: the provider writes the
//! invocation into `input`, calls `invoke`, and reads the result from
//! `output`; the module reaches native operations through `carillon.call`
//! and copies each reply with `carillon.take`.

use std::cell::RefCell;

use crate::{
    HostCalls,
    contract::{HostReply, HostRequest, Invocation, Outcome},
};
use carillon_protocol::models::ModelState;

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

#[expect(unsafe_code, reason = "wasm export")]
#[unsafe(no_mangle)]
pub extern "C" fn input(length: u32) -> u32 {
    INPUT.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        buffer.clear();
        buffer.resize(length as usize, 0);
        buffer.as_ptr() as u32
    })
}

#[expect(unsafe_code, reason = "wasm export")]
#[unsafe(no_mangle)]
pub extern "C" fn output() -> u32 {
    OUTPUT.with(|buffer| buffer.borrow().as_ptr() as u32)
}

#[expect(unsafe_code, reason = "wasm export")]
#[unsafe(no_mangle)]
pub extern "C" fn invoke() -> u32 {
    let outcome =
        INPUT.with(
            |buffer| match serde_json::from_slice::<Invocation>(&buffer.borrow()) {
                Ok(invocation) => crate::handle(&mut Provider, invocation),
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
struct Provider;

impl HostCalls for Provider {
    fn call(&mut self, request: HostRequest) -> Result<ModelState, String> {
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
            Ok(HostReply::ModelState(state)) => Ok(state),
            Ok(HostReply::Error(message)) => Err(message),
            Err(error) => Err(format!("invalid host reply: {error}")),
        }
    }
}
