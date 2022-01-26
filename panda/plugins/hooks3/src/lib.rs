mod hook_manager;
use hook_manager::{FnCb, Hook, HookManager};

use panda::prelude::*;
use panda::sys::{get_cpu, panda_do_flush_tb};
use panda::Callback;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

extern "C" {
    fn qemu_in_vcpu_thread() -> bool;
    fn panda_do_exit_cpu();
}

#[macro_use]
extern crate lazy_static;
type PluginReg = u32;
static PLUGIN_REG_NUM: AtomicU32 = AtomicU32::new(0);

lazy_static! {
    static ref BEFORE_TCG_CODEGEN_CB: Mutex<Callback> = Mutex::new(Callback::new());
    static ref HMANAGER: Mutex<HookManager> = Mutex::new(HookManager::new());
}

/// Essential C API

#[no_mangle]
pub extern "C" fn register_plugin() -> PluginReg {
    println!("registering plugin");
    PLUGIN_REG_NUM.fetch_add(1, Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn unregister_plugin(num: PluginReg) {
    let mut manager = HMANAGER.lock().unwrap();
    manager.remove_plugin(num);
    if !manager.has_hooks() {
        BEFORE_TCG_CODEGEN_CB.lock().unwrap().disable();
    }
}

#[no_mangle]
pub extern "C" fn add_hook(
    num: PluginReg,
    pc: target_ulong,
    asid: target_ulong,
    always_starts_block: bool,
    fun: FnCb,
) {
    println!("add_hook");

    let mut manager = HMANAGER.lock().unwrap();
    let h: Hook = Hook {
        pc,
        asid: match asid {
            0 => None,
            p => Some(p),
        },
        cb: fun,
        always_starts_block,
        plugin_num: num,
    };
    manager.add(&h);

    if manager.has_hooks() {
        BEFORE_TCG_CODEGEN_CB.lock().unwrap().enable();
    }
    drop(manager);
    // if we're in the vCPU thread exit without exception
    unsafe {
        let cpu = &mut *get_cpu();
        let vcpu_thread = qemu_in_vcpu_thread();
        if vcpu_thread && cpu.running {
            // check that the PC has changed to avoid infinite loops
            panda_do_exit_cpu();
        }
    }
}

/// Hook callback interactions

/// this function is what is inserted by before_tcg_codegen
extern "C" fn middle_filter(cpu: &mut CPUState, tb: &mut TranslationBlock) {
    println!("middle filter");
    let mut manager = HMANAGER.lock().unwrap();
    manager.run_tb(cpu, tb);
}

/// This function determines if a tcg instruction must be inserted
pub fn tcg_codegen(cpu: &mut CPUState, tb: &mut TranslationBlock) {
    let mut manager = HMANAGER.lock().unwrap();
    manager.clear_tbs(cpu);
    manager.insert_on_matches(cpu, tb);
}

#[panda::init]
pub fn init(_: &mut PluginHandle) -> bool {
    println!("asdf");
    let cb = BEFORE_TCG_CODEGEN_CB.lock().unwrap();
    cb.before_tcg_codegen(tcg_codegen);
    cb.disable();

    true
}

#[panda::uninit]
pub fn exit(_: &mut PluginHandle) {
    unsafe {
        panda_do_flush_tb();
    }
}
