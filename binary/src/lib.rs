#![feature(buf_read_has_data_left, int_roundings)]

extern crate alloc;

use crate::utils::sharp::hash_elements;
use alloc::vec::Vec;
use ark_ff::Field;
use ark_ff::PrimeField;
use ark_serialize::CanonicalDeserialize;
use ark_serialize::CanonicalSerialize;
use ark_serialize::Valid;
use digest::Digest;
use num_bigint::BigUint;
use ruint::aliases::U256;
use ruint::uint;
use serde::Deserialize;
use serde::Serialize;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::PathBuf;
use utils::deserialize_hex_str;
use utils::deserialize_hex_str_memory_entries;
use utils::deserialize_vec_hex_str;
use utils::field_bytes;
use utils::OutOfRangeError;

mod utils;

// https://eprint.iacr.org/2021/1063.pdf figure 3
/// Word offset of `off_DST`
pub const OFF_DST_BIT_OFFSET: usize = 0;
/// Word offset of `off_OP0`
pub const OFF_OP0_BIT_OFFSET: usize = 16;
/// Word offset of `off_OP1`
pub const OFF_OP1_BIT_OFFSET: usize = 32;
/// Word offset of instruction flags
pub const FLAGS_BIT_OFFSET: usize = 48;

/// Number of Cairo instruction flags
pub const _NUM_FLAGS: usize = 16;

// Mask for word offsets (16 bits each)
pub const OFF_MASK: usize = 0xFFFF;

pub const _OFFSET: usize = 2usize.pow(16);
pub const HALF_OFFSET: usize = 2usize.pow(15);

/// Holds register values
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegisterState {
    pub ap: usize,
    pub fp: usize,
    pub pc: usize,
}

// TODO: not being used at all ATM
/// https://www.youtube.com/live/jPxD9h7BdzU?feature=share&t=2800
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    Plain = 0,
    Small = 1,
    Dex = 2,
    Recursive = 3,
    Starknet = 4,
    RecursiveLargeOutput = 5,
    AllSolidity = 6,
    StarknetWithKeccak = 7,
}

impl Layout {
    const SHARP_CODE_STARKNET: u64 = 8319381555716711796;

    // Returns the unique code used by SHARP associated to this layout
    pub const fn sharp_code(&self) -> u64 {
        match self {
            Self::Starknet => Self::SHARP_CODE_STARKNET,
            _ => unimplemented!(),
        }
    }

    pub const fn from_sharp_code(code: u64) -> Self {
        match code {
            Self::SHARP_CODE_STARKNET => Self::Starknet,
            _ => unimplemented!(),
        }
    }
}

impl CanonicalSerialize for Layout {
    fn serialize_with_mode<W: ark_serialize::Write>(
        &self,
        writer: W,
        compress: ark_serialize::Compress,
    ) -> Result<(), ark_serialize::SerializationError> {
        self.sharp_code().serialize_with_mode(writer, compress)
    }

    fn serialized_size(&self, _compress: ark_serialize::Compress) -> usize {
        core::mem::size_of::<u64>()
    }
}

impl Valid for Layout {
    fn check(&self) -> Result<(), ark_serialize::SerializationError> {
        Ok(())
    }
}

impl CanonicalDeserialize for Layout {
    fn deserialize_with_mode<R: Read>(
        reader: R,
        compress: ark_serialize::Compress,
        validate: ark_serialize::Validate,
    ) -> Result<Self, ark_serialize::SerializationError> {
        Ok(Self::from_sharp_code(u64::deserialize_with_mode(
            reader, compress, validate,
        )?))
    }
}

#[derive(Debug)]
pub struct RegisterStates(Vec<RegisterState>);

impl RegisterStates {
    /// Parses trace data in the format outputted by a `cairo-run`.
    pub fn from_reader(r: impl Read) -> Self {
        // TODO: errors
        let mut reader = BufReader::new(r);
        let mut register_states = Vec::new();
        while reader.has_data_left().unwrap() {
            let entry: RegisterState = bincode::deserialize_from(&mut reader).unwrap();
            register_states.push(entry);
        }
        RegisterStates(register_states)
    }
}

