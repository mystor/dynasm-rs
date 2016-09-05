extern crate memmap;

use std::collections::HashMap;
use std::collections::hash_map::Entry::*;
use std::ops::Deref;
use std::iter::Extend;
use std::mem;
use std::cmp;
use std::ops::DerefMut;
use std::sync::{Arc, RwLock, RwLockReadGuard};

use memmap::{Mmap, Protection};

/// This macro takes a *const pointer from the source operand, and then casts it to the desired return type.
/// this allows it to be used as an easy shorthand for passing pointers as dynasm immediate arguments.
#[macro_export]
macro_rules! Pointer {
    ($e:expr) => {$e as *const _ as _};
}

/// Preforms the same action as the Pointer! macro, but casts to a *mut pointer.
#[macro_export]
macro_rules! MutPointer {
    ($e:expr) => {$e as *mut _ as _};
}

/// A struct representing an offset into the assembling buffer of a `DynasmApi` struct.
/// The wrapped `usize` is the offset from the start of the assembling buffer in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssemblyOffset(pub usize);

/// A dynamic label
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DynamicLabel(usize);

#[derive(Debug)]
struct PatchLoc(usize, u8);

/// A structure holding a buffer of executable memory
#[derive(Debug)]
pub struct ExecutableBuffer {
    // length of the buffer that has actually been written to
    length: usize,
    // backing buffer
    buffer: Mmap
}

/// A structure wrapping some executable memory. It dereferences into a &[u8] slice.
impl ExecutableBuffer {
    /// Obtain a pointer into the executable memory from an offset into it.
    /// When an offset returned from `DynasmApi::offset` is used, the resulting pointer
    /// will point to the start of the first instruction after the offset call,
    /// which can then be jumped or called to divert control flow into the executable
    /// buffer. Note that if this buffer is accessed through an Executor, these pointers
    /// will only be valid as long as its lock is held. When no locks are held,
    /// The assembler is free to relocate the executable buffer when it requires
    /// more memory than available.
    pub fn ptr(&self, offset: AssemblyOffset) -> *const u8 {
        &self[offset.0] as *const u8
    }

    fn as_mut_slice(&mut self) -> &mut[u8] {
        unsafe {&mut self.buffer.as_mut_slice()[..self.length] }
    }
}

impl Deref for ExecutableBuffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { &self.buffer.as_slice()[..self.length] }
    }
}

/// A read-only shared reference to the executable buffer inside an Assembler. By
/// locking it the internal `ExecutableBuffer` can be accessed and executed.
#[derive(Debug, Clone)]
pub struct Executor {
    execbuffer: Arc<RwLock<ExecutableBuffer>>
}

/// A read-only lockable reference to the internal `ExecutableBuffer` of an Assembler.
/// To gain access to this buffer, it must be locked.
impl Executor {
    /// Gain read-access to the internal `ExecutableBuffer`. While the returned guard
    /// is alive, it can be used to read and execute from the `ExecutableBuffer`.
    /// Any pointers created to the `Executablebuffer` should no longer be used when
    /// the guard is dropped.
    pub fn lock(&self) -> RwLockReadGuard<ExecutableBuffer> {
        self.execbuffer.read().unwrap()
    }
}

