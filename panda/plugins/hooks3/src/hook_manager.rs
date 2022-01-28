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
/// This file does most of the state management for the hooks plugin.
///
/// The vast majority of the logic is implemented in the hook manager.
///
use crate::api::PluginReg;
use crate::{middle_filter, tcg_codegen};
use std::borrow::BorrowMut;
use std::cmp::{Ord, Ordering};
use std::collections::{BTreeSet, HashSet};
use std::ffi::c_void;
use std::ops::Bound::Included;
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, RwLock};

use panda::current_asid;
use panda::prelude::{target_ulong, CPUState, TranslationBlock};
use panda::sys::{tb_phys_invalidate, TCGOp, TARGET_PAGE_BITS};

// middle callback type
pub(crate) type MCB = extern "C" fn(&mut CPUState, &mut TranslationBlock);
// check_cpu_exit callback type
pub(crate) type CCE = unsafe extern "C" fn(*mut c_void);
// hooks callback type
pub(crate) type FnCb = extern "C" fn(&mut CPUState, &mut TranslationBlock, &Hook) -> bool;
// wrapper function
pub(crate) type WFN =
    unsafe extern "C" fn(fn(*mut c_void, *mut c_void), a1: *mut c_void, a2: *mut c_void);

extern "C" {
    fn find_first_guest_insn() -> *mut TCGOp;
    fn find_guest_insn_by_addr(pc: target_ulong) -> *mut TCGOp;
    fn insert_call_1p(after_op: *mut *mut TCGOp, fun: CCE, cpu: *mut c_void);
    fn call_2p_check_cpu_exit(f: fn(*mut c_void, *mut c_void), a1: *mut c_void, a2: *mut c_void);
    #[allow(improper_ctypes)]
    fn insert_call_3p(
        after_op: *mut *mut TCGOp,
        wrapper_fn: WFN,
        fun: MCB,
        cpu: &mut CPUState,
        tb: &mut TranslationBlock,
    );
    fn check_cpu_exit(none: *mut c_void);
}
const TB_JMP_CACHE_BITS: u32 = 12;
const TB_JMP_PAGE_BITS: u32 = TB_JMP_CACHE_BITS / 2;
const TB_JMP_PAGE_SIZE: u32 = 1 << TB_JMP_PAGE_BITS;
const TB_JMP_ADDR_MASK: u32 = TB_JMP_PAGE_SIZE - 1;
const TB_JMP_CACHE_SIZE: u32 = 1 << TB_JMP_CACHE_BITS;
const TB_JMP_PAGE_MASK: u32 = TB_JMP_CACHE_SIZE - TB_JMP_PAGE_SIZE;

