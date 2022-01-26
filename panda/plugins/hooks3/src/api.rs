use crate::hook_manager::{FnCb, Hook, HookManager};
use panda::prelude::*;
use panda::sys::get_cpu;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

pub(crate) type PluginReg = u32;
static PLUGIN_REG_NUM: AtomicU32 = AtomicU32::new(0);
lazy_static! {
    pub(crate) static ref HMANAGER: Mutex<HookManager> = Mutex::new(HookManager::new());
}

extern "C" {
    fn qemu_in_vcpu_thread() -> bool;
    fn panda_do_exit_cpu();
}

#[no_mangle]
pub extern "C" fn register_plugin() -> PluginReg {
    PLUGIN_REG_NUM.fetch_add(1, Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn unregister_plugin(num: PluginReg) {
    let mut manager = HMANAGER.lock().unwrap();
    manager.remove_plugin(num);
}

#[no_mangle]
pub extern "C" fn add_hook(
    num: PluginReg,
    pc: target_ulong,
    asid: target_ulong,
    always_starts_block: bool,
    fun: FnCb,
) {
    let mut manager = HMANAGER.lock().unwrap();

    manager.add(&Hook {
        pc,
        asid: match asid {
            0 => None,
            p => Some(p),
        },
        cb: fun as u64,
        always_starts_block,
        plugin_num: num,
    });

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
