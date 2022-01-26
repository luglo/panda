#pragma once 
typedef uint32_t PluginReg;
typedef bool (*FnCb)(CPUState*, TranslationBlock*, const struct Hook*);

struct Hook {
    target_ulong pc;
    target_ulong asid;
    PluginReg plugin_num;
    FnCb cb;
    bool always_starts_block;
};


void add_hook(PluginReg num,
              target_ulong pc,
              target_ulong asid,
              bool always_starts_block,
              FnCb fun);

void unregister_plugin(PluginReg num);

PluginReg register_plugin(void);