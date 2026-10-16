#![forbid(unsafe_code)]
//! Bounded hosting of replaceable guest code for Carillon providers.
//!
//! A guest is a WebAssembly module speaking the byte-exchange ABI below; the
//! embedding provider defines what the bytes mean. Every invocation runs in a
//! fresh instance with its own memory, a memory ceiling, and a deadline, and
//! reaches native operations only through the single `carillon.call` import,
//! which the provider answers through its [`Native`] implementation. Nothing
//! survives between invocations inside the guest, so durable state stays
//! native.
//!
//! Guest ABI (lengths in bytes, pointers into the exported `memory`):
//!
//! - import `carillon.call(ptr, len) -> len`: send a request, learn the reply length
//! - import `carillon.take(ptr, len)`: copy the pending reply into guest memory
//! - export `input(len) -> ptr`: reserve space for the invocation payload
//! - export `invoke() -> len`: handle the payload, return the output length
//! - export `output() -> ptr`: locate the output

use std::{fmt, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use wasmtime::{
    Caller, Config, Engine, Linker, Memory, Store, StoreLimits, StoreLimitsBuilder, TypedFunc,
};

/// Ceiling for the invocation's single linear memory, not total process memory.
const MEMORY_LIMIT: usize = 16 << 20;
/// Instructions between forced yields, so a busy guest cannot starve the
/// deadline timer.
const YIELD_INTERVAL: u64 = 10_000;
const REQUIRED_EXPORTS: [&str; 4] = ["memory", "input", "invoke", "output"];

/// Provider-supplied native operations, invoked by the guest through
/// `carillon.call`. The provider decides authority and deadlines here; the
/// guest only asks.
pub trait Native: Send + 'static {
    fn call(&mut self, request: Vec<u8>) -> impl Future<Output = Vec<u8>> + Send;
}

/// A compiled module whose required exports were checked at load time.
#[derive(Clone)]
pub struct Module(wasmtime::Module);

/// Why an invocation produced no output.
#[derive(Debug)]
pub enum Failure {
    /// The guest, including its native calls, ran past the deadline.
    Deadline(Duration),
    /// The guest trapped, exhausted its memory, or broke the ABI.
    Guest(anyhow::Error),
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deadline(deadline) => write!(formatter, "guest exceeded {deadline:?}"),
            Self::Guest(error) => write!(formatter, "guest failed: {error:#}"),
        }
    }
}

impl std::error::Error for Failure {}

struct Slot<N> {
    native: N,
    pending: Vec<u8>,
    limits: StoreLimits,
}

/// An engine and import table shared by every invocation of one provider.
pub struct Host<N> {
    engine: Engine,
    linker: Linker<Slot<N>>,
}

