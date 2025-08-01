use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;
use std::{io, iter, slice};

use object::read::archive::ArchiveFile;
use object::{Object, ObjectSection};
use rustc_codegen_ssa::back::lto::{SerializedModule, ThinModule, ThinShared};
use rustc_codegen_ssa::back::write::{CodegenContext, FatLtoInput};
use rustc_codegen_ssa::traits::*;
use rustc_codegen_ssa::{ModuleCodegen, ModuleKind, looks_like_rust_object_file};
use rustc_data_structures::fx::FxHashMap;
use rustc_data_structures::memmap::Mmap;
use rustc_errors::{DiagCtxtHandle, FatalError};
use rustc_middle::bug;
use rustc_middle::dep_graph::WorkProduct;
use rustc_session::config::{self, Lto};
use tracing::{debug, info};

use crate::back::write::{
    self, CodegenDiagnosticsStage, DiagnosticHandlers, bitcode_section_name, save_temp_bitcode,
};
use crate::errors::{LlvmError, LtoBitcodeFromRlib};
use crate::llvm::AttributePlace::Function;
use crate::llvm::{self, build_string};
use crate::{LlvmCodegenBackend, ModuleLlvm, SimpleCx, attributes};

/// We keep track of the computed LTO cache keys from the previous
/// session to determine which CGUs we can reuse.
const THIN_LTO_KEYS_INCR_COMP_FILE_NAME: &str = "thin-lto-past-keys.bin";

fn prepare_lto(
    cgcx: &CodegenContext<LlvmCodegenBackend>,
    exported_symbols_for_lto: &[String],
    each_linked_rlib_for_lto: &[PathBuf],
    dcx: DiagCtxtHandle<'_>,
) -> Result<(Vec<CString>, Vec<(SerializedModule<ModuleBuffer>, CString)>), FatalError> {
    let mut symbols_below_threshold = exported_symbols_for_lto
        .iter()
        .map(|symbol| CString::new(symbol.to_owned()).unwrap())
        .collect::<Vec<CString>>();

    // __llvm_profile_counter_bias is pulled in at link time by an undefined reference to
    // __llvm_profile_runtime, therefore we won't know until link time if this symbol
    // should have default visibility.
    symbols_below_threshold.push(c"__llvm_profile_counter_bias".to_owned());

    // If we're performing LTO for the entire crate graph, then for each of our
    // upstream dependencies, find the corresponding rlib and load the bitcode
    // from the archive.
    //
    // We save off all the bytecode and LLVM module ids for later processing
    // with either fat or thin LTO
    let mut upstream_modules = Vec::new();
    if cgcx.lto != Lto::ThinLocal {
        for path in each_linked_rlib_for_lto {
            let archive_data = unsafe {
                Mmap::map(std::fs::File::open(&path).expect("couldn't open rlib"))
                    .expect("couldn't map rlib")
            };
            let archive = ArchiveFile::parse(&*archive_data).expect("wanted an rlib");
            let obj_files = archive
                .members()
                .filter_map(|child| {
                    child.ok().and_then(|c| {
                        std::str::from_utf8(c.name()).ok().map(|name| (name.trim(), c))
                    })
                })
                .filter(|&(name, _)| looks_like_rust_object_file(name));
            for (name, child) in obj_files {
                info!("adding bitcode from {}", name);
                match get_bitcode_slice_from_object_data(
                    child.data(&*archive_data).expect("corrupt rlib"),
                    cgcx,
                ) {
                    Ok(data) => {
                        let module = SerializedModule::FromRlib(data.to_vec());
                        upstream_modules.push((module, CString::new(name).unwrap()));
                    }
                    Err(e) => {
                        dcx.emit_err(e);
                        return Err(FatalError);
                    }
                }
            }
        }
    }

    Ok((symbols_below_threshold, upstream_modules))
}

fn get_bitcode_slice_from_object_data<'a>(
    obj: &'a [u8],
    cgcx: &CodegenContext<LlvmCodegenBackend>,
) -> Result<&'a [u8], LtoBitcodeFromRlib> {
    // We're about to assume the data here is an object file with sections, but if it's raw LLVM IR
    // that won't work. Fortunately, if that's what we have we can just return the object directly,
    // so we sniff the relevant magic strings here and return.
    if obj.starts_with(b"\xDE\xC0\x17\x0B") || obj.starts_with(b"BC\xC0\xDE") {
        return Ok(obj);
    }
    // We drop the "__LLVM," prefix here because on Apple platforms there's a notion of "segment
    // name" which in the public API for sections gets treated as part of the section name, but
    // internally in MachOObjectFile.cpp gets treated separately.
    let section_name = bitcode_section_name(cgcx).to_str().unwrap().trim_start_matches("__LLVM,");

    let obj =
        object::File::parse(obj).map_err(|err| LtoBitcodeFromRlib { err: err.to_string() })?;

    let section = obj
        .section_by_name(section_name)
        .ok_or_else(|| LtoBitcodeFromRlib { err: format!("Can't find section {section_name}") })?;

    section.data().map_err(|err| LtoBitcodeFromRlib { err: err.to_string() })
}

