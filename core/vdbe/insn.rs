use std::num::NonZero;

use super::{
    cast_text_to_numeric, execute, AggFunc, BranchOffset, CursorID, FuncCtx, InsnFunction, PageIdx,
};
use crate::storage::wal::CheckpointMode;
use crate::types::{OwnedValue, Record};
use limbo_macros::Description;

/// Flags provided to comparison instructions (e.g. Eq, Ne) which determine behavior related to NULL values.
#[derive(Clone, Copy, Debug, Default)]
pub struct CmpInsFlags(usize);

impl CmpInsFlags {
    const NULL_EQ: usize = 0x80;
    const JUMP_IF_NULL: usize = 0x10;

    fn has(&self, flag: usize) -> bool {
        (self.0 & flag) != 0
    }

    pub fn null_eq(mut self) -> Self {
        self.0 |= CmpInsFlags::NULL_EQ;
        self
    }

    pub fn jump_if_null(mut self) -> Self {
        self.0 |= CmpInsFlags::JUMP_IF_NULL;
        self
    }

    pub fn has_jump_if_null(&self) -> bool {
        self.has(CmpInsFlags::JUMP_IF_NULL)
    }

    pub fn has_nulleq(&self) -> bool {
        self.has(CmpInsFlags::NULL_EQ)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IdxInsertFlags(pub u8);
impl IdxInsertFlags {
    pub const APPEND: u8 = 0x01; // Hint: insert likely at the end
    pub const NCHANGE: u8 = 0x02; // Increment the change counter
    pub const USE_SEEK: u8 = 0x04; // Skip seek if last one was same key
    pub fn new() -> Self {
        IdxInsertFlags(0)
    }
    pub fn has(&self, flag: u8) -> bool {
        (self.0 & flag) != 0
    }
    pub fn append(mut self, append: bool) -> Self {
        if append {
            self.0 |= IdxInsertFlags::APPEND;
        } else {
            self.0 &= !IdxInsertFlags::APPEND;
        }
        self
    }
    pub fn use_seek(mut self, seek: bool) -> Self {
        if seek {
            self.0 |= IdxInsertFlags::USE_SEEK;
        } else {
            self.0 &= !IdxInsertFlags::USE_SEEK;
        }
        self
    }
    pub fn nchange(mut self, change: bool) -> Self {
        if change {
            self.0 |= IdxInsertFlags::NCHANGE;
        } else {
            self.0 &= !IdxInsertFlags::NCHANGE;
        }
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub enum RegisterOrLiteral<T: Copy> {
    Register(usize),
    Literal(T),
}

impl From<PageIdx> for RegisterOrLiteral<PageIdx> {
    fn from(value: PageIdx) -> Self {
        RegisterOrLiteral::Literal(value)
    }
}

#[derive(Description, Debug)]
pub enum Insn {
    /// Initialize the program state and jump to the given PC.
    Init {
        target_pc: BranchOffset,
    },
    /// Write a NULL into register dest. If dest_end is Some, then also write NULL into register dest_end and every register in between dest and dest_end. If dest_end is not set, then only register dest is set to NULL.
    Null {
        dest: usize,
        dest_end: Option<usize>,
    },
    /// Move the cursor P1 to a null row. Any Column operations that occur while the cursor is on the null row will always write a NULL.
    NullRow {
        cursor_id: CursorID,
    },
    /// Add two registers and store the result in a third register.
    Add {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// Subtract rhs from lhs and store in dest
    Subtract {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// Multiply two registers and store the result in a third register.
    Multiply {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// Divide lhs by rhs and store the result in a third register.
    Divide {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// Compare two vectors of registers in reg(P1)..reg(P1+P3-1) (call this vector "A") and in reg(P2)..reg(P2+P3-1) ("B"). Save the result of the comparison for use by the next Jump instruct.
    Compare {
        start_reg_a: usize,
        start_reg_b: usize,
        count: usize,
    },
    /// Place the result of rhs bitwise AND lhs in third register.
    BitAnd {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// Place the result of rhs bitwise OR lhs in third register.
    BitOr {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// Place the result of bitwise NOT register P1 in dest register.
    BitNot {
        reg: usize,
        dest: usize,
    },
    /// Checkpoint the database (applying wal file content to database file).
    Checkpoint {
        database: usize,                 // checkpoint database P1
        checkpoint_mode: CheckpointMode, // P2 checkpoint mode
        dest: usize,                     // P3 checkpoint result
    },
    /// Divide lhs by rhs and place the remainder in dest register.
    Remainder {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// Jump to the instruction at address P1, P2, or P3 depending on whether in the most recent Compare instruction the P1 vector was less than, equal to, or greater than the P2 vector, respectively.
    Jump {
        target_pc_lt: BranchOffset,
        target_pc_eq: BranchOffset,
        target_pc_gt: BranchOffset,
    },
    /// Move the P3 values in register P1..P1+P3-1 over into registers P2..P2+P3-1. Registers P1..P1+P3-1 are left holding a NULL. It is an error for register ranges P1..P1+P3-1 and P2..P2+P3-1 to overlap. It is an error for P3 to be less than 1.
    Move {
        source_reg: usize,
        dest_reg: usize,
        count: usize,
    },
    /// If the given register is a positive integer, decrement it by decrement_by and jump to the given PC.
    IfPos {
        reg: usize,
        target_pc: BranchOffset,
        decrement_by: usize,
    },
    /// If the given register is not NULL, jump to the given PC.
    NotNull {
        reg: usize,
        target_pc: BranchOffset,
    },
    /// Compare two registers and jump to the given PC if they are equal.
    Eq {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
        /// CmpInsFlags are nulleq (null = null) or jump_if_null.
        ///
        /// jump_if_null jumps if either of the operands is null. Used for "jump when false" logic.
        /// Eg. "SELECT * FROM users WHERE id = NULL" becomes:
        /// <JUMP TO NEXT ROW IF id != NULL>
        /// Without the jump_if_null flag it would not jump because the logical comparison "id != NULL" is never true.
        /// This flag indicates that if either is null we should still jump.
        flags: CmpInsFlags,
    },
    /// Compare two registers and jump to the given PC if they are not equal.
    Ne {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
        /// CmpInsFlags are nulleq (null = null) or jump_if_null.
        ///
        /// jump_if_null jumps if either of the operands is null. Used for "jump when false" logic.
        flags: CmpInsFlags,
    },
    /// Compare two registers and jump to the given PC if the left-hand side is less than the right-hand side.
    Lt {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
        /// jump_if_null: Jump if either of the operands is null. Used for "jump when false" logic.
        flags: CmpInsFlags,
    },
    // Compare two registers and jump to the given PC if the left-hand side is less than or equal to the right-hand side.
    Le {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
        /// jump_if_null: Jump if either of the operands is null. Used for "jump when false" logic.
        flags: CmpInsFlags,
    },
    /// Compare two registers and jump to the given PC if the left-hand side is greater than the right-hand side.
    Gt {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
        /// jump_if_null: Jump if either of the operands is null. Used for "jump when false" logic.
        flags: CmpInsFlags,
    },
    /// Compare two registers and jump to the given PC if the left-hand side is greater than or equal to the right-hand side.
    Ge {
        lhs: usize,
        rhs: usize,
        target_pc: BranchOffset,
        /// jump_if_null: Jump if either of the operands is null. Used for "jump when false" logic.
        flags: CmpInsFlags,
    },
    /// Jump to target_pc if r\[reg\] != 0 or (r\[reg\] == NULL && r\[jump_if_null\] != 0)
    If {
        reg: usize,              // P1
        target_pc: BranchOffset, // P2
        /// P3. If r\[reg\] is null, jump iff r\[jump_if_null\] != 0
        jump_if_null: bool,
    },
    /// Jump to target_pc if r\[reg\] != 0 or (r\[reg\] == NULL && r\[jump_if_null\] != 0)
    IfNot {
        reg: usize,              // P1
        target_pc: BranchOffset, // P2
        /// P3. If r\[reg\] is null, jump iff r\[jump_if_null\] != 0
        jump_if_null: bool,
    },
    /// Open a cursor for reading.
    OpenReadAsync {
        cursor_id: CursorID,
        root_page: PageIdx,
    },

    /// Await for the completion of open cursor.
    OpenReadAwait,

    /// Open a cursor for a virtual table.
    VOpenAsync {
        cursor_id: CursorID,
    },

    /// Await for the completion of open cursor for a virtual table.
    VOpenAwait,

    /// Create a new virtual table.
    VCreate {
        module_name: usize, // P1: Name of the module that contains the virtual table implementation
        table_name: usize,  // P2: Name of the virtual table
        args_reg: Option<usize>,
    },

    /// Initialize the position of the virtual table cursor.
    VFilter {
        cursor_id: CursorID,
        pc_if_empty: BranchOffset,
        arg_count: usize,
        args_reg: usize,
    },

    /// Read a column from the current row of the virtual table cursor.
    VColumn {
        cursor_id: CursorID,
        column: usize,
        dest: usize,
    },

    /// `VUpdate`: Virtual Table Insert/Update/Delete Instruction
    VUpdate {
        cursor_id: usize,     // P1: Virtual table cursor number
        arg_count: usize,     // P2: Number of arguments in argv[]
        start_reg: usize,     // P3: Start register for argv[]
        vtab_ptr: usize,      // P4: vtab pointer
        conflict_action: u16, // P5: Conflict resolution flags
    },

    /// Advance the virtual table cursor to the next row.
    /// TODO: async
    VNext {
        cursor_id: CursorID,
        pc_if_next: BranchOffset,
    },

    /// Open a cursor for a pseudo-table that contains a single row.
    OpenPseudo {
        cursor_id: CursorID,
        content_reg: usize,
        num_fields: usize,
    },

    /// Rewind the cursor to the beginning of the B-Tree.
    RewindAsync {
        cursor_id: CursorID,
    },

    /// Await for the completion of cursor rewind.
    RewindAwait {
        cursor_id: CursorID,
        pc_if_empty: BranchOffset,
    },

    LastAsync {
        cursor_id: CursorID,
    },

    LastAwait {
        cursor_id: CursorID,
        pc_if_empty: BranchOffset,
    },

    /// Read a column from the current row of the cursor.
    Column {
        cursor_id: CursorID,
        column: usize,
        dest: usize,
    },

    /// Make a record and write it to destination register.
    MakeRecord {
        start_reg: usize, // P1
        count: usize,     // P2
        dest_reg: usize,  // P3
    },

    /// Emit a row of results.
    ResultRow {
        start_reg: usize, // P1
        count: usize,     // P2
    },

    /// Advance the cursor to the next row.
    NextAsync {
        cursor_id: CursorID,
    },

    /// Await for the completion of cursor advance.
    NextAwait {
        cursor_id: CursorID,
        pc_if_next: BranchOffset,
    },

    PrevAsync {
        cursor_id: CursorID,
    },

    PrevAwait {
        cursor_id: CursorID,
        pc_if_next: BranchOffset,
    },

    /// Halt the program.
    Halt {
        err_code: usize,
        description: String,
    },

    /// Start a transaction.
    Transaction {
        write: bool,
    },

    /// Set database auto-commit mode and potentially rollback.
    AutoCommit {
        auto_commit: bool,
        rollback: bool,
    },

    /// Branch to the given PC.
    Goto {
        target_pc: BranchOffset,
    },

    /// Stores the current program counter into register 'return_reg' then jumps to address target_pc.
    Gosub {
        target_pc: BranchOffset,
        return_reg: usize,
    },

    /// Returns to the program counter stored in register 'return_reg'.
    Return {
        return_reg: usize,
    },

    /// Write an integer value into a register.
    Integer {
        value: i64,
        dest: usize,
    },

    /// Write a float value into a register
    Real {
        value: f64,
        dest: usize,
    },

    /// If register holds an integer, transform it to a float
    RealAffinity {
        register: usize,
    },

    /// Write a string value into a register.
    String8 {
        value: String,
        dest: usize,
    },

    /// Write a blob value into a register.
    Blob {
        value: Vec<u8>,
        dest: usize,
    },

    /// Read the rowid of the current row.
    RowId {
        cursor_id: CursorID,
        dest: usize,
    },

    /// Seek to a rowid in the cursor. If not found, jump to the given PC. Otherwise, continue to the next instruction.
    SeekRowid {
        cursor_id: CursorID,
        src_reg: usize,
        target_pc: BranchOffset,
    },
    SeekEnd {
        cursor_id: CursorID,
    },

    /// P1 is an open index cursor and P3 is a cursor on the corresponding table. This opcode does a deferred seek of the P3 table cursor to the row that corresponds to the current row of P1.
    /// This is a deferred seek. Nothing actually happens until the cursor is used to read a record. That way, if no reads occur, no unnecessary I/O happens.
    DeferredSeek {
        index_cursor_id: CursorID,
        table_cursor_id: CursorID,
    },

    /// If cursor_id refers to an SQL table (B-Tree that uses integer keys), use the value in start_reg as the key.
    /// If cursor_id refers to an SQL index, then start_reg is the first in an array of num_regs registers that are used as an unpacked index key.
    /// Seek to the first index entry that is greater than or equal to the given key. If not found, jump to the given PC. Otherwise, continue to the next instruction.
    SeekGE {
        is_index: bool,
        cursor_id: CursorID,
        start_reg: usize,
        num_regs: usize,
        target_pc: BranchOffset,
    },

    /// If cursor_id refers to an SQL table (B-Tree that uses integer keys), use the value in start_reg as the key.
    /// If cursor_id refers to an SQL index, then start_reg is the first in an array of num_regs registers that are used as an unpacked index key.
    /// Seek to the first index entry that is greater than the given key. If not found, jump to the given PC. Otherwise, continue to the next instruction.
    SeekGT {
        is_index: bool,
        cursor_id: CursorID,
        start_reg: usize,
        num_regs: usize,
        target_pc: BranchOffset,
    },

    /// cursor_id is a cursor pointing to a B-Tree index that uses integer keys, this op writes the value obtained from MakeRecord into the index.
    /// P3 + P4 are for the original column values that make up that key in unpacked (pre-serialized) form.
    /// If P5 has the OPFLAG_APPEND bit set, that is a hint to the b-tree layer that this insert is likely to be an append.
    /// OPFLAG_NCHANGE bit set, then the change counter is incremented by this instruction. If the OPFLAG_NCHANGE bit is clear, then the change counter is unchanged
    IdxInsertAsync {
        cursor_id: CursorID,
        record_reg: usize, // P2 the register containing the record to insert
        unpacked_start: Option<usize>, // P3 the index of the first register for the unpacked key
        unpacked_count: Option<u16>, // P4 # of unpacked values in the key in P2
        flags: IdxInsertFlags, // TODO: optimization
    },
    IdxInsertAwait {
        cursor_id: CursorID,
    },

    /// The P4 register values beginning with P3 form an unpacked index key that omits the PRIMARY KEY. Compare this key value against the index that P1 is currently pointing to, ignoring the PRIMARY KEY or ROWID fields at the end.
    /// If the P1 index entry is greater or equal than the key value then jump to P2. Otherwise fall through to the next instruction.
    IdxGE {
        cursor_id: CursorID,
        start_reg: usize,
        num_regs: usize,
        target_pc: BranchOffset,
    },

    /// The P4 register values beginning with P3 form an unpacked index key that omits the PRIMARY KEY. Compare this key value against the index that P1 is currently pointing to, ignoring the PRIMARY KEY or ROWID fields at the end.
    /// If the P1 index entry is greater than the key value then jump to P2. Otherwise fall through to the next instruction.
    IdxGT {
        cursor_id: CursorID,
        start_reg: usize,
        num_regs: usize,
        target_pc: BranchOffset,
    },

    /// The P4 register values beginning with P3 form an unpacked index key that omits the PRIMARY KEY. Compare this key value against the index that P1 is currently pointing to, ignoring the PRIMARY KEY or ROWID fields at the end.
    /// If the P1 index entry is lesser or equal than the key value then jump to P2. Otherwise fall through to the next instruction.
    IdxLE {
        cursor_id: CursorID,
        start_reg: usize,
        num_regs: usize,
        target_pc: BranchOffset,
    },

    /// The P4 register values beginning with P3 form an unpacked index key that omits the PRIMARY KEY. Compare this key value against the index that P1 is currently pointing to, ignoring the PRIMARY KEY or ROWID fields at the end.
    /// If the P1 index entry is lesser than the key value then jump to P2. Otherwise fall through to the next instruction.
    IdxLT {
        cursor_id: CursorID,
        start_reg: usize,
        num_regs: usize,
        target_pc: BranchOffset,
    },

    /// Decrement the given register and jump to the given PC if the result is zero.
    DecrJumpZero {
        reg: usize,
        target_pc: BranchOffset,
    },

    AggStep {
        acc_reg: usize,
        col: usize,
        delimiter: usize,
        func: AggFunc,
    },

    AggFinal {
        register: usize,
        func: AggFunc,
    },

    /// Open a sorter.
    SorterOpen {
        cursor_id: CursorID, // P1
        columns: usize,      // P2
        order: Record,       // P4. 0 if ASC and 1 if DESC
    },

    /// Insert a row into the sorter.
    SorterInsert {
        cursor_id: CursorID,
        record_reg: usize,
    },

    /// Sort the rows in the sorter.
    SorterSort {
        cursor_id: CursorID,
        pc_if_empty: BranchOffset,
    },

    /// Retrieve the next row from the sorter.
    SorterData {
        cursor_id: CursorID,  // P1
        dest_reg: usize,      // P2
        pseudo_cursor: usize, // P3
    },

    /// Advance to the next row in the sorter.
    SorterNext {
        cursor_id: CursorID,
        pc_if_next: BranchOffset,
    },

    /// Function
    Function {
        constant_mask: i32, // P1
        start_reg: usize,   // P2, start of argument registers
        dest: usize,        // P3
        func: FuncCtx,      // P4
    },

    InitCoroutine {
        yield_reg: usize,
        jump_on_definition: BranchOffset,
        start_offset: BranchOffset,
    },

    EndCoroutine {
        yield_reg: usize,
    },

    Yield {
        yield_reg: usize,
        end_offset: BranchOffset,
    },

    InsertAsync {
        cursor: CursorID,
        key_reg: usize,    // Must be int.
        record_reg: usize, // Blob of record data.
        flag: usize,       // Flags used by insert, for now not used.
    },

    InsertAwait {
        cursor_id: usize,
    },

    DeleteAsync {
        cursor_id: CursorID,
    },

    DeleteAwait {
        cursor_id: CursorID,
    },

    NewRowid {
        cursor: CursorID,        // P1
        rowid_reg: usize,        // P2  Destination register to store the new rowid
        prev_largest_reg: usize, // P3 Previous largest rowid in the table (Not used for now)
    },

    MustBeInt {
        reg: usize,
    },

    SoftNull {
        reg: usize,
    },

    NotExists {
        cursor: CursorID,
        rowid_reg: usize,
        target_pc: BranchOffset,
    },

    OffsetLimit {
        limit_reg: usize,
        combined_reg: usize,
        offset_reg: usize,
    },

    OpenWriteAsync {
        cursor_id: CursorID,
        root_page: RegisterOrLiteral<PageIdx>,
    },

    OpenWriteAwait {},

    Copy {
        src_reg: usize,
        dst_reg: usize,
        amount: usize, // 0 amount means we include src_reg, dst_reg..=dst_reg+amount = src_reg..=src_reg+amount
    },

    /// Allocate a new b-tree.
    CreateBtree {
        /// Allocate b-tree in main database if zero or in temp database if non-zero (P1).
        db: usize,
        /// The root page of the new b-tree (P2).
        root: usize,
        /// Flags (P3).
        flags: usize,
    },

    /// Deletes an entire database table or index whose root page in the database file is given by P1.
    Destroy {
        /// The root page of the table/index to destroy
        root: usize,
        /// Register to store the former value of any moved root page (for AUTOVACUUM)
        former_root_reg: usize,
        /// Whether this is a temporary table (1) or main database table (0)
        is_temp: usize,
    },

    ///  Drop a table
    DropTable {
        ///  The database within which this b-tree needs to be dropped (P1).
        db: usize,
        ///  unused register p2
        _p2: usize,
        ///  unused register p3
        _p3: usize,
        //  The name of the table being dropped
        table_name: String,
    },

    /// Close a cursor.
    Close {
        cursor_id: CursorID,
    },

    /// Check if the register is null.
    IsNull {
        /// Source register (P1).
        reg: usize,

        /// Jump to this PC if the register is null (P2).
        target_pc: BranchOffset,
    },
    ParseSchema {
        db: usize,
        where_clause: String,
    },

    /// Place the result of lhs >> rhs in dest register.
    ShiftRight {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },

    /// Place the result of lhs << rhs in dest register.
    ShiftLeft {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },

    /// Get parameter variable.
    Variable {
        index: NonZero<usize>,
        dest: usize,
    },
    /// If either register is null put null else put 0
    ZeroOrNull {
        /// Source register (P1).
        rg1: usize,
        rg2: usize,
        dest: usize,
    },
    /// Interpret the value in reg as boolean and store its compliment in destination
    Not {
        reg: usize,
        dest: usize,
    },
    /// Concatenates the `rhs` and `lhs` values and stores the result in the third register.
    Concat {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// Take the logical AND of the values in registers P1 and P2 and write the result into register P3.
    And {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// Take the logical OR of the values in register P1 and P2 and store the answer in register P3.
    Or {
        lhs: usize,
        rhs: usize,
        dest: usize,
    },
    /// Do nothing. Continue downward to the next opcode.
    Noop,
    /// Write the current number of pages in database P1 to memory cell P2.
    PageCount {
        db: usize,
        dest: usize,
    },
    /// Read cookie number P3 from database P1 and write it into register P2
    ReadCookie {
        db: usize,
        dest: usize,
        cookie: Cookie,
    },
}

// TODO: Add remaining cookies.
#[derive(Description, Debug, Clone, Copy)]
pub enum Cookie {
    /// The schema cookie.
    SchemaVersion = 1,
    /// The schema format number. Supported schema formats are 1, 2, 3, and 4.
    DatabaseFormat = 2,
    /// Default page cache size.
    DefaultPageCacheSize = 3,
    /// The page number of the largest root b-tree page when in auto-vacuum or incremental-vacuum modes, or zero otherwise.
    LargestRootPageNumber = 4,
    /// The database text encoding. A value of 1 means UTF-8. A value of 2 means UTF-16le. A value of 3 means UTF-16be.
    DatabaseTextEncoding = 5,
    /// The "user version" as read and set by the user_version pragma.
    UserVersion = 6,
}

pub fn exec_add(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    let result = match (lhs, rhs) {
        (OwnedValue::Integer(lhs), OwnedValue::Integer(rhs)) => {
            let result = lhs.overflowing_add(*rhs);
            if result.1 {
                OwnedValue::Float(*lhs as f64 + *rhs as f64)
            } else {
                OwnedValue::Integer(result.0)
            }
        }
        (OwnedValue::Float(lhs), OwnedValue::Float(rhs)) => OwnedValue::Float(lhs + rhs),
        (OwnedValue::Float(f), OwnedValue::Integer(i))
        | (OwnedValue::Integer(i), OwnedValue::Float(f)) => OwnedValue::Float(*f + *i as f64),
        (OwnedValue::Null, _) | (_, OwnedValue::Null) => OwnedValue::Null,
        (OwnedValue::Text(lhs), OwnedValue::Text(rhs)) => exec_add(
            &cast_text_to_numeric(lhs.as_str()),
            &cast_text_to_numeric(rhs.as_str()),
        ),
        (OwnedValue::Text(text), other) | (other, OwnedValue::Text(text)) => {
            exec_add(&cast_text_to_numeric(text.as_str()), other)
        }
        _ => todo!(),
    };
    match result {
        OwnedValue::Float(f) if f.is_nan() => OwnedValue::Null,
        _ => result,
    }
}

pub fn exec_subtract(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    let result = match (lhs, rhs) {
        (OwnedValue::Integer(lhs), OwnedValue::Integer(rhs)) => {
            let result = lhs.overflowing_sub(*rhs);
            if result.1 {
                OwnedValue::Float(*lhs as f64 - *rhs as f64)
            } else {
                OwnedValue::Integer(result.0)
            }
        }
        (OwnedValue::Float(lhs), OwnedValue::Float(rhs)) => OwnedValue::Float(lhs - rhs),
        (OwnedValue::Float(lhs), OwnedValue::Integer(rhs)) => OwnedValue::Float(lhs - *rhs as f64),
        (OwnedValue::Integer(lhs), OwnedValue::Float(rhs)) => OwnedValue::Float(*lhs as f64 - rhs),
        (OwnedValue::Null, _) | (_, OwnedValue::Null) => OwnedValue::Null,
        (OwnedValue::Text(lhs), OwnedValue::Text(rhs)) => exec_subtract(
            &cast_text_to_numeric(lhs.as_str()),
            &cast_text_to_numeric(rhs.as_str()),
        ),
        (OwnedValue::Text(text), other) => {
            exec_subtract(&cast_text_to_numeric(text.as_str()), other)
        }
        (other, OwnedValue::Text(text)) => {
            exec_subtract(other, &cast_text_to_numeric(text.as_str()))
        }
        _ => todo!(),
    };
    match result {
        OwnedValue::Float(f) if f.is_nan() => OwnedValue::Null,
        _ => result,
    }
}

pub fn exec_multiply(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    let result = match (lhs, rhs) {
        (OwnedValue::Integer(lhs), OwnedValue::Integer(rhs)) => {
            let result = lhs.overflowing_mul(*rhs);
            if result.1 {
                OwnedValue::Float(*lhs as f64 * *rhs as f64)
            } else {
                OwnedValue::Integer(result.0)
            }
        }
        (OwnedValue::Float(lhs), OwnedValue::Float(rhs)) => OwnedValue::Float(lhs * rhs),
        (OwnedValue::Integer(i), OwnedValue::Float(f))
        | (OwnedValue::Float(f), OwnedValue::Integer(i)) => OwnedValue::Float(*i as f64 * { *f }),
        (OwnedValue::Null, _) | (_, OwnedValue::Null) => OwnedValue::Null,
        (OwnedValue::Text(lhs), OwnedValue::Text(rhs)) => exec_multiply(
            &cast_text_to_numeric(lhs.as_str()),
            &cast_text_to_numeric(rhs.as_str()),
        ),
        (OwnedValue::Text(text), other) | (other, OwnedValue::Text(text)) => {
            exec_multiply(&cast_text_to_numeric(text.as_str()), other)
        }

        _ => todo!(),
    };
    match result {
        OwnedValue::Float(f) if f.is_nan() => OwnedValue::Null,
        _ => result,
    }
}

pub fn exec_divide(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    let result = match (lhs, rhs) {
        (_, OwnedValue::Integer(0)) | (_, OwnedValue::Float(0.0)) => OwnedValue::Null,
        (OwnedValue::Integer(lhs), OwnedValue::Integer(rhs)) => {
            let result = lhs.overflowing_div(*rhs);
            if result.1 {
                OwnedValue::Float(*lhs as f64 / *rhs as f64)
            } else {
                OwnedValue::Integer(result.0)
            }
        }
        (OwnedValue::Float(lhs), OwnedValue::Float(rhs)) => OwnedValue::Float(lhs / rhs),
        (OwnedValue::Float(lhs), OwnedValue::Integer(rhs)) => OwnedValue::Float(lhs / *rhs as f64),
        (OwnedValue::Integer(lhs), OwnedValue::Float(rhs)) => OwnedValue::Float(*lhs as f64 / rhs),
        (OwnedValue::Null, _) | (_, OwnedValue::Null) => OwnedValue::Null,
        (OwnedValue::Text(lhs), OwnedValue::Text(rhs)) => exec_divide(
            &cast_text_to_numeric(lhs.as_str()),
            &cast_text_to_numeric(rhs.as_str()),
        ),
        (OwnedValue::Text(text), other) => exec_divide(&cast_text_to_numeric(text.as_str()), other),
        (other, OwnedValue::Text(text)) => exec_divide(other, &cast_text_to_numeric(text.as_str())),
        _ => todo!(),
    };
    match result {
        OwnedValue::Float(f) if f.is_nan() => OwnedValue::Null,
        _ => result,
    }
}

pub fn exec_bit_and(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    match (lhs, rhs) {
        (OwnedValue::Null, _) | (_, OwnedValue::Null) => OwnedValue::Null,
        (_, OwnedValue::Integer(0))
        | (OwnedValue::Integer(0), _)
        | (_, OwnedValue::Float(0.0))
        | (OwnedValue::Float(0.0), _) => OwnedValue::Integer(0),
        (OwnedValue::Integer(lh), OwnedValue::Integer(rh)) => OwnedValue::Integer(lh & rh),
        (OwnedValue::Float(lh), OwnedValue::Float(rh)) => {
            OwnedValue::Integer(*lh as i64 & *rh as i64)
        }
        (OwnedValue::Float(lh), OwnedValue::Integer(rh)) => OwnedValue::Integer(*lh as i64 & rh),
        (OwnedValue::Integer(lh), OwnedValue::Float(rh)) => OwnedValue::Integer(lh & *rh as i64),
        (OwnedValue::Text(lhs), OwnedValue::Text(rhs)) => exec_bit_and(
            &cast_text_to_numeric(lhs.as_str()),
            &cast_text_to_numeric(rhs.as_str()),
        ),
        (OwnedValue::Text(text), other) | (other, OwnedValue::Text(text)) => {
            exec_bit_and(&cast_text_to_numeric(text.as_str()), other)
        }
        _ => todo!(),
    }
}

pub fn exec_bit_or(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    match (lhs, rhs) {
        (OwnedValue::Null, _) | (_, OwnedValue::Null) => OwnedValue::Null,
        (OwnedValue::Integer(lh), OwnedValue::Integer(rh)) => OwnedValue::Integer(lh | rh),
        (OwnedValue::Float(lh), OwnedValue::Integer(rh)) => OwnedValue::Integer(*lh as i64 | rh),
        (OwnedValue::Integer(lh), OwnedValue::Float(rh)) => OwnedValue::Integer(lh | *rh as i64),
        (OwnedValue::Float(lh), OwnedValue::Float(rh)) => {
            OwnedValue::Integer(*lh as i64 | *rh as i64)
        }
        (OwnedValue::Text(lhs), OwnedValue::Text(rhs)) => exec_bit_or(
            &cast_text_to_numeric(lhs.as_str()),
            &cast_text_to_numeric(rhs.as_str()),
        ),
        (OwnedValue::Text(text), other) | (other, OwnedValue::Text(text)) => {
            exec_bit_or(&cast_text_to_numeric(text.as_str()), other)
        }
        _ => todo!(),
    }
}

pub fn exec_remainder(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    match (lhs, rhs) {
        (OwnedValue::Null, _)
        | (_, OwnedValue::Null)
        | (_, OwnedValue::Integer(0))
        | (_, OwnedValue::Float(0.0)) => OwnedValue::Null,
        (OwnedValue::Integer(lhs), OwnedValue::Integer(rhs)) => {
            if rhs == &0 {
                OwnedValue::Null
            } else {
                OwnedValue::Integer(lhs % rhs)
            }
        }
        (OwnedValue::Float(lhs), OwnedValue::Float(rhs)) => {
            let rhs_int = *rhs as i64;
            if rhs_int == 0 {
                OwnedValue::Null
            } else {
                OwnedValue::Float(((*lhs as i64) % rhs_int) as f64)
            }
        }
        (OwnedValue::Float(lhs), OwnedValue::Integer(rhs)) => {
            if rhs == &0 {
                OwnedValue::Null
            } else {
                OwnedValue::Float(((*lhs as i64) % rhs) as f64)
            }
        }
        (OwnedValue::Integer(lhs), OwnedValue::Float(rhs)) => {
            let rhs_int = *rhs as i64;
            if rhs_int == 0 {
                OwnedValue::Null
            } else {
                OwnedValue::Float((lhs % rhs_int) as f64)
            }
        }
        (OwnedValue::Text(lhs), OwnedValue::Text(rhs)) => exec_remainder(
            &cast_text_to_numeric(lhs.as_str()),
            &cast_text_to_numeric(rhs.as_str()),
        ),
        (OwnedValue::Text(text), other) | (other, OwnedValue::Text(text)) => {
            exec_remainder(&cast_text_to_numeric(text.as_str()), other)
        }
        other => todo!("remainder not implemented for: {:?} {:?}", lhs, other),
    }
}

pub fn exec_bit_not(reg: &OwnedValue) -> OwnedValue {
    match reg {
        OwnedValue::Null => OwnedValue::Null,
        OwnedValue::Integer(i) => OwnedValue::Integer(!i),
        OwnedValue::Float(f) => OwnedValue::Integer(!(*f as i64)),
        OwnedValue::Text(text) => exec_bit_not(&cast_text_to_numeric(text.as_str())),
        _ => todo!(),
    }
}

pub fn exec_shift_left(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    match (lhs, rhs) {
        (OwnedValue::Null, _) | (_, OwnedValue::Null) => OwnedValue::Null,
        (OwnedValue::Integer(lh), OwnedValue::Integer(rh)) => {
            OwnedValue::Integer(compute_shl(*lh, *rh))
        }
        (OwnedValue::Float(lh), OwnedValue::Integer(rh)) => {
            OwnedValue::Integer(compute_shl(*lh as i64, *rh))
        }
        (OwnedValue::Integer(lh), OwnedValue::Float(rh)) => {
            OwnedValue::Integer(compute_shl(*lh, *rh as i64))
        }
        (OwnedValue::Float(lh), OwnedValue::Float(rh)) => {
            OwnedValue::Integer(compute_shl(*lh as i64, *rh as i64))
        }
        (OwnedValue::Text(lhs), OwnedValue::Text(rhs)) => exec_shift_left(
            &cast_text_to_numeric(lhs.as_str()),
            &cast_text_to_numeric(rhs.as_str()),
        ),
        (OwnedValue::Text(text), other) => {
            exec_shift_left(&cast_text_to_numeric(text.as_str()), other)
        }
        (other, OwnedValue::Text(text)) => {
            exec_shift_left(other, &cast_text_to_numeric(text.as_str()))
        }
        _ => todo!(),
    }
}

fn compute_shl(lhs: i64, rhs: i64) -> i64 {
    if rhs == 0 {
        lhs
    } else if rhs > 0 {
        // for positive shifts, if it's too large return 0
        if rhs >= 64 {
            0
        } else {
            lhs << rhs
        }
    } else {
        // for negative shifts, check if it's i64::MIN to avoid overflow on negation
        if rhs == i64::MIN || rhs <= -64 {
            if lhs < 0 {
                -1
            } else {
                0
            }
        } else {
            lhs >> (-rhs)
        }
    }
}

pub fn exec_shift_right(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    match (lhs, rhs) {
        (OwnedValue::Null, _) | (_, OwnedValue::Null) => OwnedValue::Null,
        (OwnedValue::Integer(lh), OwnedValue::Integer(rh)) => {
            OwnedValue::Integer(compute_shr(*lh, *rh))
        }
        (OwnedValue::Float(lh), OwnedValue::Integer(rh)) => {
            OwnedValue::Integer(compute_shr(*lh as i64, *rh))
        }
        (OwnedValue::Integer(lh), OwnedValue::Float(rh)) => {
            OwnedValue::Integer(compute_shr(*lh, *rh as i64))
        }
        (OwnedValue::Float(lh), OwnedValue::Float(rh)) => {
            OwnedValue::Integer(compute_shr(*lh as i64, *rh as i64))
        }
        (OwnedValue::Text(lhs), OwnedValue::Text(rhs)) => exec_shift_right(
            &cast_text_to_numeric(lhs.as_str()),
            &cast_text_to_numeric(rhs.as_str()),
        ),
        (OwnedValue::Text(text), other) => {
            exec_shift_right(&cast_text_to_numeric(text.as_str()), other)
        }
        (other, OwnedValue::Text(text)) => {
            exec_shift_right(other, &cast_text_to_numeric(text.as_str()))
        }
        _ => todo!(),
    }
}

// compute binary shift to the right if rhs >= 0 and binary shift to the left - if rhs < 0
// note, that binary shift to the right is sign-extended
fn compute_shr(lhs: i64, rhs: i64) -> i64 {
    if rhs == 0 {
        lhs
    } else if rhs > 0 {
        // for positive right shifts
        if rhs >= 64 {
            if lhs < 0 {
                -1
            } else {
                0
            }
        } else {
            lhs >> rhs
        }
    } else {
        // for negative right shifts, check if it's i64::MIN to avoid overflow
        if rhs == i64::MIN || -rhs >= 64 {
            0
        } else {
            lhs << (-rhs)
        }
    }
}

pub fn exec_boolean_not(reg: &OwnedValue) -> OwnedValue {
    match reg {
        OwnedValue::Null => OwnedValue::Null,
        OwnedValue::Integer(i) => OwnedValue::Integer((*i == 0) as i64),
        OwnedValue::Float(f) => OwnedValue::Integer((*f == 0.0) as i64),
        OwnedValue::Text(text) => exec_boolean_not(&cast_text_to_numeric(text.as_str())),
        _ => todo!(),
    }
}
pub fn exec_concat(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    match (lhs, rhs) {
        (OwnedValue::Text(lhs_text), OwnedValue::Text(rhs_text)) => {
            OwnedValue::build_text(&(lhs_text.as_str().to_string() + rhs_text.as_str()))
        }
        (OwnedValue::Text(lhs_text), OwnedValue::Integer(rhs_int)) => {
            OwnedValue::build_text(&(lhs_text.as_str().to_string() + &rhs_int.to_string()))
        }
        (OwnedValue::Text(lhs_text), OwnedValue::Float(rhs_float)) => {
            OwnedValue::build_text(&(lhs_text.as_str().to_string() + &rhs_float.to_string()))
        }
        (OwnedValue::Integer(lhs_int), OwnedValue::Text(rhs_text)) => {
            OwnedValue::build_text(&(lhs_int.to_string() + rhs_text.as_str()))
        }
        (OwnedValue::Integer(lhs_int), OwnedValue::Integer(rhs_int)) => {
            OwnedValue::build_text(&(lhs_int.to_string() + &rhs_int.to_string()))
        }
        (OwnedValue::Integer(lhs_int), OwnedValue::Float(rhs_float)) => {
            OwnedValue::build_text(&(lhs_int.to_string() + &rhs_float.to_string()))
        }
        (OwnedValue::Float(lhs_float), OwnedValue::Text(rhs_text)) => {
            OwnedValue::build_text(&(lhs_float.to_string() + rhs_text.as_str()))
        }
        (OwnedValue::Float(lhs_float), OwnedValue::Integer(rhs_int)) => {
            OwnedValue::build_text(&(lhs_float.to_string() + &rhs_int.to_string()))
        }
        (OwnedValue::Float(lhs_float), OwnedValue::Float(rhs_float)) => {
            OwnedValue::build_text(&(lhs_float.to_string() + &rhs_float.to_string()))
        }
        (OwnedValue::Null, _) | (_, OwnedValue::Null) => OwnedValue::Null,
        (OwnedValue::Blob(_), _) | (_, OwnedValue::Blob(_)) => {
            todo!("TODO: Handle Blob conversion to String")
        }
    }
}

pub fn exec_and(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    match (lhs, rhs) {
        (_, OwnedValue::Integer(0))
        | (OwnedValue::Integer(0), _)
        | (_, OwnedValue::Float(0.0))
        | (OwnedValue::Float(0.0), _) => OwnedValue::Integer(0),
        (OwnedValue::Null, _) | (_, OwnedValue::Null) => OwnedValue::Null,
        (OwnedValue::Text(lhs), OwnedValue::Text(rhs)) => exec_and(
            &cast_text_to_numeric(lhs.as_str()),
            &cast_text_to_numeric(rhs.as_str()),
        ),
        (OwnedValue::Text(text), other) | (other, OwnedValue::Text(text)) => {
            exec_and(&cast_text_to_numeric(text.as_str()), other)
        }
        _ => OwnedValue::Integer(1),
    }
}

pub fn exec_or(lhs: &OwnedValue, rhs: &OwnedValue) -> OwnedValue {
    match (lhs, rhs) {
        (OwnedValue::Null, OwnedValue::Null)
        | (OwnedValue::Null, OwnedValue::Float(0.0))
        | (OwnedValue::Float(0.0), OwnedValue::Null)
        | (OwnedValue::Null, OwnedValue::Integer(0))
        | (OwnedValue::Integer(0), OwnedValue::Null) => OwnedValue::Null,
        (OwnedValue::Float(0.0), OwnedValue::Integer(0))
        | (OwnedValue::Integer(0), OwnedValue::Float(0.0))
        | (OwnedValue::Float(0.0), OwnedValue::Float(0.0))
        | (OwnedValue::Integer(0), OwnedValue::Integer(0)) => OwnedValue::Integer(0),
        (OwnedValue::Text(lhs), OwnedValue::Text(rhs)) => exec_or(
            &cast_text_to_numeric(lhs.as_str()),
            &cast_text_to_numeric(rhs.as_str()),
        ),
        (OwnedValue::Text(text), other) | (other, OwnedValue::Text(text)) => {
            exec_or(&cast_text_to_numeric(text.as_str()), other)
        }
        _ => OwnedValue::Integer(1),
    }
}

impl Insn {
    pub fn to_function(&self) -> InsnFunction {
        match self {
            Insn::Init { .. } => execute::op_init,

            Insn::Null { .. } => execute::op_null,

            Insn::NullRow { .. } => execute::op_null_row,

            Insn::Add { .. } => execute::op_add,

            Insn::Subtract { .. } => execute::op_subtract,

            Insn::Multiply { .. } => execute::op_multiply,

            Insn::Divide { .. } => execute::op_divide,

            Insn::Compare { .. } => execute::op_compare,
            Insn::BitAnd { .. } => execute::op_bit_and,

            Insn::BitOr { .. } => execute::op_bit_or,

            Insn::BitNot { .. } => execute::op_bit_not,

            Insn::Checkpoint { .. } => execute::op_checkpoint,
            Insn::Remainder { .. } => execute::op_remainder,

            Insn::Jump { .. } => execute::op_jump,
            Insn::Move { .. } => execute::op_move,
            Insn::IfPos { .. } => execute::op_if_pos,
            Insn::NotNull { .. } => execute::op_not_null,

            Insn::Eq { .. } => execute::op_eq,
            Insn::Ne { .. } => execute::op_ne,
            Insn::Lt { .. } => execute::op_lt,
            Insn::Le { .. } => execute::op_le,
            Insn::Gt { .. } => execute::op_gt,
            Insn::Ge { .. } => execute::op_ge,
            Insn::If { .. } => execute::op_if,
            Insn::IfNot { .. } => execute::op_if_not,
            Insn::OpenReadAsync { .. } => execute::op_open_read_async,
            Insn::OpenReadAwait => execute::op_open_read_await,

            Insn::VOpenAsync { .. } => execute::op_vopen_async,

            Insn::VOpenAwait => execute::op_vopen_await,

            Insn::VCreate { .. } => execute::op_vcreate,
            Insn::VFilter { .. } => execute::op_vfilter,
            Insn::VColumn { .. } => execute::op_vcolumn,
            Insn::VUpdate { .. } => execute::op_vupdate,
            Insn::VNext { .. } => execute::op_vnext,
            Insn::OpenPseudo { .. } => execute::op_open_pseudo,
            Insn::RewindAsync { .. } => execute::op_rewind_async,

            Insn::RewindAwait { .. } => execute::op_rewind_await,
            Insn::LastAsync { .. } => execute::op_last_async,

            Insn::LastAwait { .. } => execute::op_last_await,
            Insn::Column { .. } => execute::op_column,
            Insn::MakeRecord { .. } => execute::op_make_record,
            Insn::ResultRow { .. } => execute::op_result_row,

            Insn::NextAsync { .. } => execute::op_next_async,

            Insn::NextAwait { .. } => execute::op_next_await,
            Insn::PrevAsync { .. } => execute::op_prev_async,

            Insn::PrevAwait { .. } => execute::op_prev_await,
            Insn::Halt { .. } => execute::op_halt,
            Insn::Transaction { .. } => execute::op_transaction,

            Insn::AutoCommit { .. } => execute::op_auto_commit,
            Insn::Goto { .. } => execute::op_goto,

            Insn::Gosub { .. } => execute::op_gosub,
            Insn::Return { .. } => execute::op_return,

            Insn::Integer { .. } => execute::op_integer,

            Insn::Real { .. } => execute::op_real,

            Insn::RealAffinity { .. } => execute::op_real_affinity,

            Insn::String8 { .. } => execute::op_string8,

            Insn::Blob { .. } => execute::op_blob,

            Insn::RowId { .. } => execute::op_row_id,

            Insn::SeekRowid { .. } => execute::op_seek_rowid,
            Insn::DeferredSeek { .. } => execute::op_deferred_seek,
            Insn::SeekGE { .. } => execute::op_seek_ge,
            Insn::SeekGT { .. } => execute::op_seek_gt,
            Insn::SeekEnd { .. } => execute::op_seek_end,
            Insn::IdxGE { .. } => execute::op_idx_ge,
            Insn::IdxGT { .. } => execute::op_idx_gt,
            Insn::IdxLE { .. } => execute::op_idx_le,
            Insn::IdxLT { .. } => execute::op_idx_lt,
            Insn::DecrJumpZero { .. } => execute::op_decr_jump_zero,

            Insn::AggStep { .. } => execute::op_agg_step,
            Insn::AggFinal { .. } => execute::op_agg_final,

            Insn::SorterOpen { .. } => execute::op_sorter_open,
            Insn::SorterInsert { .. } => execute::op_sorter_insert,
            Insn::SorterSort { .. } => execute::op_sorter_sort,
            Insn::SorterData { .. } => execute::op_sorter_data,
            Insn::SorterNext { .. } => execute::op_sorter_next,
            Insn::Function { .. } => execute::op_function,
            Insn::InitCoroutine { .. } => execute::op_init_coroutine,
            Insn::EndCoroutine { .. } => execute::op_end_coroutine,

            Insn::Yield { .. } => execute::op_yield,
            Insn::InsertAsync { .. } => execute::op_insert_async,
            Insn::InsertAwait { .. } => execute::op_insert_await,
            Insn::IdxInsertAsync { .. } => execute::op_idx_insert_async,
            Insn::IdxInsertAwait { .. } => execute::op_idx_insert_await,
            Insn::DeleteAsync { .. } => execute::op_delete_async,

            Insn::DeleteAwait { .. } => execute::op_delete_await,

            Insn::NewRowid { .. } => execute::op_new_rowid,
            Insn::MustBeInt { .. } => execute::op_must_be_int,

            Insn::SoftNull { .. } => execute::op_soft_null,

            Insn::NotExists { .. } => execute::op_not_exists,
            Insn::OffsetLimit { .. } => execute::op_offset_limit,
            Insn::OpenWriteAsync { .. } => execute::op_open_write_async,
            Insn::OpenWriteAwait { .. } => execute::op_open_write_await,

            Insn::Copy { .. } => execute::op_copy,
            Insn::CreateBtree { .. } => execute::op_create_btree,

            Insn::Destroy { .. } => execute::op_destroy,
            Insn::DropTable { .. } => execute::op_drop_table,
            Insn::Close { .. } => execute::op_close,

            Insn::IsNull { .. } => execute::op_is_null,

            Insn::ParseSchema { .. } => execute::op_parse_schema,

            Insn::ShiftRight { .. } => execute::op_shift_right,

            Insn::ShiftLeft { .. } => execute::op_shift_left,

            Insn::Variable { .. } => execute::op_variable,

            Insn::ZeroOrNull { .. } => execute::op_zero_or_null,

            Insn::Not { .. } => execute::op_not,

            Insn::Concat { .. } => execute::op_concat,

            Insn::And { .. } => execute::op_and,

            Insn::Or { .. } => execute::op_or,

            Insn::Noop => execute::op_noop,
            Insn::PageCount { .. } => execute::op_page_count,

            Insn::ReadCookie { .. } => execute::op_read_cookie,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        types::{OwnedValue, Text},
        vdbe::insn::exec_or,
    };

    use super::exec_add;

    #[test]
    fn test_exec_add() {
        let inputs = vec![
            (OwnedValue::Integer(3), OwnedValue::Integer(1)),
            (OwnedValue::Float(3.0), OwnedValue::Float(1.0)),
            (OwnedValue::Float(3.0), OwnedValue::Integer(1)),
            (OwnedValue::Integer(3), OwnedValue::Float(1.0)),
            (OwnedValue::Null, OwnedValue::Null),
            (OwnedValue::Null, OwnedValue::Integer(1)),
            (OwnedValue::Null, OwnedValue::Float(1.0)),
            (OwnedValue::Null, OwnedValue::Text(Text::from_str("2"))),
            (OwnedValue::Integer(1), OwnedValue::Null),
            (OwnedValue::Float(1.0), OwnedValue::Null),
            (OwnedValue::Text(Text::from_str("1")), OwnedValue::Null),
            (
                OwnedValue::Text(Text::from_str("1")),
                OwnedValue::Text(Text::from_str("3")),
            ),
            (
                OwnedValue::Text(Text::from_str("1.0")),
                OwnedValue::Text(Text::from_str("3.0")),
            ),
            (
                OwnedValue::Text(Text::from_str("1.0")),
                OwnedValue::Float(3.0),
            ),
            (
                OwnedValue::Text(Text::from_str("1.0")),
                OwnedValue::Integer(3),
            ),
            (
                OwnedValue::Float(1.0),
                OwnedValue::Text(Text::from_str("3.0")),
            ),
            (
                OwnedValue::Integer(1),
                OwnedValue::Text(Text::from_str("3")),
            ),
        ];

        let outputs = [
            OwnedValue::Integer(4),
            OwnedValue::Float(4.0),
            OwnedValue::Float(4.0),
            OwnedValue::Float(4.0),
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Integer(4),
            OwnedValue::Float(4.0),
            OwnedValue::Float(4.0),
            OwnedValue::Float(4.0),
            OwnedValue::Float(4.0),
            OwnedValue::Float(4.0),
        ];

        assert_eq!(
            inputs.len(),
            outputs.len(),
            "Inputs and Outputs should have same size"
        );
        for (i, (lhs, rhs)) in inputs.iter().enumerate() {
            assert_eq!(
                exec_add(lhs, rhs),
                outputs[i],
                "Wrong ADD for lhs: {}, rhs: {}",
                lhs,
                rhs
            );
        }
    }

    use super::exec_subtract;

    #[test]
    fn test_exec_subtract() {
        let inputs = vec![
            (OwnedValue::Integer(3), OwnedValue::Integer(1)),
            (OwnedValue::Float(3.0), OwnedValue::Float(1.0)),
            (OwnedValue::Float(3.0), OwnedValue::Integer(1)),
            (OwnedValue::Integer(3), OwnedValue::Float(1.0)),
            (OwnedValue::Null, OwnedValue::Null),
            (OwnedValue::Null, OwnedValue::Integer(1)),
            (OwnedValue::Null, OwnedValue::Float(1.0)),
            (OwnedValue::Null, OwnedValue::Text(Text::from_str("1"))),
            (OwnedValue::Integer(1), OwnedValue::Null),
            (OwnedValue::Float(1.0), OwnedValue::Null),
            (OwnedValue::Text(Text::from_str("4")), OwnedValue::Null),
            (
                OwnedValue::Text(Text::from_str("1")),
                OwnedValue::Text(Text::from_str("3")),
            ),
            (
                OwnedValue::Text(Text::from_str("1.0")),
                OwnedValue::Text(Text::from_str("3.0")),
            ),
            (
                OwnedValue::Text(Text::from_str("1.0")),
                OwnedValue::Float(3.0),
            ),
            (
                OwnedValue::Text(Text::from_str("1.0")),
                OwnedValue::Integer(3),
            ),
            (
                OwnedValue::Float(1.0),
                OwnedValue::Text(Text::from_str("3.0")),
            ),
            (
                OwnedValue::Integer(1),
                OwnedValue::Text(Text::from_str("3")),
            ),
        ];

        let outputs = [
            OwnedValue::Integer(2),
            OwnedValue::Float(2.0),
            OwnedValue::Float(2.0),
            OwnedValue::Float(2.0),
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Integer(-2),
            OwnedValue::Float(-2.0),
            OwnedValue::Float(-2.0),
            OwnedValue::Float(-2.0),
            OwnedValue::Float(-2.0),
            OwnedValue::Float(-2.0),
        ];

        assert_eq!(
            inputs.len(),
            outputs.len(),
            "Inputs and Outputs should have same size"
        );
        for (i, (lhs, rhs)) in inputs.iter().enumerate() {
            assert_eq!(
                exec_subtract(lhs, rhs),
                outputs[i],
                "Wrong subtract for lhs: {}, rhs: {}",
                lhs,
                rhs
            );
        }
    }
    use super::exec_multiply;

    #[test]
    fn test_exec_multiply() {
        let inputs = vec![
            (OwnedValue::Integer(3), OwnedValue::Integer(2)),
            (OwnedValue::Float(3.0), OwnedValue::Float(2.0)),
            (OwnedValue::Float(3.0), OwnedValue::Integer(2)),
            (OwnedValue::Integer(3), OwnedValue::Float(2.0)),
            (OwnedValue::Null, OwnedValue::Null),
            (OwnedValue::Null, OwnedValue::Integer(1)),
            (OwnedValue::Null, OwnedValue::Float(1.0)),
            (OwnedValue::Null, OwnedValue::Text(Text::from_str("1"))),
            (OwnedValue::Integer(1), OwnedValue::Null),
            (OwnedValue::Float(1.0), OwnedValue::Null),
            (OwnedValue::Text(Text::from_str("4")), OwnedValue::Null),
            (
                OwnedValue::Text(Text::from_str("2")),
                OwnedValue::Text(Text::from_str("3")),
            ),
            (
                OwnedValue::Text(Text::from_str("2.0")),
                OwnedValue::Text(Text::from_str("3.0")),
            ),
            (
                OwnedValue::Text(Text::from_str("2.0")),
                OwnedValue::Float(3.0),
            ),
            (
                OwnedValue::Text(Text::from_str("2.0")),
                OwnedValue::Integer(3),
            ),
            (
                OwnedValue::Float(2.0),
                OwnedValue::Text(Text::from_str("3.0")),
            ),
            (
                OwnedValue::Integer(2),
                OwnedValue::Text(Text::from_str("3.0")),
            ),
        ];

        let outputs = [
            OwnedValue::Integer(6),
            OwnedValue::Float(6.0),
            OwnedValue::Float(6.0),
            OwnedValue::Float(6.0),
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Integer(6),
            OwnedValue::Float(6.0),
            OwnedValue::Float(6.0),
            OwnedValue::Float(6.0),
            OwnedValue::Float(6.0),
            OwnedValue::Float(6.0),
        ];

        assert_eq!(
            inputs.len(),
            outputs.len(),
            "Inputs and Outputs should have same size"
        );
        for (i, (lhs, rhs)) in inputs.iter().enumerate() {
            assert_eq!(
                exec_multiply(lhs, rhs),
                outputs[i],
                "Wrong multiply for lhs: {}, rhs: {}",
                lhs,
                rhs
            );
        }
    }
    use super::exec_divide;

    #[test]
    fn test_exec_divide() {
        let inputs = vec![
            (OwnedValue::Integer(1), OwnedValue::Integer(0)),
            (OwnedValue::Float(1.0), OwnedValue::Float(0.0)),
            (OwnedValue::Integer(i64::MIN), OwnedValue::Integer(-1)),
            (OwnedValue::Float(6.0), OwnedValue::Float(2.0)),
            (OwnedValue::Float(6.0), OwnedValue::Integer(2)),
            (OwnedValue::Integer(6), OwnedValue::Integer(2)),
            (OwnedValue::Null, OwnedValue::Integer(2)),
            (OwnedValue::Integer(2), OwnedValue::Null),
            (OwnedValue::Null, OwnedValue::Null),
            (
                OwnedValue::Text(Text::from_str("6")),
                OwnedValue::Text(Text::from_str("2")),
            ),
            (
                OwnedValue::Text(Text::from_str("6")),
                OwnedValue::Integer(2),
            ),
        ];

        let outputs = [
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Float(9.223372036854776e18),
            OwnedValue::Float(3.0),
            OwnedValue::Float(3.0),
            OwnedValue::Float(3.0),
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Float(3.0),
            OwnedValue::Float(3.0),
        ];

        assert_eq!(
            inputs.len(),
            outputs.len(),
            "Inputs and Outputs should have same size"
        );
        for (i, (lhs, rhs)) in inputs.iter().enumerate() {
            assert_eq!(
                exec_divide(lhs, rhs),
                outputs[i],
                "Wrong divide for lhs: {}, rhs: {}",
                lhs,
                rhs
            );
        }
    }

    use super::exec_remainder;
    #[test]
    fn test_exec_remainder() {
        let inputs = vec![
            (OwnedValue::Null, OwnedValue::Null),
            (OwnedValue::Null, OwnedValue::Float(1.0)),
            (OwnedValue::Null, OwnedValue::Integer(1)),
            (OwnedValue::Null, OwnedValue::Text(Text::from_str("1"))),
            (OwnedValue::Float(1.0), OwnedValue::Null),
            (OwnedValue::Integer(1), OwnedValue::Null),
            (OwnedValue::Integer(12), OwnedValue::Integer(0)),
            (OwnedValue::Float(12.0), OwnedValue::Float(0.0)),
            (OwnedValue::Float(12.0), OwnedValue::Integer(0)),
            (OwnedValue::Integer(12), OwnedValue::Float(0.0)),
            (OwnedValue::Integer(12), OwnedValue::Integer(3)),
            (OwnedValue::Float(12.0), OwnedValue::Float(3.0)),
            (OwnedValue::Float(12.0), OwnedValue::Integer(3)),
            (OwnedValue::Integer(12), OwnedValue::Float(3.0)),
            (
                OwnedValue::Text(Text::from_str("12.0")),
                OwnedValue::Text(Text::from_str("3.0")),
            ),
            (
                OwnedValue::Text(Text::from_str("12.0")),
                OwnedValue::Float(3.0),
            ),
            (
                OwnedValue::Float(12.0),
                OwnedValue::Text(Text::from_str("12.0")),
            ),
        ];
        let outputs = vec![
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Integer(0),
            OwnedValue::Float(0.0),
            OwnedValue::Float(0.0),
            OwnedValue::Float(0.0),
            OwnedValue::Float(0.0),
            OwnedValue::Float(0.0),
            OwnedValue::Float(0.0),
        ];

        assert_eq!(
            inputs.len(),
            outputs.len(),
            "Inputs and Outputs should have same size"
        );

        for (i, (lhs, rhs)) in inputs.iter().enumerate() {
            assert_eq!(
                exec_remainder(lhs, rhs),
                outputs[i],
                "Wrong remainder for lhs: {}, rhs: {}",
                lhs,
                rhs
            );
        }
    }

    use super::exec_and;

    #[test]
    fn test_exec_and() {
        let inputs = vec![
            (OwnedValue::Integer(0), OwnedValue::Null),
            (OwnedValue::Null, OwnedValue::Integer(1)),
            (OwnedValue::Null, OwnedValue::Null),
            (OwnedValue::Float(0.0), OwnedValue::Null),
            (OwnedValue::Integer(1), OwnedValue::Float(2.2)),
            (
                OwnedValue::Integer(0),
                OwnedValue::Text(Text::from_str("string")),
            ),
            (
                OwnedValue::Integer(0),
                OwnedValue::Text(Text::from_str("1")),
            ),
            (
                OwnedValue::Integer(1),
                OwnedValue::Text(Text::from_str("1")),
            ),
        ];
        let outputs = [
            OwnedValue::Integer(0),
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Integer(0),
            OwnedValue::Integer(1),
            OwnedValue::Integer(0),
            OwnedValue::Integer(0),
            OwnedValue::Integer(1),
        ];

        assert_eq!(
            inputs.len(),
            outputs.len(),
            "Inputs and Outputs should have same size"
        );
        for (i, (lhs, rhs)) in inputs.iter().enumerate() {
            assert_eq!(
                exec_and(lhs, rhs),
                outputs[i],
                "Wrong AND for lhs: {}, rhs: {}",
                lhs,
                rhs
            );
        }
    }

    #[test]
    fn test_exec_or() {
        let inputs = vec![
            (OwnedValue::Integer(0), OwnedValue::Null),
            (OwnedValue::Null, OwnedValue::Integer(1)),
            (OwnedValue::Null, OwnedValue::Null),
            (OwnedValue::Float(0.0), OwnedValue::Null),
            (OwnedValue::Integer(1), OwnedValue::Float(2.2)),
            (OwnedValue::Float(0.0), OwnedValue::Integer(0)),
            (
                OwnedValue::Integer(0),
                OwnedValue::Text(Text::from_str("string")),
            ),
            (
                OwnedValue::Integer(0),
                OwnedValue::Text(Text::from_str("1")),
            ),
            (OwnedValue::Integer(0), OwnedValue::Text(Text::from_str(""))),
        ];
        let outputs = [
            OwnedValue::Null,
            OwnedValue::Integer(1),
            OwnedValue::Null,
            OwnedValue::Null,
            OwnedValue::Integer(1),
            OwnedValue::Integer(0),
            OwnedValue::Integer(0),
            OwnedValue::Integer(1),
            OwnedValue::Integer(0),
        ];

        assert_eq!(
            inputs.len(),
            outputs.len(),
            "Inputs and Outputs should have same size"
        );
        for (i, (lhs, rhs)) in inputs.iter().enumerate() {
            assert_eq!(
                exec_or(lhs, rhs),
                outputs[i],
                "Wrong OR for lhs: {}, rhs: {}",
                lhs,
                rhs
            );
        }
    }
}
