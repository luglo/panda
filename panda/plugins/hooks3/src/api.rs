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
/// This file contains a C-compatible API for hooks3.
///
use crate::bbeio;
use crate::hook_manager::{rust_tb_jmp_cache_hash_func, FnCb, Hook, HookManager};
use panda::sys::{get_cpu, tb_phys_invalidate, QemuMutex, TCGContext, TransactionAction};
use panda::{current_pc, prelude::*};
use std::arch::asm;
use std::sync::atomic::{AtomicU32, Ordering};

extern "C" {
    fn qemu_in_vcpu_thread() -> bool;
    fn panda_do_exit_cpu();
    fn cpu_io_recompile(cpu: *mut CPUState, retaddr: *mut usize) -> !;
    static have_tb_lock: i32;
    fn qemu_mutex_trylock(mutex: &QemuMutex) -> i32;
    fn qemu_mutex_unlock(mutex: &QemuMutex) -> i32;
    static tcg_ctx: TCGContext;
}

lazy_static! {
    pub(crate) static ref HMANAGER: HookManager = HookManager::new();
}

pub(crate) type PluginReg = u32;
static PLUGIN_REG_NUM: AtomicU32 = AtomicU32::new(1);

#[no_mangle]
pub extern "C" fn register_plugin() -> PluginReg {
    PLUGIN_REG_NUM.fetch_add(1, Ordering::SeqCst)
}

#[no_mangle]
pub extern "C" fn unregister_plugin(num: PluginReg) {
    HMANAGER.remove_plugin(num);
}

pub fn eval_jmp_list_val(cpu: &mut CPUState, pc: target_ulong, val: usize) -> bool {
    let vdir = val as usize & 2;
    let tb = vdir as *mut TranslationBlock;
    if !tb.is_null() {
        if vdir == 2 || vdir == 3 {
            false
        } else {
            pc_in_tb(cpu, pc, tb)
        }
    } else {
        false
    }
}

pub fn pc_in_tb(cpu: &mut CPUState, pc: target_ulong, tb: *mut TranslationBlock) -> bool {
    // println!("tb {:x}", tb as usize);
    unsafe {
        if tb.is_null() {
            false
        } else {
            if (*tb).pc <= pc && pc < (*tb).pc + (*tb).size as u64 {
                // println!("returning true for pc_in_tb");
                true
            } else {
                eval_jmp_list_val(cpu, pc, (*tb).jmp_list_next[0])
                    || eval_jmp_list_val(cpu, pc, (*tb).jmp_list_next[1])
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn add_hook3(
    num: PluginReg,
    pc: target_ulong,
    asid: target_ulong,
    always_starts_block: bool,
    fun: FnCb,
) {
    // TODO: Consider returning hash value of hook to plugin to
    // uniquely identify it so it can be removed with the same
    // value. Alternatively, use a UID

    if HMANAGER.add(&Hook {
        pc,
        asid: match asid {
            0 => None,
            p => Some(p),
        },
        cb: fun as u64,
        always_starts_block,
        plugin_num: num,
    }) && !HMANAGER.pc_instrumented(pc)
    {
        unsafe {
            let cpu = &mut *get_cpu();
            let vcpu_thread = qemu_in_vcpu_thread();
            if vcpu_thread && cpu.running {
                // if we can't get it we're in a TCG thread so we should
                // get it at btc. If past it bbeio should get it.
                if qemu_mutex_trylock(&tcg_ctx.tb_ctx.tb_lock) == 0 {
                    let current_pc = current_pc(cpu);
                    let index = rust_tb_jmp_cache_hash_func(current_pc);
                    let tb = cpu.tb_jmp_cache[index as usize];
                    if tb.is_null() {
                        // println!("have_tb_lock {:?}", have_tb_lock);
                        // asm!("int3");
                    }
                    if pc_in_tb(cpu, pc, tb) {
                        // println!("doing fancy stuff");
                        tb_phys_invalidate(tb, u64::MAX);
                        panda_do_exit_cpu();
                        // asm!("int3");
                    }
                    qemu_mutex_unlock(&tcg_ctx.tb_ctx.tb_lock);
                } else {
                    bbeio::enable();
                }
            }
        }
    }
}

// #[no_mangle]
// see note above
// pub extern "C" fn remove_hook(num: HookReg) {}