/// Performs fat LTO by merging all modules into a single one and returning it
/// for further optimization.
pub(crate) fn run_fat(
    cgcx: &CodegenContext<LlvmCodegenBackend>,
    exported_symbols_for_lto: &[String],
    each_linked_rlib_for_lto: &[PathBuf],
    modules: Vec<FatLtoInput<LlvmCodegenBackend>>,
) -> Result<ModuleCodegen<ModuleLlvm>, FatalError> {
    let dcx = cgcx.create_dcx();
    let dcx = dcx.handle();
    let (symbols_below_threshold, upstream_modules) =
        prepare_lto(cgcx, exported_symbols_for_lto, each_linked_rlib_for_lto, dcx)?;
    let symbols_below_threshold =
        symbols_below_threshold.iter().map(|c| c.as_ptr()).collect::<Vec<_>>();
    fat_lto(cgcx, dcx, modules, upstream_modules, &symbols_below_threshold)
}

/// Performs thin LTO by performing necessary global analysis and returning two
/// lists, one of the modules that need optimization and another for modules that
/// can simply be copied over from the incr. comp. cache.
pub(crate) fn run_thin(
    cgcx: &CodegenContext<LlvmCodegenBackend>,
    exported_symbols_for_lto: &[String],
    each_linked_rlib_for_lto: &[PathBuf],
    modules: Vec<(String, ThinBuffer)>,
    cached_modules: Vec<(SerializedModule<ModuleBuffer>, WorkProduct)>,
) -> Result<(Vec<ThinModule<LlvmCodegenBackend>>, Vec<WorkProduct>), FatalError> {
    let dcx = cgcx.create_dcx();
    let dcx = dcx.handle();
    let (symbols_below_threshold, upstream_modules) =
        prepare_lto(cgcx, exported_symbols_for_lto, each_linked_rlib_for_lto, dcx)?;
    let symbols_below_threshold =
        symbols_below_threshold.iter().map(|c| c.as_ptr()).collect::<Vec<_>>();
    if cgcx.opts.cg.linker_plugin_lto.enabled() {
        unreachable!(
            "We should never reach this case if the LTO step \
                      is deferred to the linker"
        );
    }
    thin_lto(cgcx, dcx, modules, upstream_modules, cached_modules, &symbols_below_threshold)
}

pub(crate) fn prepare_thin(
    module: ModuleCodegen<ModuleLlvm>,
    emit_summary: bool,
) -> (String, ThinBuffer) {
    let name = module.name;
    let buffer = ThinBuffer::new(module.module_llvm.llmod(), true, emit_summary);
    (name, buffer)
}

