use rustc::ty;
use rustc::ty::layout::{Align, LayoutOf, Size};
use rustc::hir::def_id::DefId;
use rustc::mir;
use syntax::attr;
use syntax::symbol::sym;

use rand::RngCore;

use crate::*;

impl<'mir, 'tcx> EvalContextExt<'mir, 'tcx> for crate::MiriEvalContext<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriEvalContextExt<'mir, 'tcx> {
    fn find_fn(
        &mut self,
        instance: ty::Instance<'tcx>,
        args: &[OpTy<'tcx, Tag>],
        dest: Option<PlaceTy<'tcx, Tag>>,
        ret: Option<mir::BasicBlock>,
    ) -> InterpResult<'tcx, Option<&'mir mir::Body<'tcx>>> {
        let this = self.eval_context_mut();
        trace!("eval_fn_call: {:#?}, {:?}", instance, dest.map(|place| *place));

        // First, run the common hooks also supported by CTFE.
        if this.hook_fn(instance, args, dest)? {
            this.goto_block(ret)?;
            return Ok(None);
        }
        // There are some more lang items we want to hook that CTFE does not hook (yet).
        if this.tcx.lang_items().align_offset_fn() == Some(instance.def.def_id()) {
            // FIXME: return a real value in case the target allocation has an
            // alignment bigger than the one requested.
            let n = u128::max_value();
            let dest = dest.unwrap();
            let n = this.truncate(n, dest.layout);
            this.write_scalar(Scalar::from_uint(n, dest.layout.size), dest)?;
            this.goto_block(ret)?;
            return Ok(None);
        }

        // Try to see if we can do something about foreign items.
        if this.tcx.is_foreign_item(instance.def_id()) {
            // An external function that we cannot find MIR for, but we can still run enough
            // of them to make miri viable.
            this.emulate_foreign_item(instance.def_id(), args, dest, ret)?;
            // `goto_block` already handled.
            return Ok(None);
        }

        // Otherwise, load the MIR.
        Ok(Some(this.load_mir(instance.def)?))
    }

    fn malloc(
        &mut self,
        size: u64,
        zero_init: bool,
    ) -> Scalar<Tag> {
        let this = self.eval_context_mut();
        let tcx = &{this.tcx.tcx};
        if size == 0 {
            Scalar::from_int(0, this.pointer_size())
        } else {
            let align = this.tcx.data_layout.pointer_align.abi;
            let ptr = this.memory_mut().allocate(Size::from_bytes(size), align, MiriMemoryKind::C.into());
            if zero_init {
                // We just allocated this, the access cannot fail
                this.memory_mut()
                    .get_mut(ptr.alloc_id).unwrap()
                    .write_repeat(tcx, ptr, 0, Size::from_bytes(size)).unwrap();
            }
            Scalar::Ptr(ptr)
        }
    }

    fn free(
        &mut self,
        ptr: Scalar<Tag>,
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        if !ptr.is_null_ptr(this) {
            this.memory_mut().deallocate(
                ptr.to_ptr()?,
                None,
                MiriMemoryKind::C.into(),
            )?;
        }
        Ok(())
    }

