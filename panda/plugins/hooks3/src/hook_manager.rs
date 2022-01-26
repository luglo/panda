use crate::api::PluginReg;
use crate::{middle_filter, tcg_codegen};
use std::cmp::{Ord, Ordering};
use std::collections::{BTreeSet, HashSet};
use std::ffi::c_void;
use std::ops::Bound::Included;

use panda::current_asid;
use panda::prelude::{target_ulong, CPUState, TranslationBlock};
use panda::sys::{tb_phys_invalidate, TCGOp, TARGET_PAGE_BITS};

// middle callback type
pub(crate) type MCB = extern "C" fn(&mut CPUState, &mut TranslationBlock);
// check_cpu_exit callback type
pub(crate) type CCE = unsafe extern "C" fn(*mut c_void);
// hooks callback type
pub(crate) type FnCb = extern "C" fn(&mut CPUState, &mut TranslationBlock, &Hook) -> bool;

extern "C" {
    fn find_first_guest_insn() -> *mut TCGOp;
    fn find_guest_insn_by_addr(pc: target_ulong) -> *mut TCGOp;
    fn insert_call_1p(after_op: *mut *mut TCGOp, fun: CCE, cpu: *mut c_void);
    #[allow(improper_ctypes)]
    fn insert_call_2p(
        after_op: *mut *mut TCGOp,
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

fn rust_tb_jmp_cache_hash_func(pc: target_ulong) -> u32 {
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

pub struct HookManager {
    hooks: BTreeSet<Hook>,
    matched_hooks: BTreeSet<Hook>,
    matched_pcs: HashSet<target_ulong>,
    clear_full_tb: Vec<target_ulong>,
    clear_start_tb: Vec<target_ulong>,
}

impl HookManager {
    pub fn new() -> Self {
        Self {
            hooks: BTreeSet::new(),
            matched_hooks: BTreeSet::new(),
            matched_pcs: HashSet::new(),
            clear_full_tb: Vec::new(),
            clear_start_tb: Vec::new(),
        }
    }

    pub fn has_hooks(self: &mut Self) -> bool {
        return !self.hooks.is_empty();
    }

    pub fn add(self: &mut Self, h: &Hook) {
        if h.always_starts_block {
            self.clear_start_tb.push(h.pc);
        } else {
            self.clear_full_tb.push(h.pc);
        }
        if !self.has_hooks() {
            tcg_codegen::enable();
        }
        self.hooks.insert(*h);
    }

    fn clear_empty_hooks(self: &mut Self) {
        if !self.matched_hooks.is_empty() {
            for &elem in self.matched_hooks.iter() {
                self.hooks.remove(&elem);
            }
            self.matched_hooks.clear();
        }

        if self.hooks.is_empty() {
            tcg_codegen::disable();
        }
    }

    pub fn remove_plugin(self: &mut Self, num: PluginReg) {
        for &elem in self.hooks.iter() {
            if elem.plugin_num == num {
                self.matched_hooks.insert(elem);
            }
        }
        self.clear_empty_hooks();
    }

    pub fn run_tb(self: &mut Self, cpu: &mut CPUState, tb: &mut TranslationBlock) {
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

        for &elem in self.hooks.range((Included(&low), Included(&high))) {
            if elem.asid == asid || elem.asid == None {
                // if callback returns true remove from hooks
                let cb = unsafe { std::mem::transmute::<u64, FnCb>(elem.cb) };
                if (cb)(cpu, tb, &elem) {
                    self.matched_hooks.insert(elem);
                }
            }
        }
        self.clear_empty_hooks();
    }

    pub fn insert_on_matches(self: &mut Self, cpu: &mut CPUState, tb: &mut TranslationBlock) {
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

        // iterate over B-tree matches. Add matches to set to avoid duplicates
        for &elem in self.hooks.range((Included(&low), Included(&high))) {
            // add matches to set. avoid duplicates
            if self.matched_pcs.contains(&elem.pc) {
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
                unsafe {
                    insert_call_2p(&mut op, middle_filter, cpu, tb);
                    insert_call_1p(&mut op, check_cpu_exit, u64::MAX as *mut c_void);
                }
            }
            self.matched_pcs.insert(elem.pc);
        }
        self.matched_pcs.clear();
    }

    pub fn clear_tbs(self: &mut Self, cpu: &mut CPUState) {
        // start_tbs guarantee that pc is the start of the block
        if !self.clear_start_tb.is_empty() {
            for &pc in self.clear_start_tb.iter() {
                let index = rust_tb_jmp_cache_hash_func(pc);
                unsafe {
                    let pot = cpu.tb_jmp_cache[index as usize];
                    if !pot.is_null() && (*pot).pc == pc {
                        // u64::MAX -> -1
                        tb_phys_invalidate(pot, u64::MAX);
                    }
                }
            }
            self.clear_start_tb.clear();
        }
        //full_tbs can be any part of the block
        if !self.clear_full_tb.is_empty() {
            for &elem in cpu.tb_jmp_cache.iter() {
                if !elem.is_null() {
                    for &pc in self.clear_full_tb.iter() {
                        unsafe {
                            if (*elem).pc <= pc && pc <= (*elem).pc + (*elem).size as u64 {
                                // u64::MAX -> -1
                                tb_phys_invalidate(elem, u64::MAX);
                                break;
                            }
                        }
                    }
                }
            }
            self.clear_full_tb.clear();
        }
    }
}