fn fat_lto(
    cgcx: &CodegenContext<LlvmCodegenBackend>,
    dcx: DiagCtxtHandle<'_>,
    modules: Vec<FatLtoInput<LlvmCodegenBackend>>,
    mut serialized_modules: Vec<(SerializedModule<ModuleBuffer>, CString)>,
    symbols_below_threshold: &[*const libc::c_char],
) -> Result<ModuleCodegen<ModuleLlvm>, FatalError> {
    let _timer = cgcx.prof.generic_activity("LLVM_fat_lto_build_monolithic_module");
    info!("going for a fat lto");

    // Sort out all our lists of incoming modules into two lists.
    //
    // * `serialized_modules` (also and argument to this function) contains all
    //   modules that are serialized in-memory.
    // * `in_memory` contains modules which are already parsed and in-memory,
    //   such as from multi-CGU builds.
    let mut in_memory = Vec::new();
    for module in modules {
        match module {
            FatLtoInput::InMemory(m) => in_memory.push(m),
            FatLtoInput::Serialized { name, buffer } => {
                info!("pushing serialized module {:?}", name);
                serialized_modules.push((buffer, CString::new(name).unwrap()));
            }
        }
    }

    // Find the "costliest" module and merge everything into that codegen unit.
    // All the other modules will be serialized and reparsed into the new
    // context, so this hopefully avoids serializing and parsing the largest
    // codegen unit.
    //
    // Additionally use a regular module as the base here to ensure that various
    // file copy operations in the backend work correctly. The only other kind
    // of module here should be an allocator one, and if your crate is smaller
    // than the allocator module then the size doesn't really matter anyway.
    let costliest_module = in_memory
        .iter()
        .enumerate()
        .filter(|&(_, module)| module.kind == ModuleKind::Regular)
        .map(|(i, module)| {
            let cost = unsafe { llvm::LLVMRustModuleCost(module.module_llvm.llmod()) };
            (cost, i)
        })
        .max();

    // If we found a costliest module, we're good to go. Otherwise all our
    // inputs were serialized which could happen in the case, for example, that
    // all our inputs were incrementally reread from the cache and we're just
    // re-executing the LTO passes. If that's the case deserialize the first
    // module and create a linker with it.
    let module: ModuleCodegen<ModuleLlvm> = match costliest_module {
        Some((_cost, i)) => in_memory.remove(i),
        None => {
            assert!(!serialized_modules.is_empty(), "must have at least one serialized module");
            let (buffer, name) = serialized_modules.remove(0);
            info!("no in-memory regular modules to choose from, parsing {:?}", name);
            let llvm_module = ModuleLlvm::parse(cgcx, &name, buffer.data(), dcx)?;
            ModuleCodegen::new_regular(name.into_string().unwrap(), llvm_module)
        }
    };
    {
        let (llcx, llmod) = {
            let llvm = &module.module_llvm;
            (&llvm.llcx, llvm.llmod())
        };
        info!("using {:?} as a base module", module.name);

        // The linking steps below may produce errors and diagnostics within LLVM
        // which we'd like to handle and print, so set up our diagnostic handlers
        // (which get unregistered when they go out of scope below).
        let _handler =
            DiagnosticHandlers::new(cgcx, dcx, llcx, &module, CodegenDiagnosticsStage::LTO);

        // For all other modules we codegened we'll need to link them into our own
        // bitcode. All modules were codegened in their own LLVM context, however,
        // and we want to move everything to the same LLVM context. Currently the
        // way we know of to do that is to serialize them to a string and them parse
        // them later. Not great but hey, that's why it's "fat" LTO, right?
        for module in in_memory {
            let buffer = ModuleBuffer::new(module.module_llvm.llmod());
            let llmod_id = CString::new(&module.name[..]).unwrap();
            serialized_modules.push((SerializedModule::Local(buffer), llmod_id));
        }
        // Sort the modules to ensure we produce deterministic results.
        serialized_modules.sort_by(|module1, module2| module1.1.cmp(&module2.1));

        // For all serialized bitcode files we parse them and link them in as we did
        // above, this is all mostly handled in C++.
        let mut linker = Linker::new(llmod);
        for (bc_decoded, name) in serialized_modules {
            let _timer = cgcx
                .prof
                .generic_activity_with_arg_recorder("LLVM_fat_lto_link_module", |recorder| {
                    recorder.record_arg(format!("{name:?}"))
                });
            info!("linking {:?}", name);
            let data = bc_decoded.data();
            linker.add(data).map_err(|()| write::llvm_err(dcx, LlvmError::LoadBitcode { name }))?;
        }
        drop(linker);
        save_temp_bitcode(cgcx, &module, "lto.input");

        // Internalize everything below threshold to help strip out more modules and such.
        unsafe {
            let ptr = symbols_below_threshold.as_ptr();
            llvm::LLVMRustRunRestrictionPass(
                llmod,
                ptr as *const *const libc::c_char,
                symbols_below_threshold.len() as libc::size_t,
            );
        }
        save_temp_bitcode(cgcx, &module, "lto.after-restriction");
    }

    Ok(module)
}

pub(crate) struct Linker<'a>(&'a mut llvm::Linker<'a>);

impl<'a> Linker<'a> {
    pub(crate) fn new(llmod: &'a llvm::Module) -> Self {
        unsafe { Linker(llvm::LLVMRustLinkerNew(llmod)) }
    }

    pub(crate) fn add(&mut self, bytecode: &[u8]) -> Result<(), ()> {
        unsafe {
            if llvm::LLVMRustLinkerAdd(
                self.0,
                bytecode.as_ptr() as *const libc::c_char,
                bytecode.len(),
            ) {
                Ok(())
            } else {
                Err(())
            }
        }
    }
}

impl Drop for Linker<'_> {
    fn drop(&mut self) {
        unsafe {
            llvm::LLVMRustLinkerFree(&mut *(self.0 as *mut _));
        }
    }
}