    fn realloc(
        &mut self,
        old_ptr: Scalar<Tag>,
        new_size: u64,
    ) -> InterpResult<'tcx, Scalar<Tag>> {
        let this = self.eval_context_mut();
        let align = this.tcx.data_layout.pointer_align.abi;
        if old_ptr.is_null_ptr(this) {
            if new_size == 0 {
                Ok(Scalar::from_int(0, this.pointer_size()))
            } else {
                let new_ptr = this.memory_mut().allocate(
                    Size::from_bytes(new_size),
                    align,
                    MiriMemoryKind::C.into()
                );
                Ok(Scalar::Ptr(new_ptr))
            }
        } else {
            let old_ptr = old_ptr.to_ptr()?;
            let memory = this.memory_mut();
            let old_size = Size::from_bytes(memory.get(old_ptr.alloc_id)?.bytes.len() as u64);
            if new_size == 0 {
                memory.deallocate(
                    old_ptr,
                    Some((old_size, align)),
                    MiriMemoryKind::C.into(),
                )?;
                Ok(Scalar::from_int(0, this.pointer_size()))
            } else {
                let new_ptr = memory.reallocate(
                    old_ptr,
                    old_size,
                    align,
                    Size::from_bytes(new_size),
                    align,
                    MiriMemoryKind::C.into(),
                )?;
                Ok(Scalar::Ptr(new_ptr))
            }
        }
    }

    /// Emulates calling a foreign item, failing if the item is not supported.
    /// This function will handle `goto_block` if needed.
    fn emulate_foreign_item(
        &mut self,
        def_id: DefId,
        args: &[OpTy<'tcx, Tag>],
        dest: Option<PlaceTy<'tcx, Tag>>,
        ret: Option<mir::BasicBlock>,
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        let attrs = this.tcx.get_attrs(def_id);
        let link_name = match attr::first_attr_value_str_by_name(&attrs, sym::link_name) {
            Some(name) => name.as_str(),
            None => this.tcx.item_name(def_id).as_str(),
        };
        // Strip linker suffixes (seen on 32-bit macOS).
        let link_name = link_name.get().trim_end_matches("$UNIX2003");
        let tcx = &{this.tcx.tcx};

        // First: functions that diverge.
        match link_name {
            "__rust_start_panic" | "panic_impl" => {
                return err!(MachineError("the evaluated program panicked".to_string()));
            }
            "exit" | "ExitProcess" => {
                // it's really u32 for ExitProcess, but we have to put it into the `Exit` error variant anyway
                let code = this.read_scalar(args[0])?.to_i32()?;
                return err!(Exit(code));
            }
            _ => if dest.is_none() {
                return err!(Unimplemented(
                    format!("can't call diverging foreign function: {}", link_name),
                ));
            }
        }

        // Next: functions that assume a ret and dest.
        let dest = dest.expect("we already checked for a dest");
        let ret = ret.expect("dest is `Some` but ret is `None`");
        match link_name {
            "malloc" => {
                let size = this.read_scalar(args[0])?.to_usize(this)?;
                let res = this.malloc(size, /*zero_init:*/ false);
                this.write_scalar(res, dest)?;
            }
            "calloc" => {
                let items = this.read_scalar(args[0])?.to_usize(this)?;
                let len = this.read_scalar(args[1])?.to_usize(this)?;
                let size = items.checked_mul(len).ok_or_else(|| InterpError::Overflow(mir::BinOp::Mul))?;
                let res = this.malloc(size, /*zero_init:*/ true);
                this.write_scalar(res, dest)?;
            }
            "posix_memalign" => {
                let ret = this.deref_operand(args[0])?;
                let align = this.read_scalar(args[1])?.to_usize(this)?;
                let size = this.read_scalar(args[2])?.to_usize(this)?;
                // Align must be power of 2, and also at least ptr-sized (POSIX rules).
                if !align.is_power_of_two() {
                    return err!(HeapAllocNonPowerOfTwoAlignment(align));
                }
                if align < this.pointer_size().bytes() {
                    return err!(MachineError(format!(
                        "posix_memalign: alignment must be at least the size of a pointer, but is {}",
                        align,
                    )));
                }
                if size == 0 {
                    this.write_null(ret.into())?;
                } else {
                    let ptr = this.memory_mut().allocate(
                        Size::from_bytes(size),
                        Align::from_bytes(align).unwrap(),
                        MiriMemoryKind::C.into()
                    );
                    this.write_scalar(Scalar::Ptr(ptr), ret.into())?;
                }
                this.write_null(dest)?;
            }
            "free" => {
                let ptr = this.read_scalar(args[0])?.not_undef()?;
                this.free(ptr)?;
            }
            "realloc" => {
                let old_ptr = this.read_scalar(args[0])?.not_undef()?;
                let new_size = this.read_scalar(args[1])?.to_usize(this)?;
                let res = this.realloc(old_ptr, new_size)?;
                this.write_scalar(res, dest)?;
            }

            "__rust_alloc" => {
                let size = this.read_scalar(args[0])?.to_usize(this)?;
                let align = this.read_scalar(args[1])?.to_usize(this)?;
                if size == 0 {
                    return err!(HeapAllocZeroBytes);
                }
                if !align.is_power_of_two() {
                    return err!(HeapAllocNonPowerOfTwoAlignment(align));
                }
                let ptr = this.memory_mut()
                    .allocate(
                        Size::from_bytes(size),
                        Align::from_bytes(align).unwrap(),
                        MiriMemoryKind::Rust.into()
                    );
                this.write_scalar(Scalar::Ptr(ptr), dest)?;
            }
            "__rust_alloc_zeroed" => {
                let size = this.read_scalar(args[0])?.to_usize(this)?;
                let align = this.read_scalar(args[1])?.to_usize(this)?;
                if size == 0 {
                    return err!(HeapAllocZeroBytes);
                }
                if !align.is_power_of_two() {
                    return err!(HeapAllocNonPowerOfTwoAlignment(align));
                }
                let ptr = this.memory_mut()
                    .allocate(
                        Size::from_bytes(size),
                        Align::from_bytes(align).unwrap(),
                        MiriMemoryKind::Rust.into()
                    );
                this.memory_mut()
                    .get_mut(ptr.alloc_id)?
                    .write_repeat(tcx, ptr, 0, Size::from_bytes(size))?;
                this.write_scalar(Scalar::Ptr(ptr), dest)?;
            }
            "__rust_dealloc" => {
                let ptr = this.read_scalar(args[0])?.to_ptr()?;
                let old_size = this.read_scalar(args[1])?.to_usize(this)?;
                let align = this.read_scalar(args[2])?.to_usize(this)?;
                if old_size == 0 {
                    return err!(HeapAllocZeroBytes);
                }
                if !align.is_power_of_two() {
                    return err!(HeapAllocNonPowerOfTwoAlignment(align));
                }
                this.memory_mut().deallocate(
                    ptr,
                    Some((Size::from_bytes(old_size), Align::from_bytes(align).unwrap())),
                    MiriMemoryKind::Rust.into(),
                )?;
            }
            "__rust_realloc" => {
                let ptr = this.read_scalar(args[0])?.to_ptr()?;
                let old_size = this.read_scalar(args[1])?.to_usize(this)?;
                let align = this.read_scalar(args[2])?.to_usize(this)?;
                let new_size = this.read_scalar(args[3])?.to_usize(this)?;
                if old_size == 0 || new_size == 0 {
                    return err!(HeapAllocZeroBytes);
                }
                if !align.is_power_of_two() {
                    return err!(HeapAllocNonPowerOfTwoAlignment(align));
                }
                let new_ptr = this.memory_mut().reallocate(
                    ptr,
                    Size::from_bytes(old_size),
                    Align::from_bytes(align).unwrap(),
                    Size::from_bytes(new_size),
                    Align::from_bytes(align).unwrap(),
                    MiriMemoryKind::Rust.into(),
                )?;
                this.write_scalar(Scalar::Ptr(new_ptr), dest)?;
            }

            "syscall" => {
                let sys_getrandom = this.eval_path_scalar(&["libc", "SYS_getrandom"])?
                    .expect("Failed to get libc::SYS_getrandom")
                    .to_usize(this)?;

                // `libc::syscall(NR_GETRANDOM, buf.as_mut_ptr(), buf.len(), GRND_NONBLOCK)`
                // is called if a `HashMap` is created the regular way (e.g. HashMap<K, V>).
                match this.read_scalar(args[0])?.to_usize(this)? {
                    id if id == sys_getrandom => {
                        let ptr = this.read_scalar(args[1])?.not_undef()?;
                        let len = this.read_scalar(args[2])?.to_usize(this)?;

                        // The only supported flags are GRND_RANDOM and GRND_NONBLOCK,
                        // neither of which have any effect on our current PRNG
                        let _flags = this.read_scalar(args[3])?.to_i32()?;

                        gen_random(this, len as usize, ptr)?;
                        this.write_scalar(Scalar::from_uint(len, dest.layout.size), dest)?;
                    }
                    id => {
                        return err!(Unimplemented(
                            format!("miri does not support syscall ID {}", id),
                        ))
                    }
                }
            }

            "dlsym" => {
                let _handle = this.read_scalar(args[0])?;
                let symbol = this.read_scalar(args[1])?.to_ptr()?;
                let symbol_name = this.memory().get(symbol.alloc_id)?.read_c_str(tcx, symbol)?;
                let err = format!("bad c unicode symbol: {:?}", symbol_name);
                let symbol_name = ::std::str::from_utf8(symbol_name).unwrap_or(&err);
                return err!(Unimplemented(format!(
                    "miri does not support dynamically loading libraries (requested symbol: {})",
                    symbol_name
                )));
            }

            "__rust_maybe_catch_panic" => {
                // fn __rust_maybe_catch_panic(
                //     f: fn(*mut u8),
                //     data: *mut u8,
                //     data_ptr: *mut usize,
                //     vtable_ptr: *mut usize,
                // ) -> u32
                // We abort on panic, so not much is going on here, but we still have to call the closure.
                let f = this.read_scalar(args[0])?.to_ptr()?;
                let data = this.read_scalar(args[1])?.not_undef()?;
                let f_instance = this.memory().get_fn(f)?;
                this.write_null(dest)?;
                trace!("__rust_maybe_catch_panic: {:?}", f_instance);

                // Now we make a function call.
                // TODO: consider making this reusable? `InterpretCx::step` does something similar
                // for the TLS destructors, and of course `eval_main`.
                let mir = this.load_mir(f_instance.def)?;
                let ret_place = MPlaceTy::dangling(this.layout_of(this.tcx.mk_unit())?, this).into();
                this.push_stack_frame(
                    f_instance,
                    mir.span,
                    mir,
                    Some(ret_place),
                    // Directly return to caller.
                    StackPopCleanup::Goto(Some(ret)),
                )?;
                let mut args = this.frame().body.args_iter();

                let arg_local = args.next().ok_or_else(||
                    InterpError::AbiViolation(
                        "Argument to __rust_maybe_catch_panic does not take enough arguments."
                            .to_owned(),
                    ),
                )?;
                let arg_dest = this.eval_place(&mir::Place::Base(mir::PlaceBase::Local(arg_local)))?;
                this.write_scalar(data, arg_dest)?;

                assert!(args.next().is_none(), "__rust_maybe_catch_panic argument has more arguments than expected");

                // We ourselves will return `0`, eventually (because we will not return if we paniced).
                this.write_null(dest)?;

                // Don't fall through, we do *not* want to `goto_block`!
                return Ok(());
            }

            "memcmp" => {
                let left = this.read_scalar(args[0])?.not_undef()?;
                let right = this.read_scalar(args[1])?.not_undef()?;
                let n = Size::from_bytes(this.read_scalar(args[2])?.to_usize(this)?);

                let result = {
                    let left_bytes = this.memory().read_bytes(left, n)?;
                    let right_bytes = this.memory().read_bytes(right, n)?;

                    use std::cmp::Ordering::*;
                    match left_bytes.cmp(right_bytes) {
                        Less => -1i32,
                        Equal => 0,
                        Greater => 1,
                    }
                };

                this.write_scalar(
                    Scalar::from_int(result, Size::from_bits(32)),
                    dest,
                )?;
            }

            "memrchr" => {
                let ptr = this.read_scalar(args[0])?.not_undef()?;
                let val = this.read_scalar(args[1])?.to_i32()? as u8;
                let num = this.read_scalar(args[2])?.to_usize(this)?;
                if let Some(idx) = this.memory().read_bytes(ptr, Size::from_bytes(num))?
                    .iter().rev().position(|&c| c == val)
                {
                    let new_ptr = ptr.ptr_offset(Size::from_bytes(num - idx as u64 - 1), this)?;
                    this.write_scalar(new_ptr, dest)?;
                } else {
                    this.write_null(dest)?;
                }
            }

            "memchr" => {
                let ptr = this.read_scalar(args[0])?.not_undef()?;
                let val = this.read_scalar(args[1])?.to_i32()? as u8;
                let num = this.read_scalar(args[2])?.to_usize(this)?;
                let idx = this
                    .memory()
                    .read_bytes(ptr, Size::from_bytes(num))?
                    .iter()
                    .position(|&c| c == val);
                if let Some(idx) = idx {
                    let new_ptr = ptr.ptr_offset(Size::from_bytes(idx as u64), this)?;
                    this.write_scalar(new_ptr, dest)?;
                } else {
                    this.write_null(dest)?;
                }
            }

            "getenv" => {
                let result = {
                    let name_ptr = this.read_scalar(args[0])?.to_ptr()?;
                    let name = this.memory().get(name_ptr.alloc_id)?.read_c_str(tcx, name_ptr)?;
                    match this.machine.env_vars.get(name) {
                        Some(&var) => Scalar::Ptr(var),
                        None => Scalar::ptr_null(&*this.tcx),
                    }
                };
                this.write_scalar(result, dest)?;
            }

            "unsetenv" => {
                let mut success = None;
                {
                    let name_ptr = this.read_scalar(args[0])?.not_undef()?;
                    if !name_ptr.is_null_ptr(this) {
                        let name_ptr = name_ptr.to_ptr()?;
                        let name = this
                            .memory()
                            .get(name_ptr.alloc_id)?
                            .read_c_str(tcx, name_ptr)?
                            .to_owned();
                        if !name.is_empty() && !name.contains(&b'=') {
                            success = Some(this.machine.env_vars.remove(&name));
                        }
                    }
                }
                if let Some(old) = success {
                    if let Some(var) = old {
                        this.memory_mut().deallocate(var, None, MiriMemoryKind::Env.into())?;
                    }
                    this.write_null(dest)?;
                } else {
                    this.write_scalar(Scalar::from_int(-1, dest.layout.size), dest)?;
                }
            }

            "setenv" => {
                let mut new = None;
                {
                    let name_ptr = this.read_scalar(args[0])?.not_undef()?;
                    let value_ptr = this.read_scalar(args[1])?.to_ptr()?;
                    let value = this.memory().get(value_ptr.alloc_id)?.read_c_str(tcx, value_ptr)?;
                    if !name_ptr.is_null_ptr(this) {
                        let name_ptr = name_ptr.to_ptr()?;
                        let name = this.memory().get(name_ptr.alloc_id)?.read_c_str(tcx, name_ptr)?;
                        if !name.is_empty() && !name.contains(&b'=') {
                            new = Some((name.to_owned(), value.to_owned()));
                        }
                    }
                }
                if let Some((name, value)) = new {
                    // `+1` for the null terminator.
                    let value_copy = this.memory_mut().allocate(
                        Size::from_bytes((value.len() + 1) as u64),
                        Align::from_bytes(1).unwrap(),
                        MiriMemoryKind::Env.into(),
                    );
                    {
                        let alloc = this.memory_mut().get_mut(value_copy.alloc_id)?;
                        alloc.write_bytes(tcx, value_copy, &value)?;
                        let trailing_zero_ptr = value_copy.offset(
                            Size::from_bytes(value.len() as u64),
                            tcx,
                        )?;
                        alloc.write_bytes(tcx, trailing_zero_ptr, &[0])?;
                    }
                    if let Some(var) = this.machine.env_vars.insert(
                        name.to_owned(),
                        value_copy,
                    )
                    {
                        this.memory_mut().deallocate(var, None, MiriMemoryKind::Env.into())?;
                    }
                    this.write_null(dest)?;
                } else {
                    this.write_scalar(Scalar::from_int(-1, dest.layout.size), dest)?;
                }
            }

            "write" => {
                let fd = this.read_scalar(args[0])?.to_i32()?;
                let buf = this.read_scalar(args[1])?.not_undef()?;
                let n = this.read_scalar(args[2])?.to_usize(&*this.tcx)?;
                trace!("Called write({:?}, {:?}, {:?})", fd, buf, n);
                let result = if fd == 1 || fd == 2 {
                    // stdout/stderr
                    use std::io::{self, Write};

                    let buf_cont = this.memory().read_bytes(buf, Size::from_bytes(n))?;
                    // We need to flush to make sure this actually appears on the screen
                    let res = if fd == 1 {
                        // Stdout is buffered, flush to make sure it appears on the screen.
                        // This is the write() syscall of the interpreted program, we want it
                        // to correspond to a write() syscall on the host -- there is no good
                        // in adding extra buffering here.
                        let res = io::stdout().write(buf_cont);
                        io::stdout().flush().unwrap();
                        res
                    } else {
                        // No need to flush, stderr is not buffered.
                        io::stderr().write(buf_cont)
                    };
                    match res {
                        Ok(n) => n as i64,
                        Err(_) => -1,
                    }
                } else {
                    eprintln!("Miri: Ignored output to FD {}", fd);
                    // Pretend it all went well.
                    n as i64
                };
                // Now, `result` is the value we return back to the program.
                this.write_scalar(
                    Scalar::from_int(result, dest.layout.size),
                    dest,
                )?;
            }

            "strlen" => {
                let ptr = this.read_scalar(args[0])?.to_ptr()?;
                let n = this.memory().get(ptr.alloc_id)?.read_c_str(tcx, ptr)?.len();
                this.write_scalar(Scalar::from_uint(n as u64, dest.layout.size), dest)?;
            }

            // math functions

            "cbrtf" | "coshf" | "sinhf" |"tanf" => {
                // FIXME: Using host floats.
                let f = f32::from_bits(this.read_scalar(args[0])?.to_u32()?);
                let f = match link_name {
                    "cbrtf" => f.cbrt(),
                    "coshf" => f.cosh(),
                    "sinhf" => f.sinh(),
                    "tanf" => f.tan(),
                    _ => bug!(),
                };
                this.write_scalar(Scalar::from_u32(f.to_bits()), dest)?;
            }
            // underscore case for windows
            "_hypotf" | "hypotf" | "atan2f" => {
                // FIXME: Using host floats.
                let f1 = f32::from_bits(this.read_scalar(args[0])?.to_u32()?);
                let f2 = f32::from_bits(this.read_scalar(args[1])?.to_u32()?);
                let n = match link_name {
                    "_hypotf" | "hypotf" => f1.hypot(f2),
                    "atan2f" => f1.atan2(f2),
                    _ => bug!(),
                };
                this.write_scalar(Scalar::from_u32(n.to_bits()), dest)?;
            }

            "cbrt" | "cosh" | "sinh" | "tan" => {
                // FIXME: Using host floats.
                let f = f64::from_bits(this.read_scalar(args[0])?.to_u64()?);
                let f = match link_name {
                    "cbrt" => f.cbrt(),
                    "cosh" => f.cosh(),
                    "sinh" => f.sinh(),
                    "tan" => f.tan(),
                    _ => bug!(),
                };
                this.write_scalar(Scalar::from_u64(f.to_bits()), dest)?;
            }
            // underscore case for windows
            "_hypot" | "hypot" | "atan2" => {
                // FIXME: Using host floats.
                let f1 = f64::from_bits(this.read_scalar(args[0])?.to_u64()?);
                let f2 = f64::from_bits(this.read_scalar(args[1])?.to_u64()?);
                let n = match link_name {
                    "_hypot" | "hypot" => f1.hypot(f2),
                    "atan2" => f1.atan2(f2),
                    _ => bug!(),
                };
                this.write_scalar(Scalar::from_u64(n.to_bits()), dest)?;
            }

            // Some things needed for `sys::thread` initialization to go through.
            "signal" | "sigaction" | "sigaltstack" => {
                this.write_scalar(Scalar::from_int(0, dest.layout.size), dest)?;
            }

            "sysconf" => {
                let name = this.read_scalar(args[0])?.to_i32()?;

                trace!("sysconf() called with name {}", name);
                // TODO: Cache the sysconf integers via Miri's global cache.
                let paths = &[
                    (&["libc", "_SC_PAGESIZE"], Scalar::from_int(PAGE_SIZE, dest.layout.size)),
                    (&["libc", "_SC_GETPW_R_SIZE_MAX"], Scalar::from_int(-1, dest.layout.size)),
                    (&["libc", "_SC_NPROCESSORS_ONLN"], Scalar::from_int(NUM_CPUS, dest.layout.size)),
                ];
                let mut result = None;
                for &(path, path_value) in paths {
                    if let Some(val) = this.eval_path_scalar(path)? {
                        let val = val.to_i32()?;
                        if val == name {
                            result = Some(path_value);
                            break;
                        }

                    }
                }
                if let Some(result) = result {
                    this.write_scalar(result, dest)?;
                } else {
                    return err!(Unimplemented(
                        format!("Unimplemented sysconf name: {}", name),
                    ));
                }
            }

            "sched_getaffinity" => {
                // Return an error; `num_cpus` then falls back to `sysconf`.
                this.write_scalar(Scalar::from_int(-1, dest.layout.size), dest)?;
            }

            "isatty" => {
                this.write_null(dest)?;
            }

            // Hook pthread calls that go to the thread-local storage memory subsystem.
            "pthread_key_create" => {
                let key_ptr = this.read_scalar(args[0])?.not_undef()?;

                // Extract the function type out of the signature (that seems easier than constructing it ourselves).
                let dtor = match this.read_scalar(args[1])?.not_undef()? {
                    Scalar::Ptr(dtor_ptr) => Some(this.memory().get_fn(dtor_ptr)?),
                    Scalar::Raw { data: 0, size } => {
                        // NULL pointer
                        assert_eq!(size as u64, this.memory().pointer_size().bytes());
                        None
                    },
                    Scalar::Raw { .. } => return err!(ReadBytesAsPointer),
                };

                // Figure out how large a pthread TLS key actually is.
                // This is `libc::pthread_key_t`.
                let key_type = args[0].layout.ty
                    .builtin_deref(true)
                    .ok_or_else(|| InterpError::AbiViolation("wrong signature used for `pthread_key_create`: first argument must be a raw pointer.".to_owned()))?
                    .ty;
                let key_layout = this.layout_of(key_type)?;

                // Create key and write it into the memory where `key_ptr` wants it.
                let key = this.machine.tls.create_tls_key(dtor, tcx) as u128;
                if key_layout.size.bits() < 128 && key >= (1u128 << key_layout.size.bits() as u128) {
                    return err!(OutOfTls);
                }

                let key_ptr = this.memory().check_ptr_access(key_ptr, key_layout.size, key_layout.align.abi)?
                    .expect("cannot be a ZST");
                this.memory_mut().get_mut(key_ptr.alloc_id)?.write_scalar(
                    tcx,
                    key_ptr,
                    Scalar::from_uint(key, key_layout.size).into(),
                    key_layout.size,
                )?;

                // Return success (`0`).
                this.write_null(dest)?;
            }
            "pthread_key_delete" => {
                let key = this.read_scalar(args[0])?.to_bits(args[0].layout.size)?;
                this.machine.tls.delete_tls_key(key)?;
                // Return success (0)
                this.write_null(dest)?;
            }
            "pthread_getspecific" => {
                let key = this.read_scalar(args[0])?.to_bits(args[0].layout.size)?;
                let ptr = this.machine.tls.load_tls(key)?;
                this.write_scalar(ptr, dest)?;
            }
            "pthread_setspecific" => {
                let key = this.read_scalar(args[0])?.to_bits(args[0].layout.size)?;
                let new_ptr = this.read_scalar(args[1])?.not_undef()?;
                this.machine.tls.store_tls(key, new_ptr)?;

                // Return success (`0`).
                this.write_null(dest)?;
            }

            // Determine stack base address.
            "pthread_attr_init" | "pthread_attr_destroy" | "pthread_attr_get_np" |
            "pthread_getattr_np" | "pthread_self" | "pthread_get_stacksize_np" => {
                this.write_null(dest)?;
            }
            "pthread_attr_getstack" => {
                // Second argument is where we are supposed to write the stack size.
                let ptr = this.deref_operand(args[1])?;
                // Just any address.
                let stack_addr = Scalar::from_uint(STACK_ADDR, args[1].layout.size);
                this.write_scalar(stack_addr, ptr.into())?;
                // Return success (`0`).
                this.write_null(dest)?;
            }
            "pthread_get_stackaddr_np" => {
                // Just any address.
                let stack_addr = Scalar::from_uint(STACK_ADDR, dest.layout.size);
                this.write_scalar(stack_addr, dest)?;
            }

            // Stub out calls for condvar, mutex and rwlock, to just return `0`.
            "pthread_mutexattr_init" | "pthread_mutexattr_settype" | "pthread_mutex_init" |
            "pthread_mutexattr_destroy" | "pthread_mutex_lock" | "pthread_mutex_unlock" |
            "pthread_mutex_destroy" | "pthread_rwlock_rdlock" | "pthread_rwlock_unlock" |
            "pthread_rwlock_wrlock" | "pthread_rwlock_destroy" | "pthread_condattr_init" |
            "pthread_condattr_setclock" | "pthread_cond_init" | "pthread_condattr_destroy" |
            "pthread_cond_destroy" => {
                this.write_null(dest)?;
            }

            // We don't support fork so we don't have to do anything for atfork.
            "pthread_atfork" => {
                this.write_null(dest)?;
            }

            "mmap" => {
                // This is a horrible hack, but since the guard page mechanism calls mmap and expects a particular return value, we just give it that value.
                let addr = this.read_scalar(args[0])?.not_undef()?;
                this.write_scalar(addr, dest)?;
            }
            "mprotect" => {
                this.write_null(dest)?;
            }

            // macOS API stubs.
            "_tlv_atexit" => {
                // FIXME: register the destructor.
            },
            "_NSGetArgc" => {
                this.write_scalar(Scalar::Ptr(this.machine.argc.unwrap()), dest)?;
            },
            "_NSGetArgv" => {
                this.write_scalar(Scalar::Ptr(this.machine.argv.unwrap()), dest)?;
            },
            "SecRandomCopyBytes" => {
                let len = this.read_scalar(args[1])?.to_usize(this)?;
                let ptr = this.read_scalar(args[2])?.not_undef()?;
                gen_random(this, len as usize, ptr)?;
                this.write_null(dest)?;
            }

            // Windows API stubs.
            // HANDLE = isize
            // DWORD = ULONG = u32
            // BOOL = i32
            "GetProcessHeap" => {
                // Just fake a HANDLE
                this.write_scalar(Scalar::from_int(1, this.pointer_size()), dest)?;
            }
            "HeapAlloc" => {
                let _handle = this.read_scalar(args[0])?.to_isize(this)?;
                let flags = this.read_scalar(args[1])?.to_u32()?;
                let size = this.read_scalar(args[2])?.to_usize(this)?;
                let zero_init = (flags & 0x00000008) != 0; // HEAP_ZERO_MEMORY
                let res = this.malloc(size, zero_init);
                this.write_scalar(res, dest)?;
            }
            "HeapFree" => {
                let _handle = this.read_scalar(args[0])?.to_isize(this)?;
                let _flags = this.read_scalar(args[1])?.to_u32()?;
                let ptr = this.read_scalar(args[2])?.not_undef()?;
                this.free(ptr)?;
                this.write_scalar(Scalar::from_int(1, Size::from_bytes(4)), dest)?;
            }
            "HeapReAlloc" => {
                let _handle = this.read_scalar(args[0])?.to_isize(this)?;
                let _flags = this.read_scalar(args[1])?.to_u32()?;
                let ptr = this.read_scalar(args[2])?.not_undef()?;
                let size = this.read_scalar(args[3])?.to_usize(this)?;
                let res = this.realloc(ptr, size)?;
                this.write_scalar(res, dest)?;
            }

            "SetLastError" => {
                let err = this.read_scalar(args[0])?.to_u32()?;
                this.machine.last_error = err;
            }
            "GetLastError" => {
                this.write_scalar(Scalar::from_u32(this.machine.last_error), dest)?;
            }

            "AddVectoredExceptionHandler" => {
                // Any non zero value works for the stdlib. This is just used for stack overflows anyway.
                this.write_scalar(Scalar::from_int(1, dest.layout.size), dest)?;
            },
            "InitializeCriticalSection" |
            "EnterCriticalSection" |
            "LeaveCriticalSection" |
            "DeleteCriticalSection" => {
                // Nothing to do, not even a return value.
            },
            "GetModuleHandleW" |
            "GetProcAddress" |
            "TryEnterCriticalSection" |
            "GetConsoleScreenBufferInfo" |
            "SetConsoleTextAttribute" => {
                // Pretend these do not exist / nothing happened, by returning zero.
                this.write_null(dest)?;
            },
            "GetSystemInfo" => {
                let system_info = this.deref_operand(args[0])?;
                let system_info_ptr = system_info.ptr.to_ptr()?;
                // Initialize with `0`.
                this.memory_mut().get_mut(system_info_ptr.alloc_id)?
                    .write_repeat(tcx, system_info_ptr, 0, system_info.layout.size)?;
                // Set number of processors.
                let dword_size = Size::from_bytes(4);
                let offset = 2*dword_size + 3*tcx.pointer_size();
                this.memory_mut().get_mut(system_info_ptr.alloc_id)?
                    .write_scalar(
                        tcx,
                        system_info_ptr.offset(offset, tcx)?,
                        Scalar::from_int(NUM_CPUS, dword_size).into(),
                        dword_size,
                    )?;
            }

            "TlsAlloc" => {
                // This just creates a key; Windows does not natively support TLS destructors.

                // Create key and return it.
                let key = this.machine.tls.create_tls_key(None, tcx) as u128;

                // Figure out how large a TLS key actually is. This is `c::DWORD`.
                if dest.layout.size.bits() < 128
                        && key >= (1u128 << dest.layout.size.bits() as u128) {
                    return err!(OutOfTls);
                }
                this.write_scalar(Scalar::from_uint(key, dest.layout.size), dest)?;
            }
            "TlsGetValue" => {
                let key = this.read_scalar(args[0])?.to_u32()? as u128;
                let ptr = this.machine.tls.load_tls(key)?;
                this.write_scalar(ptr, dest)?;
            }
            "TlsSetValue" => {
                let key = this.read_scalar(args[0])?.to_u32()? as u128;
                let new_ptr = this.read_scalar(args[1])?.not_undef()?;
                this.machine.tls.store_tls(key, new_ptr)?;

                // Return success (`1`).
                this.write_scalar(Scalar::from_int(1, dest.layout.size), dest)?;
            }
            "GetStdHandle" => {
                let which = this.read_scalar(args[0])?.to_i32()?;
                // We just make this the identity function, so we know later in `WriteFile`
                // which one it is.
                this.write_scalar(Scalar::from_int(which, this.pointer_size()), dest)?;
            }
            "WriteFile" => {
                let handle = this.read_scalar(args[0])?.to_isize(this)?;
                let buf = this.read_scalar(args[1])?.not_undef()?;
                let n = this.read_scalar(args[2])?.to_u32()?;
                let written_place = this.deref_operand(args[3])?;
                // Spec says to always write `0` first.
                this.write_null(written_place.into())?;
                let written = if handle == -11 || handle == -12 {
                    // stdout/stderr
                    use std::io::{self, Write};

                    let buf_cont = this.memory().read_bytes(buf, Size::from_bytes(u64::from(n)))?;
                    let res = if handle == -11 {
                        io::stdout().write(buf_cont)
                    } else {
                        io::stderr().write(buf_cont)
                    };
                    res.ok().map(|n| n as u32)
                } else {
                    eprintln!("Miri: Ignored output to handle {}", handle);
                    // Pretend it all went well.
                    Some(n)
                };
                // If there was no error, write back how much was written.
                if let Some(n) = written {
                    this.write_scalar(Scalar::from_u32(n), written_place.into())?;
                }
                // Return whether this was a success.
                this.write_scalar(
                    Scalar::from_int(if written.is_some() { 1 } else { 0 }, dest.layout.size),
                    dest,
                )?;
            }
            "GetConsoleMode" => {
                // Everything is a pipe.
                this.write_null(dest)?;
            }
            "GetEnvironmentVariableW" => {
                // This is not the env var you are looking for.
                this.machine.last_error = 203; // ERROR_ENVVAR_NOT_FOUND
                this.write_null(dest)?;
            }
            "GetCommandLineW" => {
                this.write_scalar(Scalar::Ptr(this.machine.cmd_line.unwrap()), dest)?;
            }
            // The actual name of 'RtlGenRandom'
            "SystemFunction036" => {
                let ptr = this.read_scalar(args[0])?.not_undef()?;
                let len = this.read_scalar(args[1])?.to_u32()?;
                gen_random(this, len as usize, ptr)?;
                this.write_scalar(Scalar::from_bool(true), dest)?;
            }

            // We can't execute anything else.
            _ => {
                return err!(Unimplemented(
                    format!("can't call foreign function: {}", link_name),
                ));
            }
        }

        this.goto_block(Some(ret))?;
        this.dump_place(*dest);
        Ok(())
    }

    fn write_null(&mut self, dest: PlaceTy<'tcx, Tag>) -> InterpResult<'tcx> {
        self.eval_context_mut().write_scalar(Scalar::from_int(0, dest.layout.size), dest)
    }

    /// Evaluates the scalar at the specified path. Returns Some(val)
    /// if the path could be resolved, and None otherwise
    fn eval_path_scalar(&mut self, path: &[&str]) -> InterpResult<'tcx, Option<ScalarMaybeUndef<Tag>>> {
        let this = self.eval_context_mut();
        if let Ok(instance) = this.resolve_path(path) {
            let cid = GlobalId {
                instance,
                promoted: None,
            };
            let const_val = this.const_eval_raw(cid)?;
            let const_val = this.read_scalar(const_val.into())?;
            return Ok(Some(const_val));
        }
        return Ok(None);
    }
}

fn gen_random<'mir, 'tcx>(
    this: &mut MiriEvalContext<'mir, 'tcx>,
    len: usize,
    dest: Scalar<Tag>,
) -> InterpResult<'tcx>  {
    if len == 0 {
        // Nothing to do
        return Ok(());
    }
    let ptr = dest.to_ptr()?;

    let data = match &mut this.memory_mut().extra.rng {
        Some(rng) => {
            let mut data = vec![0; len];
            rng.fill_bytes(&mut data);
            data
        }
        None => {
            return err!(Unimplemented(
                "miri does not support gathering system entropy in deterministic mode!
                Use '-Zmiri-seed=<seed>' to enable random number generation.
                WARNING: Miri does *not* generate cryptographically secure entropy -
                do not use Miri to run any program that needs secure random number generation".to_owned(),
            ));
        }
    };
    let tcx = &{this.tcx.tcx};
    this.memory_mut().get_mut(ptr.alloc_id)?
        .write_bytes(tcx, ptr, &data)
}