impl Deref for RegisterStates {
    type Target = Vec<RegisterState>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
pub struct Memory<F>(Vec<Option<Word<F>>>);

impl<F: Field> Memory<F> {
    /// Parses the partial memory data outputted by a `cairo-run`.
    pub fn from_reader(r: impl Read) -> Self
    where
        F: PrimeField,
    {
        // TODO: errors
        // TODO: each builtin has its own memory segment.
        // check it also contains other builtins
        // this file contains the contiguous memory segments:
        // - program
        // - execution
        // - builtin 0
        // - builtin 1
        // - ...
        let mut reader = BufReader::new(r);
        let mut partial_memory = Vec::new();
        let mut max_address = 0;
        let mut word_bytes = Vec::new();
        word_bytes.resize(field_bytes::<F>(), 0);
        while reader.has_data_left().unwrap() {
            // TODO: ensure always deserializes u64 and both are always little-endian
            let address = bincode::deserialize_from(&mut reader).unwrap();
            reader.read_exact(&mut word_bytes).unwrap();
            let word = U256::try_from_le_slice(&word_bytes).unwrap();
            partial_memory.push((address, Word::new(word)));
            max_address = std::cmp::max(max_address, address);
        }

        // TODO: DOC: None used for nondeterministic values?
        let mut memory = vec![None; max_address + 1];
        for (address, word) in partial_memory {
            // TODO: once arkworks v4 release remove num_bigint
            memory[address] = Some(word);
        }

        Memory(memory)
    }
}

impl<F: Field> Deref for Memory<F> {
    type Target = Vec<Option<Word<F>>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MemoryEntry<T> {
    pub address: u32,
    pub value: T,
}

impl<T: CanonicalSerialize> CanonicalSerialize for MemoryEntry<T> {
    fn serialize_with_mode<W: ark_serialize::Write>(
        &self,
        mut writer: W,
        compress: ark_serialize::Compress,
    ) -> Result<(), ark_serialize::SerializationError> {
        self.value.serialize_with_mode(&mut writer, compress)?;
        self.address.serialize_with_mode(writer, compress)
    }

    fn serialized_size(&self, compress: ark_serialize::Compress) -> usize {
        self.value.serialized_size(compress) + self.address.serialized_size(compress)
    }
}

impl MemoryEntry<U256> {
    /// Converts into an equivalent memory entry where the value is a field
    /// element. Returns none if the value is outside the range of the field.
    pub fn try_into_felt_entry<F: PrimeField>(self) -> Option<MemoryEntry<F>> {
        let value = BigUint::from(self.value);
        if value < F::MODULUS.into() {
            Some(MemoryEntry {
                address: self.address,
                value: value.into(),
            })
        } else {
            None
        }
    }
}

impl<T: Valid> Valid for MemoryEntry<T> {
    fn check(&self) -> Result<(), ark_serialize::SerializationError> {
        self.value.check()?;
        self.address.check()
    }
}

impl<T: CanonicalDeserialize> CanonicalDeserialize for MemoryEntry<T> {
    fn deserialize_with_mode<R: Read>(
        mut reader: R,
        compress: ark_serialize::Compress,
        validate: ark_serialize::Validate,
    ) -> Result<Self, ark_serialize::SerializationError> {
        let value = T::deserialize_with_mode(&mut reader, compress, validate)?;
        let address = u32::deserialize_with_mode(reader, compress, validate)?;
        Ok(Self { value, address })
    }
}

#[derive(
    Serialize,
    Deserialize,
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    CanonicalSerialize,
    CanonicalDeserialize,
)]
pub struct Segment {
    pub begin_addr: u32,
    pub stop_ptr: u32,
}

#[derive(Deserialize, Clone, Copy, Debug)]
pub struct MemorySegments {
    pub program: Segment,
    pub execution: Segment,
    pub output: Option<Segment>,
    pub pedersen: Option<Segment>,
    pub range_check: Option<Segment>,
    pub ecdsa: Option<Segment>,
    pub bitwise: Option<Segment>,
    pub ec_op: Option<Segment>,
    pub poseidon: Option<Segment>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct AirPublicInput {
    pub rc_min: u16,
    pub rc_max: u16,
    pub n_steps: u64,
    pub layout: Layout,
    pub memory_segments: MemorySegments,
    #[serde(deserialize_with = "deserialize_hex_str_memory_entries")]
    pub public_memory: Vec<MemoryEntry<U256>>,
}

impl AirPublicInput {
    pub fn initial_pc(&self) -> u32 {
        self.memory_segments.program.begin_addr
    }