/// Prepare "thin" LTO to get run on these modules.
///
/// The general structure of ThinLTO is quite different from the structure of
/// "fat" LTO above. With "fat" LTO all LLVM modules in question are merged into
/// one giant LLVM module, and then we run more optimization passes over this
/// big module after internalizing most symbols. Thin LTO, on the other hand,
/// avoid this large bottleneck through more targeted optimization.
///
/// At a high level Thin LTO looks like:
///
///    1. Prepare a "summary" of each LLVM module in question which describes
///       the values inside, cost of the values, etc.
///    2. Merge the summaries of all modules in question into one "index"
///    3. Perform some global analysis on this index
///    4. For each module, use the index and analysis calculated previously to
///       perform local transformations on the module, for example inlining
///       small functions from other modules.
///    5. Run thin-specific optimization passes over each module, and then code
///       generate everything at the end.
///
/// The summary for each module is intended to be quite cheap, and the global
/// index is relatively quite cheap to create as well. As a result, the goal of
/// ThinLTO is to reduce the bottleneck on LTO and enable LTO to be used in more
/// situations. For example one cheap optimization is that we can parallelize
/// all codegen modules, easily making use of all the cores on a machine.
///
/// With all that in mind, the function here is designed at specifically just
/// calculating the *index* for ThinLTO. This index will then be shared amongst
/// all of the `LtoModuleCodegen` units returned below and destroyed once
/// they all go out of scope.
fn thin_lto(
    cgcx: &CodegenContext<LlvmCodegenBackend>,
    dcx: DiagCtxtHandle<'_>,
    modules: Vec<(String, ThinBuffer)>,
    serialized_modules: Vec<(SerializedModule<ModuleBuffer>, CString)>,
    cached_modules: Vec<(SerializedModule<ModuleBuffer>, WorkProduct)>,
    symbols_below_threshold: &[*const libc::c_char],
) -> Result<(Vec<ThinModule<LlvmCodegenBackend>>, Vec<WorkProduct>), FatalError> {
    let _timer = cgcx.prof.generic_activity("LLVM_thin_lto_global_analysis");
    unsafe {
        info!("going for that thin, thin LTO");

        let green_modules: FxHashMap<_, _> =
            cached_modules.iter().map(|(_, wp)| (wp.cgu_name.clone(), wp.clone())).collect();

        let full_scope_len = modules.len() + serialized_modules.len() + cached_modules.len();
        let mut thin_buffers = Vec::with_capacity(modules.len());
        let mut module_names = Vec::with_capacity(full_scope_len);
        let mut thin_modules = Vec::with_capacity(full_scope_len);

        for (i, (name, buffer)) in modules.into_iter().enumerate() {
            info!("local module: {} - {}", i, name);
            let cname = CString::new(name.as_bytes()).unwrap();
            thin_modules.push(llvm::ThinLTOModule {
                identifier: cname.as_ptr(),
                data: buffer.data().as_ptr(),
                len: buffer.data().len(),
            });
            thin_buffers.push(buffer);
            module_names.push(cname);
        }

        // FIXME: All upstream crates are deserialized internally in the
        //        function below to extract their summary and modules. Note that
        //        unlike the loop above we *must* decode and/or read something
        //        here as these are all just serialized files on disk. An
        //        improvement, however, to make here would be to store the
        //        module summary separately from the actual module itself. Right
        //        now this is store in one large bitcode file, and the entire
        //        file is deflate-compressed. We could try to bypass some of the
        //        decompression by storing the index uncompressed and only
        //        lazily decompressing the bytecode if necessary.
        //
        //        Note that truly taking advantage of this optimization will
        //        likely be further down the road. We'd have to implement
        //        incremental ThinLTO first where we could actually avoid
        //        looking at upstream modules entirely sometimes (the contents,
        //        we must always unconditionally look at the index).
        let mut serialized = Vec::with_capacity(serialized_modules.len() + cached_modules.len());

        let cached_modules =
            cached_modules.into_iter().map(|(sm, wp)| (sm, CString::new(wp.cgu_name).unwrap()));

        for (module, name) in serialized_modules.into_iter().chain(cached_modules) {
            info!("upstream or cached module {:?}", name);
            thin_modules.push(llvm::ThinLTOModule {
                identifier: name.as_ptr(),
                data: module.data().as_ptr(),
                len: module.data().len(),
            });
            serialized.push(module);
            module_names.push(name);
        }

        // Sanity check
        assert_eq!(thin_modules.len(), module_names.len());

        // Delegate to the C++ bindings to create some data here. Once this is a
        // tried-and-true interface we may wish to try to upstream some of this
        // to LLVM itself, right now we reimplement a lot of what they do
        // upstream...
        let data = llvm::LLVMRustCreateThinLTOData(
            thin_modules.as_ptr(),
            thin_modules.len(),
            symbols_below_threshold.as_ptr(),
            symbols_below_threshold.len(),
        )
        .ok_or_else(|| write::llvm_err(dcx, LlvmError::PrepareThinLtoContext))?;

        let data = ThinData(data);

        info!("thin LTO data created");

        let (key_map_path, prev_key_map, curr_key_map) = if let Some(ref incr_comp_session_dir) =
            cgcx.incr_comp_session_dir
        {
            let path = incr_comp_session_dir.join(THIN_LTO_KEYS_INCR_COMP_FILE_NAME);
            // If the previous file was deleted, or we get an IO error
            // reading the file, then we'll just use `None` as the
            // prev_key_map, which will force the code to be recompiled.
            let prev =
                if path.exists() { ThinLTOKeysMap::load_from_file(&path).ok() } else { None };
            let curr = ThinLTOKeysMap::from_thin_lto_modules(&data, &thin_modules, &module_names);
            (Some(path), prev, curr)
        } else {
            // If we don't compile incrementally, we don't need to load the
            // import data from LLVM.
            assert!(green_modules.is_empty());
            let curr = ThinLTOKeysMap::default();
            (None, None, curr)
        };
        info!("thin LTO cache key map loaded");
        info!("prev_key_map: {:#?}", prev_key_map);
        info!("curr_key_map: {:#?}", curr_key_map);

        // Throw our data in an `Arc` as we'll be sharing it across threads. We
        // also put all memory referenced by the C++ data (buffers, ids, etc)
        // into the arc as well. After this we'll create a thin module
        // codegen per module in this data.
        let shared = Arc::new(ThinShared {
            data,
            thin_buffers,
            serialized_modules: serialized,
            module_names,
        });

        let mut copy_jobs = vec![];
        let mut opt_jobs = vec![];

        info!("checking which modules can be-reused and which have to be re-optimized.");
        for (module_index, module_name) in shared.module_names.iter().enumerate() {
            let module_name = module_name_to_str(module_name);
            if let (Some(prev_key_map), true) =
                (prev_key_map.as_ref(), green_modules.contains_key(module_name))
            {
                assert!(cgcx.incr_comp_session_dir.is_some());

                // If a module exists in both the current and the previous session,
                // and has the same LTO cache key in both sessions, then we can re-use it
                if prev_key_map.keys.get(module_name) == curr_key_map.keys.get(module_name) {
                    let work_product = green_modules[module_name].clone();
                    copy_jobs.push(work_product);
                    info!(" - {}: re-used", module_name);
                    assert!(cgcx.incr_comp_session_dir.is_some());
                    continue;
                }
            }

            info!(" - {}: re-compiled", module_name);
            opt_jobs.push(ThinModule { shared: Arc::clone(&shared), idx: module_index });
        }

        // Save the current ThinLTO import information for the next compilation
        // session, overwriting the previous serialized data (if any).
        if let Some(path) = key_map_path
            && let Err(err) = curr_key_map.save_to_file(&path)
        {
            return Err(write::llvm_err(dcx, LlvmError::WriteThinLtoKey { err }));
        }

        Ok((opt_jobs, copy_jobs))
    }
}

