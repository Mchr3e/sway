use super::{
    asm_builder::{AsmBuilder, AsmBuilderResult},
    evm::EvmAsmBuilder,
    finalized_asm::{check_invalid_opcodes, FinalizedAsm},
    fuel::{
        data_section::{DataId, DataSection},
        fuel_asm_builder::FuelAsmBuilder,
        register_sequencer::RegisterSequencer,
    },
    programs::{AbstractEntry, AbstractProgram, FinalProgram, ProgramKind},
    MidenVMAsmBuilder,
};

use crate::{err, ok, BuildConfig, BuildTarget, CompileResult, CompileWarning};

use sway_error::error::CompileError;
use sway_ir::*;

pub fn compile_ir_to_asm(
    ir: &Context,
    build_config: Option<&BuildConfig>,
) -> CompileResult<FinalizedAsm> {
    // Eventually when we get this 'correct' with no hacks we'll want to compile all the modules
    // separately and then use a linker to connect them.  This way we could also keep binary caches
    // of libraries and link against them, rather than recompile everything each time.  For now we
    // assume there is one module.
    assert!(ir.module_iter().count() == 1);

    let mut warnings: Vec<CompileWarning> = Vec::new();
    let mut errors: Vec<CompileError> = Vec::new();

    let module = ir.module_iter().next().unwrap();
    let final_program = check!(
        compile_module_to_asm(RegisterSequencer::new(), ir, module, build_config),
        return err(warnings, errors),
        warnings,
        errors
    );

    if build_config
        .map(|cfg| cfg.print_finalized_asm)
        .unwrap_or(false)
    {
        println!(";; --- FINAL PROGRAM ---\n");
        println!("{final_program}");
    }

    let final_asm = final_program.finalize();

    check!(
        check_invalid_opcodes(&final_asm),
        return err(warnings, errors),
        warnings,
        errors
    );

    ok(final_asm, warnings, errors)
}

fn compile_module_to_asm(
    reg_seqr: RegisterSequencer,
    context: &Context,
    module: Module,
    build_config: Option<&BuildConfig>,
) -> CompileResult<FinalProgram> {
    let kind = match module.get_kind(context) {
        Kind::Contract => ProgramKind::Contract,
        Kind::Library => ProgramKind::Library,
        Kind::Predicate => ProgramKind::Predicate,
        Kind::Script => ProgramKind::Script,
    };

    let build_target = match build_config {
        Some(cfg) => cfg.build_target,
        None => BuildTarget::default(),
    };

    let mut builder: Box<dyn AsmBuilder> = match build_target {
        BuildTarget::Fuel => Box::new(FuelAsmBuilder::new(
            kind,
            DataSection::default(),
            reg_seqr,
            context,
        )),
        BuildTarget::EVM => Box::new(EvmAsmBuilder::new(kind, context)),
        BuildTarget::MidenVM => Box::new(MidenVMAsmBuilder::new(kind, context)),
    };

    // Pre-create labels for all functions before we generate other code, so we can call them
    // before compiling them if needed.
    for func in module.function_iter(context) {
        builder.func_to_labels(&func);
    }

    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    for function in module.function_iter(context) {
        check!(
            builder.compile_function(function),
            return err(warnings, errors),
            warnings,
            errors
        );
    }

    // Get the compiled result and massage a bit for the AbstractProgram.
    let result = builder.finalize();
    let final_program = match result {
        AsmBuilderResult::Fuel(result) => {
            let (data_section, reg_seqr, entries, non_entries) = result;
            let entries = entries
                .into_iter()
                .map(|(func, label, ops, test_decl_ref)| {
                    let selector = func.get_selector(context);
                    let name = func.get_name(context).to_string();
                    AbstractEntry {
                        test_decl_ref,
                        selector,
                        label,
                        ops,
                        name,
                    }
                })
                .collect();

            let abstract_program =
                AbstractProgram::new(kind, data_section, entries, non_entries, reg_seqr);

            if build_config
                .map(|cfg| cfg.print_intermediate_asm)
                .unwrap_or(false)
            {
                println!(";; --- ABSTRACT VIRTUAL PROGRAM ---\n");
                println!("{abstract_program}\n");
            }

            let allocated_program = check!(
                CompileResult::from(abstract_program.into_allocated_program()),
                return err(warnings, errors),
                warnings,
                errors
            );

            if build_config
                .map(|cfg| cfg.print_intermediate_asm)
                .unwrap_or(false)
            {
                println!(";; --- ABSTRACT ALLOCATED PROGRAM ---\n");
                println!("{allocated_program}");
            }

            check!(
                CompileResult::from(allocated_program.into_final_program()),
                return err(warnings, errors),
                warnings,
                errors
            )
        }
        AsmBuilderResult::Evm(result) => FinalProgram::Evm {
            ops: result.ops,
            abi: result.abi,
        },
        AsmBuilderResult::MidenVM(result) => FinalProgram::MidenVM { ops: result.ops },
    };

    ok(final_program, warnings, errors)
}

// -------------------------------------------------------------------------------------------------

#[macro_export]
macro_rules! size_bytes_in_words {
    ($bytes_expr: expr) => {
        ($bytes_expr + 7) / 8
    };
}

// This is a mouthful...
#[macro_export]
macro_rules! size_bytes_round_up_to_word_alignment {
    ($bytes_expr: expr) => {
        ($bytes_expr + 7) - (($bytes_expr + 7) % 8)
    };
}

// NOTE: For stack storage we need to be aware:
// - sizes are in bytes; CFEI reserves in bytes.
// - offsets are in 64-bit words; LW/SW reads/writes to word offsets. XXX Wrap in a WordOffset struct.

#[derive(Clone, Debug)]
pub(super) enum Storage {
    Data(DataId), // Const storage in the data section.
    Stack(u64), // Storage in the runtime stack starting at an absolute word offset.  Essentially a global.
}

pub enum StateAccessType {
    Read,
    Write,
}

pub(crate) fn ir_type_size_in_bytes(context: &Context, ty: &Type) -> u64 {
    ty.size_in_bytes(context)
}

pub(crate) fn ir_type_str_size_in_bytes(context: &Context, ty: &Type) -> u64 {
    match ty.get_content(context) {
        TypeContent::String(n) => *n,
        _ => 0,
    }
}