/// This trait represents the interface that must be implemented to allow
/// the dynasm preprocessor to assemble into a datastructure.
pub trait DynasmApi<'a> : Extend<u8> + Extend<&'a u8> {
    /// Report the current offset into the assembling target
    fn offset(&self) -> AssemblyOffset;
    /// Push a byte into the assembling target
    fn push(&mut self, byte: u8);
    /// Push a signed byte into the assembling target
    #[inline]
    fn push_i8(&mut self, value: i8) {
        self.push(value as u8);
    }
    /// Push a signed word into the assembling target
    #[inline]
    fn push_i16(&mut self, value: i16) {
        self.extend(unsafe {
            mem::transmute::<_, [u8; 2]>(value.to_le())
        }.iter().cloned());
    }
    /// Push a signed doubleword into the assembling target
    #[inline]
    fn push_i32(&mut self, value: i32) {
        self.extend(unsafe {
            mem::transmute::<_, [u8; 4]>(value.to_le())
        }.iter().cloned());
    }
    /// Push a signed quadword into the assembling target
    #[inline]
    fn push_i64(&mut self, value: i64) {
        self.extend(unsafe {
            mem::transmute::<_, [u8; 8]>(value.to_le())
        }.iter().cloned());
    }
    /// Push nops until the assembling target end is aligned to the given alignment
    fn align(&mut self, alignment: usize);
    /// Record the definition of a local label
    fn local_label(  &mut self, name: &'static str);
    /// Record the definition of a global label
    fn global_label( &mut self, name: &'static str);
    /// Record the definition of a dynamic label
    fn dynamic_label(&mut self, id: DynamicLabel);

    /// Record a relocation spot for a forward reference to a local label
    fn forward_reloc( &mut self, name: &'static str, size: u8);
    /// Record a relocation spot for a backward reference to a local label
    fn backward_reloc(&mut self, name: &'static str, size: u8);
    /// Record a relocation spot for a reference to a global label
    fn global_reloc(  &mut self, name: &'static str, size: u8);
    /// Record a relocation spot for a reference to a dynamic label
    fn dynamic_reloc( &mut self, id: DynamicLabel,   size: u8);

    /// This function is called in when a runtime error has to be generated. It panics.
    #[inline]
    fn runtime_error(&self, msg: &'static str) -> ! {
        panic!(msg);
    }
}

/// This struct is an implementation of a dynasm runtime. It supports incremental
/// compilation as well as multithreaded execution with simultaneous compilation.
/// Its implementation ensures that no memory is writeable and executable at the
/// same time.
#[derive(Debug)]
pub struct Assembler {
    // buffer where the end result is copied into
    execbuffer: Arc<RwLock<ExecutableBuffer>>,
    // length of the allocated mmap (so we don't have to go through RwLock to get it)
    map_len: usize,

    // offset of the buffer that's being assembled into to the start of the execbuffer
    asmoffset: usize,
    // instruction buffer while building the assembly
    ops: Vec<u8>,

    // label name -> target loc
    global_labels: HashMap<&'static str, usize>,
    // end of patch location -> name
    global_relocs: Vec<(PatchLoc, &'static str)>,

    // label id -> target loc
    dynamic_labels: Vec<Option<usize>>,
    // location to be resolved, loc, label id
    dynamic_relocs: Vec<(PatchLoc, DynamicLabel)>,

    // labelname -> most recent patch location
    local_labels: HashMap<&'static str, usize>,
    // locations to be patched once this label gets seen. name -> Vec<locs>
    local_relocs: HashMap<&'static str, Vec<PatchLoc>>
}

impl Assembler {
    /// Create a new `Assembler` instance
    pub fn new() -> Assembler {
        const MMAP_INIT_SIZE: usize = 1024 * 256;
        Assembler {
            execbuffer: Arc::new(RwLock::new(ExecutableBuffer {
                length: 0,
                buffer: Mmap::anonymous(MMAP_INIT_SIZE, Protection::ReadExecute).unwrap()
            })),
            asmoffset: 0,
            map_len: MMAP_INIT_SIZE,
            ops: Vec::new(),
            global_labels: HashMap::new(),
            dynamic_labels: Vec::new(),
            local_labels: HashMap::new(),
            global_relocs: Vec::new(),
            dynamic_relocs: Vec::new(),
            local_relocs: HashMap::new()
        }
    }

    /// Create a new dynamic label that can be referenced and defined.
    pub fn new_dynamic_label(&mut self) -> DynamicLabel {
        let id = self.dynamic_labels.len();
        self.dynamic_labels.push(None);
        DynamicLabel(id)
    }

    /// To allow already committed code to be altered, this method allows modification
    /// of the internal ExecutableBuffer directly. When this method is called, all
    /// data will be committed and access to the internal `ExecutableBuffer` will be locked.
    /// The passed function will then be called with an `AssemblyModifier` as argument.
    /// Using this `AssemblyModifier` changes can be made to the committed code.
    /// After this function returns, any labels in these changes will be resolved
    /// and the `ExecutableBuffer` will be unlocked again.
    pub fn alter<F>(&mut self, f: F) where F: FnOnce(&mut AssemblyModifier) -> () {
        self.commit();
        let asmoffset = self.asmoffset;
        self.asmoffset = 0;

        let lock = self.execbuffer.clone();
        let mut lock = lock.write().unwrap();
        let buf = lock.deref_mut();
        buf.buffer.set_protection(Protection::ReadWrite).unwrap();

        {
            let mut m = AssemblyModifier {
                assembler: self,
                buffer: buf
            };
            f(&mut m);
            m.encode_relocs();
        }

        buf.buffer.set_protection(Protection::ReadExecute).unwrap();
        self.asmoffset = asmoffset;
        // no commit is required as we directly modified the buffer.
    }

    #[inline]
    fn patch_loc(&mut self, loc: PatchLoc, target: usize) {
        let buf_loc = loc.0 - self.asmoffset;
        let buf = &mut self.ops[buf_loc - loc.1 as usize .. buf_loc];
        let target = target as isize - loc.0 as isize;

        unsafe { match loc.1 {
            1 => buf.copy_from_slice(&mem::transmute::<_, [u8; 1]>( (target as i8 ).to_le() )),
            2 => buf.copy_from_slice(&mem::transmute::<_, [u8; 2]>( (target as i16).to_le() )),
            4 => buf.copy_from_slice(&mem::transmute::<_, [u8; 4]>( (target as i32).to_le() )),
            8 => buf.copy_from_slice(&mem::transmute::<_, [u8; 8]>( (target as i64).to_le() )),
            _ => panic!("invalid patch size")
        } }
    }

    fn encode_relocs(&mut self) {
        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.global_relocs);
        for (loc, name) in relocs {
            if let Some(&target) = self.global_labels.get(&name) {
                self.patch_loc(loc, target)
            } else {
                panic!("Unkonwn global label '{}'", name);
            }
        }

        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.dynamic_relocs);
        for (loc, id) in relocs {
            if let Some(&Some(target)) = self.dynamic_labels.get(id.0) {
                self.patch_loc(loc, target)
            } else {
                panic!("Unkonwn dynamic label '{}'", id.0);
            }
        }

        if let Some(name) = self.local_relocs.keys().next() {
            panic!("Unknown local label '{}'", name);
        }
    }

    /// Commit the assembled code from a temporary buffer to the executable buffer.
    /// This method requires write access to the execution buffer and therefore
    /// has to obtain a lock on the datastructure. When this method is called, all
    /// labels will be resolved, and the result can no longer be changed.
    pub fn commit(&mut self) {
        // This is where the part overridden by the current assembling buffer starts.
        // This is guaranteed to be in the actual backing buffer.
        let buf_start = self.asmoffset;
        // and this is where it ends. This is not guaranteed to be in the actual mmap
        let buf_end = self.offset().0;
        // is there any work to do?
        if buf_start == buf_end {
            return
        }
        // finalize all relocs in the newest part.
        self.encode_relocs();

        let same    =          ..buf_start;
        let changed = buf_start..buf_end;

        // The reason we don't have to copy the part after buf_end here is because we will only
        // enter the resize branch if all data past buf_start has been overwritten if we're in an
        // alter invocation
        if buf_end > self.map_len {
            // create a new buffer of the necessary size max(current_buf_len * 2, wanted_len)
            let map_len = cmp::max(buf_end, self.map_len * 2);
            let mut new_buf = Mmap::anonymous(map_len, Protection::ReadWrite).unwrap();
            self.map_len = new_buf.len();

            // copy over from the old buffer and the asm buffer (unsafe is completely safe due to use of anonymous mappings)
            unsafe {
                new_buf.as_mut_slice()[same].copy_from_slice(&self.execbuffer.read().unwrap().buffer.as_slice()[same]);
                new_buf.as_mut_slice()[changed].copy_from_slice(&self.ops);
            }
            new_buf.set_protection(Protection::ReadExecute).unwrap();

            // swap the buffers and the initialized length
            let mut data = ExecutableBuffer {
                length: buf_end,
                buffer: new_buf
            };
            mem::swap(&mut data, &mut self.execbuffer.write().unwrap());
            // and the old buffer is dropped.
        } else {
            // make the buffer writeable and copy things over.
            let mut data = self.execbuffer.write().unwrap();
            data.buffer.set_protection(Protection::ReadWrite).unwrap();
            unsafe {
                data.buffer.as_mut_slice()[changed].copy_from_slice(&self.ops);
            }
            data.buffer.set_protection(Protection::ReadExecute).unwrap();
            // update the length of the initialized part of the buffer, if this commit adds length
            if buf_end > data.length {
                data.length = buf_end;
            }
        }
        // empty the assembling buffer and update the assembling offset
        self.ops.clear();
        self.asmoffset = buf_end;
    }

    /// Consumes the assembler to return the internal ExecutableBuffer. This
    /// method will only fail if an `Executor` currently holds a lock on the datastructure,
    /// in which case it will return itself.
    pub fn finalize(mut self) -> Result<ExecutableBuffer, Assembler> {
        self.commit();
        match Arc::try_unwrap(self.execbuffer) {
            Ok(execbuffer) => Ok(execbuffer.into_inner().unwrap()),
            Err(arc) => Err(Assembler {
                execbuffer: arc,
                ..self
            })
        }
    }

    /// Creates a read-only reference to the internal `ExecutableBuffer` that must
    /// be locked to access it. Multiple of such read-only locks can be obtained
    /// at the same time, but as long as they are alive they will block any `self.commit()`
    /// calls.
    pub fn reader(&self) -> Executor {
        Executor {
            execbuffer: self.execbuffer.clone()
        }
    }
}

impl<'a> DynasmApi<'a> for Assembler {
    #[inline]
    fn offset(&self) -> AssemblyOffset {
        AssemblyOffset(self.ops.len() + self.asmoffset)
    }

    #[inline]
    fn push(&mut self, value: u8) {
        self.ops.push(value);
    }

    #[inline]
    fn align(&mut self, alignment: usize) {
        let offset = self.offset().0 % alignment;
        if offset != 0 {
            for _ in 0..(alignment - offset) {
                self.push(0x90);
            }
        }
    }

    #[inline]
    fn global_label(&mut self, name: &'static str) {
        let offset = self.offset().0;
        if let Some(name) = self.global_labels.insert(name, offset) {
            panic!("Duplicate global label '{}'", name);
        }
    }

    #[inline]
    fn global_reloc(&mut self, name: &'static str, size: u8) {
        let offset = self.offset().0;
        self.global_relocs.push((PatchLoc(offset, size), name));
    }

    #[inline]
    fn dynamic_label(&mut self, id: DynamicLabel) {
        let offset = self.offset().0;
        let entry = &mut self.dynamic_labels[id.0];
        if entry.is_some() {
            panic!("Duplicate label '{}'", id.0);
        }
        *entry = Some(offset);
    }

    #[inline]
    fn dynamic_reloc(&mut self, id: DynamicLabel, size: u8) {
        let offset = self.offset().0;
        self.dynamic_relocs.push((PatchLoc(offset, size), id));
    }

    #[inline]
    fn local_label(&mut self, name: &'static str) {
        let offset = self.offset().0;
        if let Some(relocs) = self.local_relocs.remove(&name) {
            for loc in relocs {
                self.patch_loc(loc, offset);
            }
        }
        self.local_labels.insert(name, offset);
    }

    #[inline]
    fn forward_reloc(&mut self, name: &'static str, size: u8) {
        let offset = self.offset().0;
        match self.local_relocs.entry(name) {
            Occupied(mut o) => {
                o.get_mut().push(PatchLoc(offset, size));
            },
            Vacant(v) => {
                v.insert(vec![PatchLoc(offset, size)]);
            }
        }
    }

    #[inline]
    fn backward_reloc(&mut self, name: &'static str, size: u8) {
        if let Some(&target) = self.local_labels.get(&name) {
            let len = self.offset().0;
            self.patch_loc(PatchLoc(len, size), target)
        } else {
            panic!("Unknown local label '{}'", name);
        }
    }
}

impl Extend<u8> for Assembler {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=u8> {
        self.ops.extend(iter)
    }
}

impl<'a> Extend<&'a u8> for Assembler {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=&'a u8> {
        self.extend(iter.into_iter().cloned())
    }
}


/// This struct is a wrapper around an `Assembler` normally created using the
/// `Assembler.alter` method. Instead of writing to a temporary assembling buffer,
/// this struct assembles directly into an executable buffer. The `goto` method can
/// be used to set the assembling offset in the `ExecutableBuffer` of the assembler
/// (this offset is initialized to 0) after which the data at this location can be
/// overwritten by assembling into this struct.
pub struct AssemblyModifier<'a: 'b, 'b> {
    assembler: &'a mut Assembler,
    buffer: &'b mut ExecutableBuffer
}

impl<'a, 'b> AssemblyModifier<'a, 'b> {
    /// Sets the current modification offset to the given value
    #[inline]
    pub fn goto(&mut self, offset: AssemblyOffset) {
        self.assembler.asmoffset = offset.0;
    }

    /// Checks that the current modification offset is not larger than the specified offset.
    /// If this is violated, it panics.
    #[inline]
    pub fn check(&mut self, offset: AssemblyOffset) {
        if self.assembler.asmoffset > offset.0 {
            panic!("specified offset to check is smaller than the actual offset");
        }
    }

    #[inline]
    fn patch_loc(&mut self, loc: PatchLoc, target: usize) {
        let buf = &mut self.buffer.as_mut_slice()[loc.0 - loc.1 as usize .. loc.0];
        let target = target as isize - loc.0 as isize;

        unsafe { match loc.1 {
            1 => buf.copy_from_slice(&mem::transmute::<_, [u8; 1]>( (target as i8 ).to_le() )),
            2 => buf.copy_from_slice(&mem::transmute::<_, [u8; 2]>( (target as i16).to_le() )),
            4 => buf.copy_from_slice(&mem::transmute::<_, [u8; 4]>( (target as i32).to_le() )),
            8 => buf.copy_from_slice(&mem::transmute::<_, [u8; 8]>( (target as i64).to_le() )),
            _ => panic!("invalid patch size")
        } }
    }

    fn encode_relocs(&mut self) {
        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.assembler.global_relocs);
        for (loc, name) in relocs {
            if let Some(&target) = self.assembler.global_labels.get(&name) {
                self.patch_loc(loc, target)
            } else {
                panic!("Unkonwn global label '{}'", name);
            }
        }

        let mut relocs = Vec::new();
        mem::swap(&mut relocs, &mut self.assembler.dynamic_relocs);
        for (loc, id) in relocs {
            if let Some(&Some(target)) = self.assembler.dynamic_labels.get(id.0) {
                self.patch_loc(loc, target)
            } else {
                panic!("Unkonwn dynamic label '{}'", id.0);
            }
        }

        if let Some(name) = self.assembler.local_relocs.keys().next() {
            panic!("Unknown local label '{}'", name);
        }
    }
}

impl<'a, 'b, 'c> DynasmApi<'c> for AssemblyModifier<'a, 'b> {
    #[inline]
    fn offset(&self) -> AssemblyOffset {
        self.assembler.offset()
    }

