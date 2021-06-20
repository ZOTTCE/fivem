use std::ffi::CStr;

use anyhow::anyhow;
use cfx_wasm_rt_types::{call_result::CRITICAL_ERROR, ScrObject};

use wasmtime::*;
use wasmtime_wasi::{sync::WasiCtxBuilder, Wasi};

mod invoker;

pub type LogFunc = extern "C" fn(msg: *const i8);
pub type CanonicalizeRefFunc =
    extern "C" fn(ref_idx: u32, buffer: *mut i8, buffer_size: u32) -> i32;

static mut LOGGER: Option<LogFunc> = None;
static mut CANONICALIZE_REF: Option<CanonicalizeRefFunc> = None;

// expected exports from guests
const CFX_START: &str = "_start";
const CFX_ON_EVENT: &str = "__cfx_on_event";
const CFX_ON_TICK: &str = "__cfx_on_tick";
const CFX_CALL_REF: &str = "__cfx_call_ref";
const CFX_ALLOC: &str = "__cfx_alloc";
const CFX_FREE: &str = "__cfx_free";
const CFX_DUPLICATE_REF: &str = "__cfx_duplicate_ref";
const CFX_REMOVE_REF: &str = "__cfx_remove_ref";

// exports from the host
const HOST: &str = "host";
const HOST_LOG: &str = "log";
const HOST_INVOKE: &str = "invoke";
const HOST_CANONICALIZE_REF: &str = "canonicalize_ref";
const HOST_INVOKE_REF_FUNC: &str = "invoke_ref_func";

pub struct Runtime {
    engine: Engine,
    script: Option<ScriptModule>,
}

impl Runtime {
    pub fn new() -> Runtime {
        let mut config = Config::default();
        config.cranelift_opt_level(wasmtime::OptLevel::Speed);

        let engine = Engine::new(&config).unwrap();

        Runtime {
            engine,
            script: None,
        }
    }

    pub fn load_module(&mut self, bytes: &[u8], wasi: bool) -> anyhow::Result<()> {
        let script = if wasi {
            ScriptModule::new_with_wasi(&self.engine, bytes)?
        } else {
            ScriptModule::new(&self.engine, bytes)?
        };

        self.script = Some(script);

        if let Some(start) = self
            .script
            .as_ref()
            .and_then(|script| script.instance.get_func(CFX_START))
        {
            start.call(&[])?;
        }

        Ok(())
    }

    pub fn unload_module(&mut self) {
        self.script = None;
    }

    #[inline]
    pub fn trigger_event(
        &mut self,
        event_name: &CStr,
        args: &[u8],
        source: &CStr,
    ) -> anyhow::Result<()> {
        if let Some(script) = self.script.as_mut() {
            let mut wrapper = || -> anyhow::Result<()> {
                if let Some(func) = script.on_event.clone() {
                    let ev = script.copy_event_name(event_name)?;
                    let args = script.copy_event_args(args)?;
                    let src = script.copy_event_source(source)?;

                    // event, args, args_len, src
                    func.call(&[
                        Val::I32(ev.0 as _),
                        Val::I32(args.0 as _),
                        Val::I32(args.1 as _),
                        Val::I32(src.0 as _),
                    ])?;
                }

                Ok(())
            };

            if let Err(err) = wrapper() {
                self.script = None;
                script_log(format!("{} error: {:?}", CFX_ON_EVENT, err));

                return Err(err);
            }
        }

        Ok(())
    }

    pub fn tick(&mut self) -> anyhow::Result<()> {
        if let Some(func) = self
            .script
            .as_ref()
            .and_then(|script| script.instance.get_func(CFX_ON_TICK))
        {
            if let Err(err) = func.call(&[]) {
                self.script = None;
                script_log(format!("{} error: {:?}", CFX_ON_TICK, err));

                return Err(err);
            }
        }

        Ok(())
    }

    pub fn call_ref(
        &mut self,
        ref_idx: u32,
        args: &[u8],
        ret_buf: &mut Vec<u8>,
    ) -> anyhow::Result<u32> {
        if let Some(script) = self.script.as_ref() {
            match call_call_ref(script, ref_idx, args, ret_buf) {
                Err(err) => {
                    self.script = None;
                    script_log(format!("{} error: {:?}", CFX_CALL_REF, err));

                    return Err(err);
                }

                Ok(val) => return Ok(val),
            }
        }

        Ok(0)
    }

    pub fn duplicate_ref(&mut self, ref_idx: u32) -> u32 {
        if let Some(func) = self.script.as_ref().and_then(|script| {
            script
                .instance
                .get_typed_func::<i32, i32>(CFX_DUPLICATE_REF)
                .ok()
        }) {
            match func.call(ref_idx as _).map(|idx| idx as _) {
                Err(err) => {
                    self.script = None;
                    script_log(format!("{} error: {:?}", CFX_DUPLICATE_REF, err));
                }

                Ok(val) => return val,
            }
        }

        0
    }