fn enable_autodiff_settings(ad: &[config::AutoDiff]) {
    for val in ad {
        // We intentionally don't use a wildcard, to not forget handling anything new.
        match val {
            config::AutoDiff::PrintPerf => {
                llvm::set_print_perf(true);
            }
            config::AutoDiff::PrintAA => {
                llvm::set_print_activity(true);
            }
            config::AutoDiff::PrintTA => {
                llvm::set_print_type(true);
            }
            config::AutoDiff::PrintTAFn(fun) => {
                llvm::set_print_type(true); // Enable general type printing
                llvm::set_print_type_fun(&fun); // Set specific function to analyze
            }
            config::AutoDiff::Inline => {
                llvm::set_inline(true);
            }
            config::AutoDiff::LooseTypes => {
                llvm::set_loose_types(true);
            }
            config::AutoDiff::PrintSteps => {
                llvm::set_print(true);
            }
            // We handle this in the PassWrapper.cpp
            config::AutoDiff::PrintPasses => {}
            // We handle this in the PassWrapper.cpp
            config::AutoDiff::PrintModBefore => {}
            // We handle this in the PassWrapper.cpp
            config::AutoDiff::PrintModAfter => {}
            // We handle this in the PassWrapper.cpp
            config::AutoDiff::PrintModFinal => {}
            // This is required and already checked
            config::AutoDiff::Enable => {}
            // We handle this below
            config::AutoDiff::NoPostopt => {}
        }
    }
    // This helps with handling enums for now.
    llvm::set_strict_aliasing(false);
    // FIXME(ZuseZ4): Test this, since it was added a long time ago.
    llvm::set_rust_rules(true);
}