    #[inline]
    fn push(&mut self, value: u8) {
        self.buffer.as_mut_slice()[self.assembler.asmoffset] = value;
        self.assembler.asmoffset += 1;
    }

    #[inline]
    fn align(&mut self, alignment: usize) {
        self.assembler.align(alignment);
    }

    #[inline]
    fn global_label(&mut self, name: &'static str) {
        self.assembler.global_label(name);
    }

    #[inline]
    fn global_reloc(&mut self, name: &'static str, size: u8) {
        self.assembler.global_reloc(name, size);
    }

    #[inline]
    fn dynamic_label(&mut self, id: DynamicLabel) {
        self.assembler.dynamic_label(id);
    }

    #[inline]
    fn dynamic_reloc(&mut self, id: DynamicLabel, size: u8) {
        self.assembler.dynamic_reloc(id, size);
    }

    #[inline]
    fn local_label(&mut self, name: &'static str) {
        let offset = self.offset().0;
        if let Some(relocs) = self.assembler.local_relocs.remove(&name) {
            for loc in relocs {
                self.patch_loc(loc, offset);
            }
        }
        self.assembler.local_labels.insert(name, offset);
    }

    #[inline]
    fn forward_reloc(&mut self, name: &'static str, size: u8) {
        self.assembler.forward_reloc(name, size);
    }

    #[inline]
    fn backward_reloc(&mut self, name: &'static str, size: u8) {
        if let Some(&target) = self.assembler.local_labels.get(&name) {
            let len = self.offset().0;
            self.patch_loc(PatchLoc(len, size), target)
        } else {
            panic!("Unknown local label '{}'", name);
        }
    }
}

impl<'a, 'b> Extend<u8> for AssemblyModifier<'a, 'b> {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=u8> {
        for i in iter {
            self.push(i)
        }
    }
}

impl<'a, 'b, 'c> Extend<&'c u8> for AssemblyModifier<'a, 'b> {
    #[inline]
    fn extend<T>(&mut self, iter: T) where T: IntoIterator<Item=&'c u8> {
        self.extend(iter.into_iter().cloned())
    }
}