    pub fn remove_ref(&mut self, ref_idx: u32) {
        if let Some(func) = self.script.as_ref().and_then(|script| {
            script
                .instance
                .get_typed_func::<i32, i32>(CFX_REMOVE_REF)
                .ok()
        }) {
            if let Err(err) = func.call(ref_idx as _) {
                self.script = None;
                script_log(format!("{} error: {:?}", CFX_REMOVE_REF, err));
            }
        }
    }

    pub fn memory_size(&self) -> u32 {
        self.script
            .as_ref()
            .and_then(|script| script.instance.get_memory("memory"))
            .map(|memory| memory.size())
            .unwrap_or(0)
    }
}

struct ScriptModule {
    store: Store,
    instance: Instance,
    on_event: Option<Func>,
    event_allocs: EventAlloc,
    memory: Memory,
}

#[derive(Default)]
struct EventAlloc {
    name: (u32, usize),
    args: (u32, usize),
    source: (u32, usize),
}

impl ScriptModule {
    fn new(engine: &Engine, bytes: &[u8]) -> anyhow::Result<ScriptModule> {
        let store = Store::new(&engine);
        let module = Module::new(engine, bytes)?;

        let instance = Instance::new(&store, &module, &[])?;
        let on_event = instance.get_func(CFX_ON_EVENT);
        let memory = instance.get_memory("memory").ok_or(anyhow!("No memory"))?;

        let mut module = ScriptModule {
            store,
            instance,
            on_event,
            memory,
            event_allocs: EventAlloc::default(),
        };

        module.make_startup_allocs()?;

        Ok(module)
    }

    fn new_with_wasi(engine: &Engine, bytes: &[u8]) -> anyhow::Result<ScriptModule> {
        let store = Store::new(&engine);
        let mut linker = Linker::new(&store);

        let wasi = Wasi::new(
            &store,
            WasiCtxBuilder::new()
                .inherit_stdout()
                .inherit_stdio()
                .inherit_stderr()
                .build(),
        );

        wasi.add_to_linker(&mut linker)?;

        linker.func(HOST, HOST_LOG, |caller: Caller, ptr: i32, len: i32| {
            log(caller, ptr, len);
        })?;

        linker.func(
            HOST,
            HOST_INVOKE,
            |caller: Caller, hash: u64, ptr: i32, len: i32, retval: i32| -> i32 {
                match crate::invoker::call_native_wrapper(caller, hash, ptr, len, retval) {
                    Ok(result) => result.into(),
                    Err(err) => {
                        script_log(format!("{}::{} error: {:?}", HOST, HOST_INVOKE, err));

                        CRITICAL_ERROR
                    }
                }
            },
        )?;

        linker.func(
            HOST,
            HOST_CANONICALIZE_REF,
            |caller: Caller, ref_idx: i32, ptr: i32, len: i32| {
                canonicalize_ref(caller, ref_idx, ptr, len).unwrap_or(0)
            },
        )?;

        linker.func(
            HOST,
            HOST_INVOKE_REF_FUNC,
            |caller: Caller,
             ref_name: i32,
             args: i32,
             len: i32,
             buffer: i32,
             buffer_cap: i32|
             -> i32 {
                match crate::invoker::invoke_ref_func_wrapper(
                    caller, ref_name, args, len, buffer, buffer_cap,
                ) {
                    Ok(result) => result.into(),
                    Err(err) => {
                        script_log(format!(
                            "{}::{} error: {:?}",
                            HOST, HOST_INVOKE_REF_FUNC, err
                        ));

                        CRITICAL_ERROR
                    }
                }
            },
        )?;

        let module = Module::new(engine, bytes)?;
        let instance = linker.instantiate(&module)?;
        let on_event = instance.get_func(CFX_ON_EVENT);
        let memory = instance.get_memory("memory").ok_or(anyhow!("No memory"))?;

        let mut module = ScriptModule {
            store,
            instance,
            on_event,
            memory,
            event_allocs: EventAlloc::default(),
        };

        module.make_startup_allocs()?;

        Ok(module)
    }

    #[inline]
    fn alloc_bytes(&self, bytes: &[u8]) -> anyhow::Result<(u32, usize)> {
        let malloc = self.instance.get_typed_func::<(i32, u32), u32>(CFX_ALLOC)?;

        let data_ptr = malloc.call((bytes.len() as _, 1))?;
        let mem = &self.memory;

        mem.write(data_ptr as _, bytes)?;

        Ok((data_ptr, bytes.len()))
    }

    #[inline]
    fn free_bytes(&self, (offset, length): (u32, usize)) -> anyhow::Result<()> {
        let free = self
            .instance
            .get_typed_func::<(u32, u32, u32), ()>(CFX_FREE)?;

        free.call((offset as _, length as _, 1))?;

        Ok(())
    }

    fn make_startup_allocs(&mut self) -> anyhow::Result<()> {
        self.event_allocs.name = self.alloc_bytes(&[0; 1024])?;
        self.event_allocs.args = self.alloc_bytes(&[0; 1 << 15])?;
        self.event_allocs.source = self.alloc_bytes(&[0; 1024])?;

        Ok(())
    }