pub(crate) fn run_pass_manager(
    cgcx: &CodegenContext<LlvmCodegenBackend>,
    dcx: DiagCtxtHandle<'_>,
    module: &mut ModuleCodegen<ModuleLlvm>,
    thin: bool,
) -> Result<(), FatalError> {
    let _timer = cgcx.prof.generic_activity_with_arg("LLVM_lto_optimize", &*module.name);
    let config = cgcx.config(module.kind);

    // Now we have one massive module inside of llmod. Time to run the
    // LTO-specific optimization passes that LLVM provides.
    //
    // This code is based off the code found in llvm's LTO code generator:
    //      llvm/lib/LTO/LTOCodeGenerator.cpp
    debug!("running the pass manager");
    let opt_stage = if thin { llvm::OptStage::ThinLTO } else { llvm::OptStage::FatLTO };
    let opt_level = config.opt_level.unwrap_or(config::OptLevel::No);

    // The PostAD behavior is the same that we would have if no autodiff was used.
    // It will run the default optimization pipeline. If AD is enabled we select
    // the DuringAD stage, which will disable vectorization and loop unrolling, and
    // schedule two autodiff optimization + differentiation passes.
    // We then run the llvm_optimize function a second time, to optimize the code which we generated
    // in the enzyme differentiation pass.
    let enable_ad = config.autodiff.contains(&config::AutoDiff::Enable);
    let enable_gpu = config.offload.contains(&config::Offload::Enable);
    let stage = if thin {
        write::AutodiffStage::PreAD
    } else {
        if enable_ad { write::AutodiffStage::DuringAD } else { write::AutodiffStage::PostAD }
    };

    if enable_ad {
        enable_autodiff_settings(&config.autodiff);
    }

    unsafe {
        write::llvm_optimize(cgcx, dcx, module, None, config, opt_level, opt_stage, stage)?;
    }

    if enable_gpu && !thin {
        let cx =
            SimpleCx::new(module.module_llvm.llmod(), &module.module_llvm.llcx, cgcx.pointer_size);
        crate::builder::gpu_offload::handle_gpu_code(cgcx, &cx);
    }

    if cfg!(llvm_enzyme) && enable_ad && !thin {
        let cx =
            SimpleCx::new(module.module_llvm.llmod(), &module.module_llvm.llcx, cgcx.pointer_size);

        for function in cx.get_functions() {
            let enzyme_marker = "enzyme_marker";
            if attributes::has_string_attr(function, enzyme_marker) {
                // Sanity check: Ensure 'noinline' is present before replacing it.
                assert!(
                    attributes::has_attr(function, Function, llvm::AttributeKind::NoInline),
                    "Expected __enzyme function to have 'noinline' before adding 'alwaysinline'"
                );

                attributes::remove_from_llfn(function, Function, llvm::AttributeKind::NoInline);
                attributes::remove_string_attr_from_llfn(function, enzyme_marker);

                assert!(
                    !attributes::has_string_attr(function, enzyme_marker),
                    "Expected function to not have 'enzyme_marker'"
                );

                let always_inline = llvm::AttributeKind::AlwaysInline.create_attr(cx.llcx);
                attributes::apply_to_llfn(function, Function, &[always_inline]);
            }
        }

        let opt_stage = llvm::OptStage::FatLTO;
        let stage = write::AutodiffStage::PostAD;
        if !config.autodiff.contains(&config::AutoDiff::NoPostopt) {
            unsafe {
                write::llvm_optimize(cgcx, dcx, module, None, config, opt_level, opt_stage, stage)?;
            }
        }

        // This is the final IR, so people should be able to inspect the optimized autodiff output,
        // for manual inspection.
        if config.autodiff.contains(&config::AutoDiff::PrintModFinal) {
            unsafe { llvm::LLVMDumpModule(module.module_llvm.llmod()) };
        }
    }

    debug!("lto done");
    Ok(())
}