impl<N: Native> Host<N> {
    /// # Errors
    ///
    /// Returns an error when the engine cannot be configured on this machine.
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config)?;
        let mut linker = Linker::new(&engine);
        linker.func_wrap_async(
            "carillon",
            "call",
            |mut caller: Caller<'_, Slot<N>>, (pointer, length): (u32, u32)| {
                Box::new(async move {
                    let memory = guest_memory(&mut caller)?;
                    let request = read_guest(&memory, &caller, pointer, length)?;
                    let response = caller.data_mut().native.call(request).await;
                    let length = u32::try_from(response.len())?;
                    caller.data_mut().pending = response;
                    wasmtime::Result::Ok(length)
                })
            },
        )?;
        linker.func_wrap(
            "carillon",
            "take",
            |mut caller: Caller<'_, Slot<N>>, pointer: u32, length: u32| {
                let memory = guest_memory(&mut caller)?;
                let pending = std::mem::take(&mut caller.data_mut().pending);
                if pending.len() != length as usize {
                    wasmtime::bail!(
                        "guest requested {length} bytes of a {} byte response",
                        pending.len()
                    );
                }
                memory.write(&mut caller, pointer as usize, &pending)?;
                wasmtime::Result::Ok(())
            },
        )?;
        Ok(Self { engine, linker })
    }

    /// Compile and validate a module without running it.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes are not a valid module or the module
    /// lacks a required export.
    pub fn load(&self, bytes: &[u8]) -> Result<Module> {
        let module = wasmtime::Module::new(&self.engine, bytes)
            .map_err(anyhow::Error::from)
            .context("compile guest module")?;
        for name in REQUIRED_EXPORTS {
            if module.get_export(name).is_none() {
                return Err(anyhow!("guest module lacks export {name}"));
            }
        }
        Ok(Module(module))
    }

    /// Run one invocation to completion or failure within `deadline`.
    ///
    /// # Errors
    ///
    /// Returns [`Failure::Deadline`] when the invocation is abandoned at the
    /// deadline and [`Failure::Guest`] when the guest traps or breaks the ABI.
    pub async fn invoke(
        &self,
        module: &Module,
        native: N,
        payload: &[u8],
        deadline: Duration,
    ) -> Result<Vec<u8>, Failure> {
        let slot = Slot {
            native,
            pending: Vec::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_LIMIT)
                .memories(1)
                .instances(1)
                .build(),
        };
        let mut store = Store::new(&self.engine, slot);
        store.limiter(|slot| &mut slot.limits);
        let run = async {
            store.set_fuel(u64::MAX)?;
            store.fuel_async_yield_interval(Some(YIELD_INTERVAL))?;
            let instance = self.linker.instantiate_async(&mut store, &module.0).await?;
            let memory = instance
                .get_memory(&mut store, "memory")
                .context("guest memory export")?;
            let input: TypedFunc<u32, u32> = instance.get_typed_func(&mut store, "input")?;
            let invoke: TypedFunc<(), u32> = instance.get_typed_func(&mut store, "invoke")?;
            let output: TypedFunc<(), u32> = instance.get_typed_func(&mut store, "output")?;
            let pointer = input
                .call_async(&mut store, u32::try_from(payload.len())?)
                .await?;
            memory.write(&mut store, pointer as usize, payload)?;
            let length = invoke.call_async(&mut store, ()).await?;
            let pointer = output.call_async(&mut store, ()).await?;
            Ok::<_, anyhow::Error>(read_guest(&memory, &store, pointer, length)?)
        };
        match tokio::time::timeout(deadline, run).await {
            Ok(result) => result.map_err(Failure::Guest),
            Err(_) => Err(Failure::Deadline(deadline)),
        }
    }
}

fn guest_memory<N>(caller: &mut Caller<'_, Slot<N>>) -> wasmtime::Result<Memory> {
    caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::format_err!("guest memory export"))
}

fn read_guest(
    memory: &Memory,
    store: impl wasmtime::AsContext,
    pointer: u32,
    length: u32,
) -> wasmtime::Result<Vec<u8>> {
    let end = (pointer as usize)
        .checked_add(length as usize)
        .filter(|end| *end <= memory.data_size(&store))
        .ok_or_else(|| {
            wasmtime::format_err!("guest named {length} bytes at {pointer} outside its memory")
        })?;
    Ok(memory.data(&store)[pointer as usize..end].to_vec())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use super::{Failure, Host, Native};

    /// Echoes the payload through the host and returns the host's reply.
    const ECHO: &str = r#"
(module
  (import "carillon" "call" (func $call (param i32 i32) (result i32)))
  (import "carillon" "take" (func $take (param i32 i32)))
  (memory (export "memory") 1)
  (global $length (mut i32) (i32.const 0))
  (func (export "input") (param $length i32) (result i32)
    (global.set $length (local.get $length))
    (i32.const 1024))
  (func (export "invoke") (result i32)
    (local $reply i32)
    (local.set $reply (call $call (i32.const 1024) (global.get $length)))
    (call $take (i32.const 8192) (local.get $reply))
    (local.get $reply))
  (func (export "output") (result i32) (i32.const 8192)))
"#;

    const SPIN: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "input") (param i32) (result i32) (i32.const 0))
  (func (export "invoke") (result i32) (loop $forever (br $forever)) (i32.const 0))
  (func (export "output") (result i32) (i32.const 0)))
"#;

    const TRAP: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "input") (param i32) (result i32) (i32.const 0))
  (func (export "invoke") (result i32) (unreachable))
  (func (export "output") (result i32) (i32.const 0)))
