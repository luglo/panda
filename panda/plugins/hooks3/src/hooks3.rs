use panda::prelude::*;
use panda::sys::panda_do_flush_tb;

#[macro_use]
extern crate lazy_static;

mod hook_manager;

mod api;
use api::HMANAGER;

/// Hook callback interactions

/// this function is what is inserted by before_tcg_codegen
extern "C" fn middle_filter(cpu: &mut CPUState, tb: &mut TranslationBlock) {
    let mut manager = HMANAGER.lock().unwrap();
    manager.run_tb(cpu, tb);
}

/// This function determines if a tcg instruction must be inserted
#[panda::before_tcg_codegen]
pub fn tcg_codegen(cpu: &mut CPUState, tb: &mut TranslationBlock) {
    let mut manager = HMANAGER.lock().unwrap();
    manager.clear_tbs(cpu);
    manager.insert_on_matches(cpu, tb);
}

#[panda::init]
pub fn init(_: &mut PluginHandle) -> bool {
    tcg_codegen::disable();
    true
}

#[panda::uninit]
pub fn exit(_: &mut PluginHandle) {
    unsafe {
        panda_do_flush_tb();
    }
}