pub struct ModuleBuffer(&'static mut llvm::ModuleBuffer);

unsafe impl Send for ModuleBuffer {}
unsafe impl Sync for ModuleBuffer {}

impl ModuleBuffer {
    pub(crate) fn new(m: &llvm::Module) -> ModuleBuffer {
        ModuleBuffer(unsafe { llvm::LLVMRustModuleBufferCreate(m) })
    }
}

impl ModuleBufferMethods for ModuleBuffer {
    fn data(&self) -> &[u8] {
        unsafe {
            let ptr = llvm::LLVMRustModuleBufferPtr(self.0);
            let len = llvm::LLVMRustModuleBufferLen(self.0);
            slice::from_raw_parts(ptr, len)
        }
    }
}

impl Drop for ModuleBuffer {
    fn drop(&mut self) {
        unsafe {
            llvm::LLVMRustModuleBufferFree(&mut *(self.0 as *mut _));
        }
    }
}

pub struct ThinData(&'static mut llvm::ThinLTOData);

unsafe impl Send for ThinData {}
unsafe impl Sync for ThinData {}

impl Drop for ThinData {
    fn drop(&mut self) {
        unsafe {
            llvm::LLVMRustFreeThinLTOData(&mut *(self.0 as *mut _));
        }
    }
}

pub struct ThinBuffer(&'static mut llvm::ThinLTOBuffer);

unsafe impl Send for ThinBuffer {}
unsafe impl Sync for ThinBuffer {}

impl ThinBuffer {
    pub(crate) fn new(m: &llvm::Module, is_thin: bool, emit_summary: bool) -> ThinBuffer {
        unsafe {
            let buffer = llvm::LLVMRustThinLTOBufferCreate(m, is_thin, emit_summary);
            ThinBuffer(buffer)
        }
    }

    pub(crate) unsafe fn from_raw_ptr(ptr: *mut llvm::ThinLTOBuffer) -> ThinBuffer {
        let mut ptr = NonNull::new(ptr).unwrap();
        ThinBuffer(unsafe { ptr.as_mut() })
    }
}

impl ThinBufferMethods for ThinBuffer {
    fn data(&self) -> &[u8] {
        unsafe {
            let ptr = llvm::LLVMRustThinLTOBufferPtr(self.0) as *const _;
            let len = llvm::LLVMRustThinLTOBufferLen(self.0);
            slice::from_raw_parts(ptr, len)
        }
    }

    fn thin_link_data(&self) -> &[u8] {
        unsafe {
            let ptr = llvm::LLVMRustThinLTOBufferThinLinkDataPtr(self.0) as *const _;
            let len = llvm::LLVMRustThinLTOBufferThinLinkDataLen(self.0);
            slice::from_raw_parts(ptr, len)
        }
    }
}

impl Drop for ThinBuffer {
    fn drop(&mut self) {
        unsafe {
            llvm::LLVMRustThinLTOBufferFree(&mut *(self.0 as *mut _));
        }
    }
}

pub(crate) fn optimize_thin_module(
    thin_module: ThinModule<LlvmCodegenBackend>,
    cgcx: &CodegenContext<LlvmCodegenBackend>,
) -> Result<ModuleCodegen<ModuleLlvm>, FatalError> {
    let dcx = cgcx.create_dcx();
    let dcx = dcx.handle();

    let module_name = &thin_module.shared.module_names[thin_module.idx];

    // Right now the implementation we've got only works over serialized
    // modules, so we create a fresh new LLVM context and parse the module
    // into that context. One day, however, we may do this for upstream
    // crates but for locally codegened modules we may be able to reuse
    // that LLVM Context and Module.
    let module_llvm = ModuleLlvm::parse(cgcx, module_name, thin_module.data(), dcx)?;
    let mut module = ModuleCodegen::new_regular(thin_module.name(), module_llvm);
    // Given that the newly created module lacks a thinlto buffer for embedding, we need to re-add it here.
    if cgcx.config(ModuleKind::Regular).embed_bitcode() {
        module.thin_lto_buffer = Some(thin_module.data().to_vec());
    }
    {
        let target = &*module.module_llvm.tm;
        let llmod = module.module_llvm.llmod();
        save_temp_bitcode(cgcx, &module, "thin-lto-input");

        // Up next comes the per-module local analyses that we do for Thin LTO.
        // Each of these functions is basically copied from the LLVM
        // implementation and then tailored to suit this implementation. Ideally
        // each of these would be supported by upstream LLVM but that's perhaps
        // a patch for another day!
        //
        // You can find some more comments about these functions in the LLVM
        // bindings we've got (currently `PassWrapper.cpp`)
        {
            let _timer =
                cgcx.prof.generic_activity_with_arg("LLVM_thin_lto_rename", thin_module.name());
            unsafe {
                llvm::LLVMRustPrepareThinLTORename(thin_module.shared.data.0, llmod, target.raw())
            };
            save_temp_bitcode(cgcx, &module, "thin-lto-after-rename");
        }

        {
            let _timer = cgcx
                .prof
                .generic_activity_with_arg("LLVM_thin_lto_resolve_weak", thin_module.name());
            if unsafe { !llvm::LLVMRustPrepareThinLTOResolveWeak(thin_module.shared.data.0, llmod) }
            {
                return Err(write::llvm_err(dcx, LlvmError::PrepareThinLtoModule));
            }
            save_temp_bitcode(cgcx, &module, "thin-lto-after-resolve");
        }

        {
            let _timer = cgcx
                .prof
                .generic_activity_with_arg("LLVM_thin_lto_internalize", thin_module.name());
            if unsafe { !llvm::LLVMRustPrepareThinLTOInternalize(thin_module.shared.data.0, llmod) }
            {
                return Err(write::llvm_err(dcx, LlvmError::PrepareThinLtoModule));
            }
            save_temp_bitcode(cgcx, &module, "thin-lto-after-internalize");
        }

        {
            let _timer =
                cgcx.prof.generic_activity_with_arg("LLVM_thin_lto_import", thin_module.name());
            if unsafe {
                !llvm::LLVMRustPrepareThinLTOImport(thin_module.shared.data.0, llmod, target.raw())
            } {
                return Err(write::llvm_err(dcx, LlvmError::PrepareThinLtoModule));
            }
            save_temp_bitcode(cgcx, &module, "thin-lto-after-import");
        }

        // Alright now that we've done everything related to the ThinLTO
        // analysis it's time to run some optimizations! Here we use the same
        // `run_pass_manager` as the "fat" LTO above except that we tell it to
        // populate a thin-specific pass manager, which presumably LLVM treats a
        // little differently.
        {
            info!("running thin lto passes over {}", module.name);
            run_pass_manager(cgcx, dcx, &mut module, true)?;
            save_temp_bitcode(cgcx, &module, "thin-lto-after-pm");
        }
    }
    Ok(module)
}

/// Maps LLVM module identifiers to their corresponding LLVM LTO cache keys
#[derive(Debug, Default)]
struct ThinLTOKeysMap {
    // key = llvm name of importing module, value = LLVM cache key
    keys: BTreeMap<String, String>,
}

impl ThinLTOKeysMap {
    fn save_to_file(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;
        let mut writer = File::create_buffered(path)?;
        // The entries are loaded back into a hash map in `load_from_file()`, so
        // the order in which we write them to file here does not matter.
        for (module, key) in &self.keys {
            writeln!(writer, "{module} {key}")?;
        }
        Ok(())
    }

    fn load_from_file(path: &Path) -> io::Result<Self> {
        use std::io::BufRead;
        let mut keys = BTreeMap::default();
        let file = File::open_buffered(path)?;
        for line in file.lines() {
            let line = line?;
            let mut split = line.split(' ');
            let module = split.next().unwrap();
            let key = split.next().unwrap();
            assert_eq!(split.next(), None, "Expected two space-separated values, found {line:?}");
            keys.insert(module.to_string(), key.to_string());
        }
        Ok(Self { keys })
    }

    fn from_thin_lto_modules(
        data: &ThinData,
        modules: &[llvm::ThinLTOModule],
        names: &[CString],
    ) -> Self {
        let keys = iter::zip(modules, names)
            .map(|(module, name)| {
                let key = build_string(|rust_str| unsafe {
                    llvm::LLVMRustComputeLTOCacheKey(rust_str, module.identifier, data.0);
                })
                .expect("Invalid ThinLTO module key");
                (module_name_to_str(name).to_string(), key)
            })
            .collect();
        Self { keys }
    }
}

fn module_name_to_str(c_str: &CStr) -> &str {
    c_str.to_str().unwrap_or_else(|e| {
        bug!("Encountered non-utf8 LLVM module name `{}`: {}", c_str.to_string_lossy(), e)
    })
}

pub(crate) fn parse_module<'a>(
    cx: &'a llvm::Context,
    name: &CStr,
    data: &[u8],
    dcx: DiagCtxtHandle<'_>,
) -> Result<&'a llvm::Module, FatalError> {
    unsafe {
        llvm::LLVMRustParseBitcodeForLTO(cx, data.as_ptr(), data.len(), name.as_ptr())
            .ok_or_else(|| write::llvm_err(dcx, LlvmError::ParseBitcode))
    }
}