    #[inline]
    fn copy_event_name(&mut self, event_name: &CStr) -> anyhow::Result<(u32, usize)> {
        let bytes = event_name.to_bytes_with_nul();
        let name = self.event_allocs.name;

        if name.0 == 0 || name.1 < bytes.len() {
            let new = self.alloc_bytes(bytes)?;

            if name.0 != 0 {
                self.free_bytes(name)?;
            }

            return Ok(new);
        }

        let mem = &self.memory;
        mem.write(name.0 as _, bytes)?;

        Ok((name.0, bytes.len()))
    }

    #[inline]
    fn copy_event_args(&mut self, event_args: &[u8]) -> anyhow::Result<(u32, usize)> {
        let bytes = event_args;
        let args = self.event_allocs.args;

        if args.0 == 0 || args.1 < bytes.len() {
            let new = self.alloc_bytes(bytes)?;

            if args.0 != 0 {
                self.free_bytes(args)?;
            }

            return Ok(new);
        }

        let mem = &self.memory;
        mem.write(args.0 as _, bytes)?;

        Ok((args.0, bytes.len()))
    }

    #[inline]
    fn copy_event_source(&mut self, event_source: &CStr) -> anyhow::Result<(u32, usize)> {
        let bytes = event_source.to_bytes_with_nul();
        let source = self.event_allocs.source;

        if source.0 == 0 || source.1 < bytes.len() {
            let new = self.alloc_bytes(bytes)?;

            if source.0 != 0 {
                self.free_bytes(source)?;
            }

            return Ok(new);
        }

        let mem = &self.memory;
        mem.write(source.0 as _, bytes)?;

        Ok((source.0, bytes.len()))
    }
}

pub fn set_logger(log: LogFunc) {
    unsafe {
        LOGGER = Some(log);
    }
}

pub fn set_native_invoke(invoke: extern "C" fn(ctx: *mut std::ffi::c_void) -> u32) {
    unsafe {
        crate::invoker::set_native_invoke(std::mem::transmute(invoke));
    }
}

pub fn set_canonicalize_ref(canonicalize_ref: CanonicalizeRefFunc) {
    unsafe {
        CANONICALIZE_REF = Some(canonicalize_ref);
    }
}

fn call_call_ref(
    script: &ScriptModule,
    ref_idx: u32,
    args: &[u8],
    ret_buf: &mut Vec<u8>,
) -> anyhow::Result<u32> {
    let memory = script
        .instance
        .get_memory("memory")
        .ok_or(anyhow!("No memory"))?;

    let cfx_call_ref = script
        .instance
        .get_typed_func::<(i32, i32, i32), i32>(CFX_CALL_REF)?;

    let args_guest = script.alloc_bytes(args)?;

    let scrobj = {
        let result = cfx_call_ref.call((ref_idx as _, args_guest.0 as _, args.len() as _));
        script.free_bytes(args_guest)?;

        result?
    };

    if scrobj == 0 {
        return Ok(0);
    }

    let scrobj = unsafe {
        let ptr = memory.data_ptr().add(scrobj as _) as *const ScrObject;
        &*ptr
    };

    unsafe {
        ret_buf.set_len(0);
    }

    if scrobj.data == 0 || scrobj.length == 0 {
        return Ok(0);
    }

    let slice = unsafe {
        let ptr = memory.data_ptr().add(scrobj.data as _);
        std::slice::from_raw_parts(ptr, scrobj.length as _)
    };

    ret_buf.extend_from_slice(slice);

    Ok(ret_buf.len() as _)
}

fn log(caller: Caller, ptr: i32, len: i32) -> anyhow::Result<()> {
    let mut buf = vec![0u8; len as usize];
    let mem = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or(anyhow!("No memory"))?;

    mem.read(ptr as _, buf.as_mut())?;

    unsafe {
        if let Some(logger) = LOGGER {
            logger(buf.as_mut_ptr() as _);
        }
    }

    Ok(())
}

fn canonicalize_ref(caller: Caller, ref_idx: i32, ptr: i32, len: i32) -> anyhow::Result<i32> {
    let mem = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or(anyhow!("No memory"))?;

    unsafe {
        let ptr = mem.data_ptr().add(ptr as _) as *mut _;

        if let Some(canonicalize_ref) = CANONICALIZE_REF {
            return Ok(canonicalize_ref(ref_idx as _, ptr, len as _));
        }
    }

    Err(anyhow!("Bad WASM runtime. No canonicalize_ref ..."))
}

pub(crate) fn script_log<T: AsRef<str>>(msg: T) {
    if let Some(log) = unsafe { LOGGER } {
        let cstr = std::ffi::CString::new(msg.as_ref()).unwrap();
        log(cstr.as_ptr());
    }
}

pub fn fx_succeeded(result: u32) -> bool {
    (result & 0x80000000) == 0
}