pub fn rust_tb_jmp_cache_hash_func(pc: target_ulong) -> u32 {
    let tmp = pc ^ (pc >> (TARGET_PAGE_BITS - TB_JMP_PAGE_BITS));
    (((tmp >> (TARGET_PAGE_BITS - TB_JMP_PAGE_BITS)) & TB_JMP_PAGE_MASK as target_ulong)
        | (tmp & TB_JMP_ADDR_MASK as target_ulong)) as u32
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct Hook {
    /// Represents the basic hook type.
    ///
    /// pc   -  program counter as virtual address
    /// asid -  optional value that represents the ASID to match to
    /// cb   -  Pointer to C function to call
    /// always_starts_block - guarantee that PC starts the TB
    pub pc: target_ulong,
    pub asid: Option<target_ulong>,
    pub plugin_num: PluginReg,
    pub cb: u64,
    pub always_starts_block: bool,
}

impl PartialOrd for Hook {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Hook {
    fn eq(&self, other: &Self) -> bool {
        if self.pc == other.pc && self.asid == other.asid && self.plugin_num == other.plugin_num {
            let a = self.cb as usize;
            a.cmp(&(other.cb as usize)) == Ordering::Equal
        } else {
            false
        }
    }
}

impl Eq for Hook {}

impl Ord for Hook {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.pc.cmp(&other.pc) {
            Ordering::Equal => match self.asid.cmp(&other.asid) {
                Ordering::Equal => match self.plugin_num.cmp(&other.plugin_num) {
                    Ordering::Equal => {
                        let a = self.cb as usize;
                        a.cmp(&(other.cb as usize))
                    }
                    a => a,
                },
                a => a,
            },
            a => a,
        }
    }

    fn max(self, other: Self) -> Self
    where
        Self: Sized,
    {
        std::cmp::max_by(self, other, Ord::cmp)
    }

    fn min(self, other: Self) -> Self
    where
        Self: Sized,
    {
        std::cmp::min_by(self, other, Ord::cmp)
    }
}

#[derive(Clone)]
pub struct HookManagerState {
    clear_full_tb: Vec<target_ulong>,
    clear_start_tb: Vec<target_ulong>,
}

pub struct HookManager {
    add_hooks: Mutex<Vec<Hook>>,
    hooks: RwLock<BTreeSet<Hook>>,
    instrumented_pcs: RwLock<HashSet<target_ulong>>,
    state: Mutex<HookManagerState>,
    last_retranslated_tb: AtomicU64,
}

impl HookManager {
    pub fn new() -> Self {
        Self {
            add_hooks: Mutex::new(Vec::new()),
            hooks: RwLock::new(BTreeSet::new()),
            instrumented_pcs: RwLock::new(HashSet::new()),
            state: Mutex::new(HookManagerState {
                clear_full_tb: Vec::new(),
                clear_start_tb: Vec::new(),
            }),
            last_retranslated_tb: AtomicU64::new(0),
        }
    }

    pub fn has_hooks(self: &Self) -> bool {
        let hooks = self.hooks.read().unwrap();
        return !hooks.is_empty();
    }

    pub fn add(self: &Self, h: &Hook) -> bool {
        if !self.has_hooks() {
            tcg_codegen::enable();
        }
        let hooks = self.hooks.read().unwrap();
        if !hooks.contains(h) {
            let mut add_hooks = self.add_hooks.lock().unwrap();
            if !add_hooks.contains(h) {
                add_hooks.push(*h);
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    fn new_hooks_add(self: &Self) {
        let mut add_hooks = self.add_hooks.lock().unwrap();
        if !add_hooks.is_empty() {
            let mut hooks = self.hooks.write().unwrap();
            let mut state = self.state.lock().unwrap();
            for &h in add_hooks.iter() {
                hooks.insert(h);
                if h.always_starts_block {
                    state.clear_start_tb.push(h.pc);
                } else {
                    state.clear_full_tb.push(h.pc);
                }
            }
            add_hooks.clear();
        }
    }

    fn clear_empty_hooks(self: &Self, matched_hooks: Vec<Hook>) {
        if !matched_hooks.is_empty() {
            let mut hooks = self.hooks.write().unwrap();
            for &elem in matched_hooks.iter() {
                hooks.remove(&elem);
            }
        }

        if !self.has_hooks() {
            tcg_codegen::disable();
        }
    }

    pub fn remove_plugin(self: &Self, num: PluginReg) {
        let hooks = self.hooks.read().unwrap();
        let mut matched_hooks = Vec::new();
        for &elem in hooks.iter() {
            if elem.plugin_num == num {
                matched_hooks.push(elem);
            }
        }
        drop(hooks);
        self.clear_empty_hooks(matched_hooks);
    }

    pub fn run_tb(self: &Self, cpu: &mut CPUState, tb: &mut TranslationBlock) {
        self.new_hooks_add();
        let pc_start = tb.pc;
        let pc_end = tb.pc + tb.size as u64;
        let asid = Some(current_asid(cpu));

        let low: Hook = Hook {
            pc: pc_start,
            asid: None,
            cb: u64::MIN,
            plugin_num: 0,
            always_starts_block: false,
        };
        let high: Hook = Hook {
            pc: pc_end,
            asid: Some(target_ulong::MAX),
            cb: u64::MAX,
            plugin_num: PluginReg::MAX,
            always_starts_block: true,
        };

        let mut matched_hooks = Vec::new();
        let hooks = self.hooks.read().unwrap();

        for &elem in hooks.range((Included(&low), Included(&high))) {
            if pc_start <= elem.pc && elem.pc < pc_end {
                if elem.asid == asid || elem.asid == None {
                    let cb = unsafe { std::mem::transmute::<u64, FnCb>(elem.cb) };
                    // if callback returns true remove from hooks
                    if (cb)(cpu, tb, &elem) {
                        matched_hooks.push(elem);
                    }
                }
            }
        }
        drop(hooks);
        self.clear_empty_hooks(matched_hooks);
    }

    pub fn insert_on_matches(self: &Self, cpu: &mut CPUState, tb: &mut TranslationBlock) {
        let pc_start = tb.pc;
        let pc_end = tb.pc + tb.size as u64;

        // make hooks to compare to. highest and lowest candidates
        let low: Hook = Hook {
            pc: pc_start,
            asid: None,
            cb: u64::MIN,
            plugin_num: 0,
            always_starts_block: false,
        };
        let high: Hook = Hook {
            pc: pc_end,
            asid: Some(target_ulong::MAX),
            cb: u64::MAX,
            plugin_num: PluginReg::MAX,
            always_starts_block: true,
        };
        let hooks = self.hooks.read().unwrap();

        // these are different hashsets.
        // matched_pcs are the pcs matched this round
        // instrumented_pcs are globally instrumented PCs.
        let mut matched_pcs = HashSet::new();

        // iterate over B-tree matches. Add matches to set to avoid duplicates
        for &elem in hooks.range((Included(&low), Included(&high))) {
            // add matches to set. avoid duplicates
            if matched_pcs.contains(&elem.pc) {
                continue;
            }

            // get op by technique based on guarantees
            let mut op = unsafe {
                if elem.always_starts_block {
                    find_first_guest_insn()
                } else {
                    find_guest_insn_by_addr(elem.pc)
                }
            };

            // check op and insert both middle filter and check_cpu_exit
            // so we can cpu_exit if need be.
            if !op.is_null() {
                println!("inserting call {:x}", elem.pc);
                unsafe {
                    insert_call_3p(&mut op, call_2p_check_cpu_exit, middle_filter, cpu, tb);
                }
            }
            matched_pcs.insert(elem.pc);
        }
        let mut instrumented_pcs = self.instrumented_pcs.write().unwrap();
        for pc in matched_pcs.iter() {
            instrumented_pcs.insert(*pc);
        }
    }

    pub fn tb_needs_retranslated(self: &Self, tb: &mut TranslationBlock) -> bool {
        self.new_hooks_add();
        let pc_start = tb.pc;
        let pc_end = tb.pc + tb.size as u64;

        // make hooks to compare to. highest and lowest candidates
        let low: Hook = Hook {
            pc: pc_start,
            asid: None,
            cb: u64::MIN,
            plugin_num: 0,
            always_starts_block: false,
        };
        let high: Hook = Hook {
            pc: pc_end,
            asid: Some(target_ulong::MAX),
            cb: u64::MAX,
            plugin_num: PluginReg::MAX,
            always_starts_block: true,
        };

        let hooks = self.hooks.read().unwrap();
        let instrumented_pcs = self.instrumented_pcs.read().unwrap();

        // iterate over B-tree matches. Add matches to set to avoid duplicates
        for &elem in hooks.range((Included(&low), Included(&high))) {
            if pc_start <= elem.pc && elem.pc < pc_end {
                if !instrumented_pcs.contains(&elem.pc) {
                    return true;
                }
            }
        }
        return false;
    }

    pub fn pc_instrumented(self: &Self, pc: target_ulong) -> bool {
        // println!("trying to lock instrumented_pcs");
        let instrumented_pcs = self.instrumented_pcs.read().unwrap();
        // println!("trying to lock instrumented pcs exit");
        instrumented_pcs.contains(&pc)
    }

    pub fn clear_tbs(self: &Self, cpu: &mut CPUState, tb: Option<*mut TranslationBlock>) {
        // println!("clear_tbs");
        self.new_hooks_add();
        // println!("new_hooks_add end");
        // start_tbs guarantee that pc is the start of the block
        let mut state = self.state.lock().unwrap();
        if !state.clear_start_tb.is_empty() {
            let instrumented_pcs = self.instrumented_pcs.read().unwrap();
            for &pc in state.clear_start_tb.iter() {
                if !instrumented_pcs.contains(&pc) {
                    let index = rust_tb_jmp_cache_hash_func(pc);
                    unsafe {
                        let pot = cpu.tb_jmp_cache[index as usize];
                        if !pot.is_null() && Some(pot) != tb && (*pot).pc == pc {
                            println!("invalidating {:x}", pc);
                            // u64::MAX -> -1
                            tb_phys_invalidate(pot, u64::MAX);
                        }
                    }
                }
            }
            state.clear_start_tb.clear();
        }
        //full_tbs can be any part of the block
        if !state.clear_full_tb.is_empty() {
            let instrumented_pcs = self.instrumented_pcs.read().unwrap();
            for &elem in cpu.tb_jmp_cache.iter() {
                if !elem.is_null() && Some(elem) != tb {
                    for &pc in state.clear_full_tb.iter() {
                        if !instrumented_pcs.contains(&pc) {
                            unsafe {
                                if (*elem).pc <= pc && pc < (*elem).pc + (*elem).size as u64 {
                                    println!("invalidating {:x}", pc);
                                    // u64::MAX -> -1
                                    tb_phys_invalidate(elem, u64::MAX);
                                    // break because other matches are irrelevant
                                    // for this tb
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            state.clear_full_tb.clear();
        }
        // println!("clear_tbs end");
    }
}
