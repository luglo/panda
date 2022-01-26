/// PANDABEGINCOMMENT
///
///  Authors:
///  Luke Craig                  luke.craig@ll.mit.edu
///
/// This work is licensed under the terms of the GNU GPL, version 2.
/// See the COPYING file in the top-level directory.
///
/// PANDAENDCOMMENT
///
/// DESCRIPTION:
///
/// This file is the PANDA plugin interactions: init, uninit, and callbacks.
///
/// It also contains the callback inserted for TCG instrumentation.
///
use panda::prelude::*;
use panda::sys::panda_do_flush_tb;

#[macro_use]
extern crate lazy_static;

mod hook_manager;

mod api;
use api::HMANAGER;

extern "C" fn middle_filter(cpu: &mut CPUState, tb: &mut TranslationBlock) {
    let mut manager = HMANAGER.lock().unwrap();
    manager.run_tb(cpu, tb);
}

#[panda::before_tcg_codegen]
pub fn tcg_codegen(cpu: &mut CPUState, tb: &mut TranslationBlock) {
    let mut manager = HMANAGER.lock().unwrap();
    manager.clear_tbs(cpu, tb);
    manager.insert_on_matches(cpu, tb);
}

#[panda::before_block_exec_invalidate_opt]
pub fn bbeio(cpu: &mut CPUState, tb: &mut TranslationBlock) -> bool {
    let mut manager = HMANAGER.lock().unwrap();
    if manager.tb_needs_retranslated(tb) {
        true
    } else {
        manager.clear_tbs(cpu, tb);
        false
    }
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