"#;

    const OVERSIZED_MEMORY: &str = r#"
(module
  (memory (export "memory") 300)
  (func (export "input") (param i32) (result i32) (i32.const 0))
  (func (export "invoke") (result i32) (i32.const 0))
  (func (export "output") (result i32) (i32.const 0)))
"#;

    const MULTIPLE_MEMORIES: &str = r#"
(module
  (memory (export "memory") 256)
  (memory 256)
  (func (export "input") (param i32) (result i32) (i32.const 0))
  (func (export "invoke") (result i32) (i32.const 0))
  (func (export "output") (result i32) (i32.const 0)))
"#;

    const OUT_OF_BOUNDS_OUTPUT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "input") (param i32) (result i32) (i32.const 0))
  (func (export "invoke") (result i32) (i32.const 70000))
  (func (export "output") (result i32) (i32.const 0)))
"#;

    #[derive(Clone, Default)]
    struct Recorder {
        requests: Arc<Mutex<Vec<Vec<u8>>>>,
        delay: Duration,
    }

    impl Native for Recorder {
        async fn call(&mut self, request: Vec<u8>) -> Vec<u8> {
            tokio::time::sleep(self.delay).await;
            let reply = format!("host saw {}", String::from_utf8_lossy(&request)).into_bytes();
            if let Ok(mut requests) = self.requests.lock() {
                requests.push(request);
            }
            reply
        }
    }

    fn host() -> anyhow::Result<Host<Recorder>> {
        Host::new()
    }

    #[tokio::test]
    async fn guest_exchanges_bytes_with_native_calls() -> anyhow::Result<()> {
        let host = host()?;
        let module = host.load(&wat::parse_str(ECHO)?)?;
        let recorder = Recorder::default();
        let output = host
            .invoke(&module, recorder.clone(), b"ping", Duration::from_secs(5))
            .await?;
        assert_eq!(output, b"host saw ping");
        assert_eq!(
            recorder
                .requests
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?[..],
            [b"ping".to_vec()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn busy_guests_and_slow_native_calls_stop_at_the_deadline() -> anyhow::Result<()> {
        let host = host()?;
        let spin = host.load(&wat::parse_str(SPIN)?)?;
        let started = Instant::now();
        let failure = host
            .invoke(&spin, Recorder::default(), b"", Duration::from_millis(200))
            .await;
        assert!(matches!(failure, Err(Failure::Deadline(_))), "{failure:?}");
        assert!(started.elapsed() < Duration::from_secs(2));

        let echo = host.load(&wat::parse_str(ECHO)?)?;
        let slow = Recorder {
            delay: Duration::from_secs(10),
            ..Recorder::default()
        };
        let started = Instant::now();
        let failure = host
            .invoke(&echo, slow, b"x", Duration::from_millis(200))
            .await;
        assert!(matches!(failure, Err(Failure::Deadline(_))), "{failure:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
        Ok(())
    }

    #[tokio::test]
    async fn traps_and_abi_violations_are_guest_failures() -> anyhow::Result<()> {
        let host = host()?;
        for source in [
            TRAP,
            OVERSIZED_MEMORY,
            MULTIPLE_MEMORIES,
            OUT_OF_BOUNDS_OUTPUT,
        ] {
            let module = host.load(&wat::parse_str(source)?)?;
            let failure = host
                .invoke(&module, Recorder::default(), b"", Duration::from_secs(5))
                .await;
            assert!(matches!(failure, Err(Failure::Guest(_))), "{failure:?}");
        }
        Ok(())
    }

    #[test]
    fn loading_rejects_invalid_modules_and_missing_exports() -> anyhow::Result<()> {
        let host = host()?;
        let refusal = |bytes: &[u8]| host.load(bytes).err().map(|error| error.to_string());
        let echo = wat::parse_str(ECHO)?;
        for invalid in [&b"not wasm"[..], &echo[..echo.len() / 2]] {
            assert_eq!(refusal(invalid).as_deref(), Some("compile guest module"));
        }
        assert_eq!(
            refusal(&wat::parse_str("(module (memory (export \"memory\") 1))")?).as_deref(),
            Some("guest module lacks export input")
        );
        Ok(())
    }
}