    pub fn final_pc(&self) -> u32 {
        self.memory_segments.program.stop_ptr
    }

    pub fn initial_ap(&self) -> u32 {
        self.memory_segments.execution.begin_addr
    }

    pub fn final_ap(&self) -> u32 {
        self.memory_segments.execution.stop_ptr
    }

    pub fn public_memory_padding(&self) -> MemoryEntry<U256> {
        *self.public_memory.iter().find(|e| e.address == 1).unwrap()
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
pub struct Signature {
    #[serde(deserialize_with = "deserialize_hex_str")]
    pub r: U256,
    #[serde(deserialize_with = "deserialize_hex_str")]
    pub w: U256,
}

#[derive(Deserialize, Clone, Copy, Debug)]
pub struct EcdsaInstance {
    pub index: u32,
    #[serde(rename = "pubkey", deserialize_with = "deserialize_hex_str")]
    pub pubkey_x: U256,
    #[serde(rename = "msg", deserialize_with = "deserialize_hex_str")]
    pub message: U256,
    #[serde(rename = "signature_input")]
    pub signature: Signature,
}

impl EcdsaInstance {
    /// Get the memory address for this instance
    /// Output is of the form (pubkey_addr, msg_addr)
    pub fn mem_addr(&self, ecdsa_segment_addr: u32) -> (u32, u32) {
        let instance_offset = ecdsa_segment_addr + self.index * 2;
        (instance_offset, instance_offset + 1)
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
pub struct PedersenInstance {
    pub index: u32,
    #[serde(rename = "x", deserialize_with = "deserialize_hex_str")]
    pub a: U256,
    #[serde(rename = "y", deserialize_with = "deserialize_hex_str")]
    pub b: U256,
}

impl PedersenInstance {
    pub fn new_empty(index: u32) -> Self {
        Self {
            index,
            a: U256::ZERO,
            b: U256::ZERO,
        }
    }

    /// Get the memory address for this instance
    /// Output is of the form (a_addr, b_addr, output_addr)
    pub fn mem_addr(&self, pedersen_segment_addr: u32) -> (u32, u32, u32) {
        let instance_offset = pedersen_segment_addr + self.index * 3;
        (instance_offset, instance_offset + 1, instance_offset + 2)
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
pub struct RangeCheckInstance {
    pub index: u32,
    #[serde(deserialize_with = "deserialize_hex_str")]
    pub value: U256,
}

impl RangeCheckInstance {
    pub fn new_empty(index: u32) -> Self {
        Self {
            index,
            value: U256::ZERO,
        }
    }

    /// Get the memory address for this instance
    pub fn mem_addr(&self, range_check_segment_addr: u32) -> u32 {
        range_check_segment_addr + self.index
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
pub struct BitwiseInstance {
    pub index: u32,
    #[serde(deserialize_with = "deserialize_hex_str")]
    pub x: U256,
    #[serde(deserialize_with = "deserialize_hex_str")]
    pub y: U256,
}

impl BitwiseInstance {
    pub fn new_empty(index: u32) -> Self {
        Self {
            index,
            x: U256::ZERO,
            y: U256::ZERO,
        }
    }

    /// Get the memory address for this instance
    /// Output is of the form (x_addr, y_addr, x&y_addr, x^y_addr, x|y_addr)
    // TODO: better to use struct. Could cause bug if user gets ordering wrong.
    pub fn mem_addr(&self, bitwise_segment_addr: u32) -> (u32, u32, u32, u32, u32) {
        let instance_offset = bitwise_segment_addr + self.index * 5;
        (
            instance_offset,
            instance_offset + 1,
            instance_offset + 2,
            instance_offset + 3,
            instance_offset + 4,
        )
    }
}

/// Elliptic Curve operation instance for `p + m * q` on an elliptic curve
#[derive(Deserialize, Clone, Copy, Debug)]
pub struct EcOpInstance {
    pub index: u32,
    #[serde(deserialize_with = "deserialize_hex_str")]
    pub p_x: U256,
    #[serde(deserialize_with = "deserialize_hex_str")]
    pub p_y: U256,
    #[serde(deserialize_with = "deserialize_hex_str")]
    pub q_x: U256,
    #[serde(deserialize_with = "deserialize_hex_str")]
    pub q_y: U256,
    #[serde(deserialize_with = "deserialize_hex_str")]
    pub m: U256,
}

impl EcOpInstance {
    /// Get the memory address for this instance
    /// Output is of the form (p_x_addr, p_y_addr, q_x_addr, q_y_addr, m_addr,
    /// r_x_addr, r_y_addr)
    pub fn mem_addr(&self, ec_op_segment_addr: u32) -> (u32, u32, u32, u32, u32, u32, u32) {
        let instance_offset = ec_op_segment_addr + self.index * 7;
        (
            instance_offset,
            instance_offset + 1,
            instance_offset + 2,
            instance_offset + 3,
            instance_offset + 4,
            instance_offset + 5,
            instance_offset + 6,
        )
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
pub struct PoseidonInstance {
    pub index: u32,
    #[serde(rename = "input_s0", deserialize_with = "deserialize_hex_str")]
    pub input0: U256,
    #[serde(rename = "input_s1", deserialize_with = "deserialize_hex_str")]
    pub input1: U256,
    #[serde(rename = "input_s2", deserialize_with = "deserialize_hex_str")]
    pub input2: U256,
}

impl PoseidonInstance {
    pub fn new_empty(index: u32) -> Self {
        Self {
            index,
            input0: U256::ZERO,
            input1: U256::ZERO,
            input2: U256::ZERO,
        }
    }

    /// Get the memory address for this instance
    /// Output is of the form (input0_addr, input1_addr, input2_addr,
    /// output0_addr, output1_addr, output2_addr)
    pub fn mem_addr(&self, poseidon_segment_addr: u32) -> (u32, u32, u32, u32, u32, u32) {
        let instance_offset = poseidon_segment_addr + self.index * 6;
        (
            instance_offset,
            instance_offset + 1,
            instance_offset + 2,
            instance_offset + 3,
            instance_offset + 4,
            instance_offset + 5,
        )
    }
}

#[derive(Debug, Deserialize)]
pub struct AirPrivateInput {
    pub trace_path: PathBuf,
    pub memory_path: PathBuf,
    pub pedersen: Vec<PedersenInstance>,
    pub ecdsa: Vec<EcdsaInstance>,
    pub range_check: Vec<RangeCheckInstance>,
    pub bitwise: Vec<BitwiseInstance>,
    pub ec_op: Vec<EcOpInstance>,
    pub poseidon: Vec<PoseidonInstance>,
}

#[derive(Clone, Deserialize, Debug)]
pub struct CompiledProgram {
    #[serde(deserialize_with = "deserialize_vec_hex_str")]
    pub data: Vec<U256>,
    pub prime: String,
}

impl CompiledProgram {
    // TODO: could use https://github.com/Keats/validator instead of calling this everywhere
    // but seems a bit heave to add as a dependency just to do this
    // TODO: is this even being used. maybe remove
    pub fn validate<F: PrimeField>(&self) {
        // Make sure the field modulus matches the expected
        let modulus: BigUint = F::MODULUS.into();
        assert_eq!(format!("{:#x}", modulus), self.prime.to_lowercase());
    }

    pub fn program_memory<F: PrimeField>(&self) -> Vec<MemoryEntry<F>> {
        self.validate::<F>();
        self.data
            .iter()
            .enumerate()
            .map(|(i, &value)| {
                // address 0 is reserved for dummy accesses
                MemoryEntry {
                    address: i as u32 + 1,
                    value: Word::new(value).into_felt(),
                }
            })
            .collect()
    }
}

/// Represents a Cairo word
/// Value is a field element in the range `[0, Fp::MODULUS)`
/// Stored as a U256 to make binary decompositions more efficient
#[derive(Clone, Copy, Debug)]
pub struct Word<F>(pub U256, PhantomData<F>);

impl<F> Word<F> {
    /// Calculates $\tilde{f_i}$ - https://eprint.iacr.org/2021/1063.pdf
    pub fn get_flag_prefix(&self, flag: Flag) -> u16 {
        if flag == Flag::Zero {
            return 0;
        }

        let flag = flag as usize;
        let prefix = self.0 >> (FLAGS_BIT_OFFSET + flag);
        let mask = (uint!(1_U256) << (15 - flag)) - uint!(1_U256);
        (prefix & mask).try_into().unwrap()
    }

    pub fn get_op0_addr(&self, ap: usize, fp: usize) -> usize {
        // TODO: put the if statement first good for rust quiz
        self.get_off_op0() as usize + if self.get_flag(Flag::Op0Reg) { fp } else { ap }
            - HALF_OFFSET
    }

    pub fn get_dst_addr(&self, ap: usize, fp: usize) -> usize {
        self.get_off_dst() as usize + if self.get_flag(Flag::DstReg) { fp } else { ap }
            - HALF_OFFSET
    }

    pub fn get_flag(&self, flag: Flag) -> bool {
        self.0.bit(FLAGS_BIT_OFFSET + flag as usize)
    }

    pub fn get_off_dst(&self) -> u16 {
        let prefix = self.0 >> OFF_DST_BIT_OFFSET;
        let mask = U256::from(OFF_MASK);
        (prefix & mask).try_into().unwrap()
    }

    pub fn get_off_op0(&self) -> u16 {
        let prefix = self.0 >> OFF_OP0_BIT_OFFSET;
        let mask = U256::from(OFF_MASK);
        (prefix & mask).try_into().unwrap()
    }

    pub fn get_off_op1(&self) -> u16 {
        let prefix = self.0 >> OFF_OP1_BIT_OFFSET;
        let mask = U256::from(OFF_MASK);
        (prefix & mask).try_into().unwrap()
    }

    pub fn get_flag_group(&self, flag_group: FlagGroup) -> u8 {
        match flag_group {
            FlagGroup::DstReg => self.get_flag(Flag::DstReg) as u8,
            FlagGroup::Op0Reg => self.get_flag(Flag::Op0Reg) as u8,
            FlagGroup::Op1Src => {
                self.get_flag(Flag::Op1Imm) as u8
                    + self.get_flag(Flag::Op1Fp) as u8 * 2
                    + self.get_flag(Flag::Op1Ap) as u8 * 4
            }
            FlagGroup::ResLogic => {
                self.get_flag(Flag::ResAdd) as u8 + self.get_flag(Flag::ResMul) as u8 * 2
            }
            FlagGroup::PcUpdate => {
                self.get_flag(Flag::PcJumpAbs) as u8
                    + self.get_flag(Flag::PcJumpRel) as u8 * 2
                    + self.get_flag(Flag::PcJnz) as u8 * 4
            }
            FlagGroup::ApUpdate => {
                self.get_flag(Flag::ApAdd) as u8 + self.get_flag(Flag::ApAdd1) as u8 * 2
            }
            FlagGroup::Opcode => {
                self.get_flag(Flag::OpcodeCall) as u8
                    + self.get_flag(Flag::OpcodeRet) as u8 * 2
                    + self.get_flag(Flag::OpcodeAssertEq) as u8 * 4
            }
        }
    }
}

impl<F: PrimeField> Word<F> {
    pub fn new(word: U256) -> Self {
        let modulus: BigUint = F::MODULUS.into();
        debug_assert!(BigUint::from(word) < modulus);
        Word(word, PhantomData)
    }

    pub fn get_op0(&self, ap: usize, fp: usize, mem: &Memory<F>) -> F {
        mem[self.get_op0_addr(ap, fp)].unwrap().into_felt()
    }

    pub fn get_dst(&self, ap: usize, fp: usize, mem: &Memory<F>) -> F {
        mem[self.get_dst_addr(ap, fp)].unwrap().into_felt()
    }

    pub fn get_op1_addr(&self, pc: usize, ap: usize, fp: usize, mem: &Memory<F>) -> usize {
        self.get_off_op1() as usize
            + match self.get_flag_group(FlagGroup::Op1Src) {
                0 => usize::try_from(mem[self.get_op0_addr(ap, fp)].unwrap().0).unwrap(),
                1 => pc,
                2 => fp,
                4 => ap,
                _ => unreachable!(),
            }
            - HALF_OFFSET
    }

    pub fn get_op1(&self, pc: usize, ap: usize, fp: usize, mem: &Memory<F>) -> F {
        mem[self.get_op1_addr(pc, ap, fp, mem)].unwrap().into_felt()
    }

    pub fn get_res(&self, pc: usize, ap: usize, fp: usize, mem: &Memory<F>) -> F {
        let pc_update = self.get_flag_group(FlagGroup::PcUpdate);
        let res_logic = self.get_flag_group(FlagGroup::ResLogic);
        match pc_update {
            4 => {
                let opcode = self.get_flag_group(FlagGroup::Opcode);
                let ap_update = self.get_flag_group(FlagGroup::ApUpdate);
                if res_logic == 0 && opcode == 0 && ap_update != 1 {
                    // From the Cairo whitepaper "We use the term Unused to
                    // describe a variable that will not be used later in the
                    // flow. As such, we don’t need to assign it a concrete
                    // value.". Note `res` is repurposed when calculating next_pc and
                    // stores the value of `dst^(-1)` (see air.rs for more details).
                    self.get_dst(ap, fp, mem).inverse().unwrap_or_else(F::zero)
                } else {
                    unreachable!()
                }
            }
            0 | 1 | 2 => {
                let op0: F = mem[self.get_op0_addr(ap, fp)].unwrap().into_felt();
                let op1: F = mem[self.get_op1_addr(pc, ap, fp, mem)].unwrap().into_felt();
                match res_logic {
                    0 => op1,
                    1 => op0 + op1,
                    2 => op0 * op1,
                    _ => unreachable!(),
                }
            }
            _ => unreachable!(),
        }
    }

    pub fn get_tmp0(&self, ap: usize, fp: usize, mem: &Memory<F>) -> F {
        if self.get_flag(Flag::PcJnz) {
            self.get_dst(ap, fp, mem)
        } else {
            // TODO: change
            F::zero()
        }
    }

    pub fn get_tmp1(&self, pc: usize, ap: usize, fp: usize, mem: &Memory<F>) -> F {
        self.get_tmp0(ap, fp, mem) * self.get_res(pc, ap, fp, mem)
    }

    pub fn into_felt(self) -> F {
        BigUint::from(self.0).into()
    }
}

// Section 9.2 https://eprint.iacr.org/2021/1063.pdf
// TODO: might need to have an type info per layout
#[derive(Debug, CanonicalSerialize, CanonicalDeserialize, Clone, PartialEq, Eq)]
pub struct CairoAuxInput<F: Field> {
    pub log_n_steps: u32,
    pub layout: Layout,
    pub initial_ap: F,
    pub initial_pc: F,
    pub final_ap: F,
    pub final_pc: F,
    pub range_check_min: u16,
    pub range_check_max: u16,
    pub public_memory_padding: MemoryEntry<F>,
    pub program_segment: Segment,
    pub execution_segment: Segment,
    pub output_segment: Option<Segment>,
    pub pedersen_segment: Option<Segment>,
    pub rc_segment: Option<Segment>,
    pub ecdsa_segment: Option<Segment>,
    pub bitwise_segment: Option<Segment>,
    pub ec_op_segment: Option<Segment>,
    pub poseidon_segment: Option<Segment>,
    pub public_memory: Vec<MemoryEntry<F>>,
}

impl<F: PrimeField> TryFrom<AirPublicInput> for CairoAuxInput<F> {
    // TODO: proper error
    type Error = OutOfRangeError;

    fn try_from(value: AirPublicInput) -> Result<Self, OutOfRangeError> {
        todo!()
    }
}

impl<F: Field> CairoAuxInput<F> {
    /// Serializes the data to be compatible with StarkWare's solidity verifier
    pub fn serialize_sharp<D: Digest>(&self) -> Vec<U256>
    where
        F: PrimeField,
    {
        const OFFSET_LOG_N_STEPS: usize = 0;
        const OFFSET_RC_MIN: usize = 1;
        const OFFSET_RC_MAX: usize = 2;
        const OFFSET_LAYOUT_CODE: usize = 3;
        const OFFSET_PROGRAM_BEGIN_ADDR: usize = 4;
        const OFFSET_PROGRAM_STOP_PTR: usize = 5;
        const OFFSET_EXECUTION_BEGIN_ADDR: usize = 6;
        const OFFSET_EXECUTION_STOP_PTR: usize = 7;
        const OFFSET_OUTPUT_BEGIN_ADDR: usize = 8;
        const OFFSET_OUTPUT_STOP_PTR: usize = 9;
        const OFFSET_PEDERSEN_BEGIN_ADDR: usize = 10;
        const OFFSET_PEDERSEN_STOP_PTR: usize = 11;
        const OFFSET_RANGE_CHECK_BEGIN_ADDR: usize = 12;
        const OFFSET_RANGE_CHECK_STOP_PTR: usize = 13;

        const NUM_BASE_VALS: usize = OFFSET_RANGE_CHECK_STOP_PTR + 1;
        let mut base_vals = vec![None; NUM_BASE_VALS];
        base_vals[OFFSET_LOG_N_STEPS] = Some(U256::from(self.log_n_steps));
        base_vals[OFFSET_RC_MIN] = Some(U256::from(self.range_check_min));
        base_vals[OFFSET_RC_MAX] = Some(U256::from(self.range_check_max));
        base_vals[OFFSET_LAYOUT_CODE] = Some(U256::from(self.layout.sharp_code()));
        base_vals[OFFSET_PROGRAM_BEGIN_ADDR] = Some(U256::from(self.program_segment.begin_addr));
        base_vals[OFFSET_PROGRAM_STOP_PTR] = Some(U256::from(self.program_segment.stop_ptr));
        base_vals[OFFSET_EXECUTION_BEGIN_ADDR] =
            Some(U256::from(self.execution_segment.begin_addr));
        base_vals[OFFSET_EXECUTION_STOP_PTR] = Some(U256::from(self.execution_segment.stop_ptr));
        base_vals[OFFSET_OUTPUT_BEGIN_ADDR] = self.output_segment.map(|s| U256::from(s.begin_addr));
        base_vals[OFFSET_OUTPUT_STOP_PTR] = self.output_segment.map(|s| U256::from(s.stop_ptr));
        base_vals[OFFSET_PEDERSEN_BEGIN_ADDR] =
            self.pedersen_segment.map(|s| U256::from(s.begin_addr));
        base_vals[OFFSET_PEDERSEN_STOP_PTR] = self.pedersen_segment.map(|s| U256::from(s.stop_ptr));
        base_vals[OFFSET_RANGE_CHECK_BEGIN_ADDR] =
            self.rc_segment.map(|s| U256::from(s.begin_addr));
        base_vals[OFFSET_RANGE_CHECK_STOP_PTR] = self.rc_segment.map(|s| U256::from(s.stop_ptr));

        let layout_vals = match self.layout {
            Layout::Starknet => {
                const OFFSET_ECDSA_BEGIN_ADDR: usize = 0;
                const OFFSET_ECDSA_STOP_PTR: usize = 1;
                const OFFSET_BITWISE_BEGIN_ADDR: usize = 2;
                const OFFSET_BITWISE_STOP_ADDR: usize = 3;
                const OFFSET_EC_OP_BEGIN_ADDR: usize = 4;
                const OFFSET_EC_OP_STOP_ADDR: usize = 5;
                const OFFSET_POSEIDON_BEGIN_ADDR: usize = 6;
                const OFFSET_POSEIDON_STOP_PTR: usize = 7;
                const OFFSET_PUBLIC_MEMORY_PADDING_ADDR: usize = 8;
                const OFFSET_PUBLIC_MEMORY_PADDING_VALUE: usize = 9;
                const OFFSET_N_PUBLIC_MEMORY_PAGES: usize = 10;

                const NUM_VALS: usize = OFFSET_N_PUBLIC_MEMORY_PAGES + 1;
                let mut vals = vec![None; NUM_VALS];
                vals[OFFSET_ECDSA_BEGIN_ADDR] =
                    self.ecdsa_segment.map(|s| U256::from(s.begin_addr));
                vals[OFFSET_ECDSA_STOP_PTR] = self.ecdsa_segment.map(|s| U256::from(s.stop_ptr));
                vals[OFFSET_BITWISE_BEGIN_ADDR] =
                    self.bitwise_segment.map(|s| U256::from(s.begin_addr));
                vals[OFFSET_BITWISE_STOP_ADDR] =
                    self.bitwise_segment.map(|s| U256::from(s.stop_ptr));
                vals[OFFSET_EC_OP_BEGIN_ADDR] =
                    self.ec_op_segment.map(|s| U256::from(s.begin_addr));
                vals[OFFSET_EC_OP_STOP_ADDR] = self.ec_op_segment.map(|s| U256::from(s.stop_ptr));
                vals[OFFSET_POSEIDON_BEGIN_ADDR] =
                    self.poseidon_segment.map(|s| U256::from(s.begin_addr));
                vals[OFFSET_POSEIDON_STOP_PTR] =
                    self.poseidon_segment.map(|s| U256::from(s.stop_ptr));
                vals[OFFSET_PUBLIC_MEMORY_PADDING_ADDR] =
                    Some(U256::from(self.public_memory_padding.address));
                vals[OFFSET_PUBLIC_MEMORY_PADDING_VALUE] = Some(U256::from::<BigUint>(
                    self.public_memory_padding.value.into(),
                ));
                // Only 1 memory page currently for the main memory page
                // TODO: support more memory pages
                vals[OFFSET_N_PUBLIC_MEMORY_PAGES] = Some(uint!(1_U256));
                vals
            }
            _ => unimplemented!(),
        };

        // The public memory consists of individual memory pages.
        // The first page is for main memory.
        // For each page:
        // * First address in the page (this field is not included for the first page).
        // * Page size. (number of memory pairs)
        // * Page hash (hash of memory pairs)
        // TODO: support other memory pages
        let public_memory = {
            const _PAGE_INFO_ADDRESS_OFFSET: usize = 0;
            const _PAGE_INFO_SIZE_OFFSET: usize = 1;
            const _PAGE_INFO_HASH_OFFSET: usize = 2;

            // Hash the address value pairs of the main memory page
            let main_page_hash: [u8; 32] = {
                let pairs = self
                    .public_memory
                    .iter()
                    .flat_map(|e| [e.address.into(), e.value])
                    .collect::<Vec<F>>();

                let mut hasher = D::new();
                hash_elements(&mut hasher, &pairs);
                (*hasher.finalize()).try_into().unwrap()
            };

            // NOTE: no address main memory page because It's implicitly "1".
            let mut main_page = vec![None; 2];
            main_page[0] = Some(U256::from(self.public_memory.len()));
            main_page[1] = Some(U256::try_from_be_slice(&main_page_hash).unwrap());

            main_page
        };

        [base_vals, layout_vals, public_memory]
            .into_iter()
            .flatten()
            // ensure there are no unfilled gaps
            .map(Option::unwrap)
            .collect()
    }
}

/// Cairo flag group
/// https://eprint.iacr.org/2021/1063.pdf section 9.4
#[derive(Clone, Copy)]
pub enum FlagGroup {
    DstReg,
    Op0Reg,
    Op1Src,
    ResLogic,
    PcUpdate,
    ApUpdate,
    Opcode,
}

/// Cairo flag
/// https://eprint.iacr.org/2021/1063.pdf section 9
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Flag {
    // Group: [FlagGroup::DstReg]
    DstReg = 0,

    // Group: [FlagGroup::Op0]
    Op0Reg = 1,

    // Group: [FlagGroup::Op1Src]
    Op1Imm = 2,
    Op1Fp = 3,
    Op1Ap = 4,

    // Group: [FlagGroup::ResLogic]
    ResAdd = 5,
    ResMul = 6,

    // Group: [FlagGroup::PcUpdate]
    PcJumpAbs = 7,
    PcJumpRel = 8,
    PcJnz = 9,

    // Group: [FlagGroup::ApUpdate]
    ApAdd = 10,
    ApAdd1 = 11,

    // Group: [FlagGroup::Opcode]
    OpcodeCall = 12,
    OpcodeRet = 13,
    OpcodeAssertEq = 14,

    // 0 - padding to make flag cells a power-of-2
    Zero = 15,
}
