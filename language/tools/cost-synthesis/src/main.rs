// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

//! This performs instruction cost synthesis for the various bytecode instructions that we have. We
//! separate instructions into three sets:
//! * Global-memory independent instructions;
//! * Global-memory dependent instructions; and
//! * Native operations.
use cost_synthesis::{
    global_state::{account::Account, inhabitor::RandomInhabitor},
    module_generator::generate_padded_modules,
    natives::StackAccessorMocker,
    stack_generator::RandomStackGenerator,
};
use language_e2e_tests::data_store::FakeDataStore;
use libra_types::vm_error::StatusCode;
use move_vm_runtime::{
    interpreter::InterpreterForCostSynthesis,
    loaded_data::function::{FunctionRef, FunctionReference},
    MoveVM,
};
use move_vm_state::execution_context::SystemExecutionContext;
use move_vm_types::{native_functions::hash, values::Value};
use std::{
    collections::{HashMap, VecDeque},
    convert::TryFrom,
    path::Path,
    time::Instant,
    u64,
};
use stdlib::env_stdlib_modules;
use structopt::StructOpt;
use vm::{
    access::ModuleAccess,
    file_format::{
        AddressPoolIndex, ByteArrayPoolIndex, Bytecode, FieldHandleIndex, FunctionDefinitionIndex,
        FunctionHandleIndex, StructDefinitionIndex,
    },
    gas_schedule::{AbstractMemorySize, CostTable, GasAlgebra, GasCarrier, GasUnits},
    transaction_metadata::TransactionMetadata,
};

#[derive(Debug, StructOpt)]
#[structopt(
    name = "Instruction Cost Synthesis",
    about = "Cost synthesis parameter settings"
)]
struct Opt {
    /// The number of iterations each instruction should be run for.
    #[structopt(short = "i", long = "num-iters", default_value = "10000")]
    num_iters: u16,

    /// The maximum stack size generated.
    #[structopt(short = "ms", long = "max-stack-size", default_value = "100")]
    max_stack_size: u64,

    #[structopt(short = "o", long = "output")]
    output: bool,
}

fn output_to_csv(path: &Path, data: HashMap<String, Vec<u64>>, output: bool) {
    if output {
        let mut writer = csv::Writer::from_path(path).unwrap();
        let keys: Vec<_> = data.keys().collect();
        let datavars: Vec<_> = data.values().collect();
        writer.write_record(&keys).unwrap();
        for i in 0..datavars[0].len() {
            let row: Vec<_> = datavars.iter().map(|v| v[i]).collect();
            writer.serialize(&row).unwrap();
        }

        writer.flush().unwrap();
    }
}

fn size_normalize_cost(instr: &Bytecode, cost: u64, size: AbstractMemorySize<GasCarrier>) -> u64 {
    match instr {
        Bytecode::MoveToSender(_)
        | Bytecode::Exists(_)
        | Bytecode::MutBorrowGlobal(_)
        | Bytecode::ImmBorrowGlobal(_)
        | Bytecode::Eq
        | Bytecode::Neq
        | Bytecode::LdByteArray(_)
        | Bytecode::StLoc(_)
        | Bytecode::CopyLoc(_)
        | Bytecode::Pack(_)
        | Bytecode::Unpack(_)
        | Bytecode::WriteRef
        | Bytecode::ReadRef
        | Bytecode::MoveFrom(_) => cost / size.get() + if cost % size.get() == 0 { 0 } else { 1 },
        _ => cost,
    }
}

fn stack_instructions(options: &Opt) {
    use Bytecode::*;
    let stack_opcodes: Vec<Bytecode> = vec![
        ReadRef,
        WriteRef,
        FreezeRef,
        MoveToSender(StructDefinitionIndex(0)),
        Exists(StructDefinitionIndex(0)),
        MutBorrowGlobal(StructDefinitionIndex(0)),
        ImmBorrowGlobal(StructDefinitionIndex(0)),
        MoveFrom(StructDefinitionIndex(0)),
        MutBorrowField(FieldHandleIndex(0)),
        ImmBorrowField(FieldHandleIndex(0)),
        CopyLoc(0),
        MoveLoc(0),
        MutBorrowLoc(0),
        ImmBorrowLoc(0),
        StLoc(0),
        Unpack(StructDefinitionIndex(0)),
        Pack(StructDefinitionIndex(0)),
        Call(FunctionHandleIndex(0)),
        Sub,
        Ret,
        Add,
        Mul,
        Mod,
        Div,
        BitOr,
        BitAnd,
        Xor,
        Shl,
        Shr,
        Or,
        And,
        Eq,
        Neq,
        Not,
        Lt,
        Gt,
        Le,
        Ge,
        Abort,
        LdFalse,
        LdTrue,
        LdU64(0),
        LdByteArray(ByteArrayPoolIndex(0)),
        LdAddr(AddressPoolIndex(0)),
        BrFalse(0),
        BrTrue(0),
        Branch(0),
        Pop,
        GetTxnGasUnitPrice,
        GetTxnMaxGasUnits,
        GetGasRemaining,
        GetTxnSenderAddress,
        GetTxnSequenceNumber,
        GetTxnPublicKey,
    ];

    let mut account = Account::new();

    // create a set of modules to work with on top of stdlib.
    // The root module is the module based upon how we generate modules.
    let mut modules = env_stdlib_modules().to_vec();
    let (root, mut callee_modules) = generate_padded_modules(3, options.num_iters as usize);
    modules.append(&mut callee_modules);
    let module_id = root.self_id();
    modules.push(root);

    // create a Move VM and populate it with generated modules
    let move_vm = MoveVM::new();
    for m in modules.clone() {
        move_vm.cache_module(m);
    }

    // create an InterpreterContext for runtime operations
    let mut data_cache = FakeDataStore::default();
    let ctx = SystemExecutionContext::new(&data_cache, GasUnits::new(0));

    // get the root LoadedModule
    let loaded_module = move_vm
        .get_loaded_module(&module_id, &ctx)
        .expect("[Module Lookup] Runtime error while looking up module");

    // create the inhabitor to build the resources to be published
    let mut inhabitor = RandomInhabitor::new(&loaded_module, &move_vm, &ctx);
    account.modules = modules;
    for (access_path, blob) in account.generate_resources(&mut inhabitor).into_iter() {
        data_cache.set(access_path, blob);
    }

    // make an Interpreter for execution
    let gas_schedule = CostTable::zero();
    let txn_data = TransactionMetadata::default();
    let mut vm = InterpreterForCostSynthesis::new(&txn_data, &gas_schedule);

    // load the entry point
    let entry_idx = FunctionDefinitionIndex(0);
    let entry_func = FunctionRef::new(&loaded_module, entry_idx);
    vm.push_frame(entry_func, vec![]);

    let costs: HashMap<String, Vec<u64>> = stack_opcodes
        .into_iter()
        .map(|instruction| {
            println!("Running: {:?}", instruction);
            let mut stack_gen = RandomStackGenerator::new(
                &account.addr,
                &loaded_module,
                &move_vm,
                SystemExecutionContext::new(&data_cache, GasUnits::new(0)),
                &instruction,
                options.max_stack_size,
                options.num_iters,
            );

            let mut instr_costs: Vec<u64> = vec![];
            while let Some(stack_state) = stack_gen.next_stack() {
                let (instr, size) = RandomStackGenerator::stack_transition(&mut vm, stack_state);
                let before = Instant::now();
                let ignore = stack_gen.execute_code_snippet(&mut vm, &[instr]);
                //let ignore = vm.execute_code_snippet(&move_vm, &mut ctx, &[instr]);
                let time = before.elapsed().as_nanos();
                // Check to make sure we didn't error. Need to special case the abort bytecode.
                if instruction != Bytecode::Abort {
                    // We want any errors here to bubble up to us with the actual VM error.
                    ignore.unwrap();
                } else {
                    // In the case of the Abort bytecode we want to only make sure that we
                    // don't have a VMInvariantViolation error, and then make sure that the any
                    // error generated was an abort failure.
                    match ignore {
                        Ok(_) => (),
                        Err(err) => match err.major_status {
                            StatusCode::ABORTED => (),
                            _ => panic!("Abort bytecode failed"),
                        },
                    }
                }
                instr_costs.push(size_normalize_cost(
                    &instruction,
                    u64::try_from(time).unwrap(),
                    size,
                ));
            }
            (format!("{:?}", instruction), instr_costs)
        })
        .collect();

    output_to_csv(
        Path::new("data/bytecode_instruction_costs.csv"),
        costs,
        options.output,
    );
}

macro_rules! bench_native {
    ($name:expr, $function:path, $table:ident, $iters:expr) => {
        let mut stack_access = StackAccessorMocker::new();
        let cost_table = CostTable::zero();
        let per_byte_costs: Vec<u64> = (1..512)
            .map(|i| {
                stack_access.set_hash_length(i);
                let time = (0..$iters).fold(0, |acc, _| {
                    let before = Instant::now();
                    let mut args = VecDeque::new();
                    args.push_front(Value::vector_u8(stack_access.next_bytearray()));
                    let _ = $function(vec![], args, &cost_table);
                    acc + before.elapsed().as_nanos()
                });
                // Time per byte averaged over the number of iterations that we performed.
                u64::try_from(time).unwrap() / (u64::from($iters) * (i as u64))
            })
            .collect();
        $table.insert($name, per_byte_costs);
    };
}

fn natives(options: &Opt) {
    let mut cost_table = HashMap::new();
    bench_native!(
        "native_sha2_256".to_string(),
        hash::native_sha2_256,
        cost_table,
        options.num_iters
    );
    bench_native!(
        "native_sha3_256".to_string(),
        hash::native_sha3_256,
        cost_table,
        options.num_iters
    );
    output_to_csv(
        Path::new("data/native_function_costs.csv"),
        cost_table,
        options.output,
    );
}

pub fn main() {
    let opt = Opt::from_args();
    stack_instructions(&opt);
    natives(&opt);
}
