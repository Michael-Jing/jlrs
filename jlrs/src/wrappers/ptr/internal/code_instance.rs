//! Wrapper for `CodeInstance`.
//!
//! The documentation for this module has been slightly adapted from the comments for this struct
//! in [`julia.h`]
//!
//! [`julia.h`]: https://github.com/JuliaLang/julia/blob/96786e22ccabfdafd073122abb1fb69cea921e17/src/julia.h#L273

use crate::{
    impl_julia_typecheck,
    memory::output::Output,
    private::Private,
    wrappers::ptr::{
        internal::method_instance::MethodInstanceRef, private::WrapperPriv, value::ValueRef, Ref,
    },
};
use cfg_if::cfg_if;
use jl_sys::{jl_code_instance_t, jl_code_instance_type};
use std::{ffi::c_void, marker::PhantomData, ptr::NonNull};

cfg_if! {
    if #[cfg(any(not(feature = "lts"), feature = "all-features-override"))] {
        use std::{sync::atomic::Ordering, ptr::null_mut};
    }
}

/// A `CodeInstance` represents an executable operation.
#[derive(Copy, Clone)]
#[repr(transparent)]
pub struct CodeInstance<'scope>(NonNull<jl_code_instance_t>, PhantomData<&'scope ()>);

impl<'scope> CodeInstance<'scope> {
    /*
    for (a, b) in zip(fieldnames(Core.CodeInstance), fieldtypes(Core.CodeInstance))
        println(a, ": ", b)
    end
    def: Core.MethodInstance
    next: Core.CodeInstance
    min_world: UInt64
    max_world: UInt64
    rettype: Any
    rettype_const: Any
    inferred: Any
    ipo_purity_bits: UInt32
    purity_bits: UInt32
    argescapes: Any
    isspecsig: Bool
    precompile: Bool _Atomic
    invoke: Ptr{Nothing} _Atomic
    specptr: Ptr{Nothing} _Atomic
    relocatability: UInt8
    */

    /// Method this instance is specialized from.
    pub fn def(self) -> MethodInstanceRef<'scope> {
        // Safety: the pointer points to valid data
        unsafe { MethodInstanceRef::wrap(self.unwrap_non_null(Private).as_ref().def) }
    }

    /// Next cache entry.
    pub fn next(self) -> CodeInstanceRef<'scope> {
        cfg_if! {
            if #[cfg(all(feature = "lts", not(feature = "all-features-override")))] {
                // Safety: the pointer points to valid data
                unsafe { CodeInstanceRef::wrap(self.unwrap_non_null(Private).as_ref().next) }
            } else {
                // Safety: the pointer points to valid data
                unsafe {
                    let next = self.unwrap_non_null(Private).as_ref().next.load(Ordering::Relaxed);
                    CodeInstanceRef::wrap(next)
                }
            }
        }
    }

    /// Returns the minimum of the world range for which this object is valid to use.
    pub fn min_world(self) -> usize {
        // Safety: the pointer points to valid data
        unsafe { self.unwrap_non_null(Private).as_ref().min_world }
    }

    /// Returns the maximum of the world range for which this object is valid to use.
    pub fn max_world(self) -> usize {
        // Safety: the pointer points to valid data
        unsafe { self.unwrap_non_null(Private).as_ref().max_world }
    }

    /// Return type for fptr.
    pub fn rettype(self) -> ValueRef<'scope, 'static> {
        // Safety: the pointer points to valid data
        unsafe { ValueRef::wrap(self.unwrap_non_null(Private).as_ref().rettype) }
    }

    /// Inferred constant return value, or null
    pub fn rettype_const(self) -> ValueRef<'scope, 'static> {
        // Safety: the pointer points to valid data
        unsafe { ValueRef::wrap(self.unwrap_non_null(Private).as_ref().rettype_const) }
    }

    /// Inferred `CodeInfo`, `Nothing`, or `None`.
    pub fn inferred(self) -> ValueRef<'scope, 'static> {
        // Safety: the pointer points to valid data
        cfg_if! {
            if #[cfg(any(not(feature = "nightly"), feature = "all-features-override"))] {
                unsafe { ValueRef::wrap(self.unwrap_non_null(Private).as_ref().inferred) }
            } else {
                // Safety: the pointer points to valid data
                unsafe {
                    let inferred = self.unwrap_non_null(Private).as_ref().inferred.load(Ordering::Relaxed);
                    ValueRef::wrap(inferred)
                }
            }
        }
    }

    /// The `ipo_purity_bits` field of this `CodeInstance`.
    #[cfg(any(not(feature = "lts"), feature = "all-features-override"))]
    pub fn ipo_purity_bits(self) -> u32 {
        // Safety: the pointer points to valid data
        unsafe { self.unwrap_non_null(Private).as_ref().ipo_purity_bits }
    }

    /// The `purity_bits` field of this `CodeInstance`.
    #[cfg(any(not(feature = "lts"), feature = "all-features-override"))]
    pub fn purity_bits(self) -> u32 {
        // Safety: the pointer points to valid data
        #[cfg(feature = "nightly")]
        unsafe {
            self.unwrap_non_null(Private)
                .as_ref()
                .purity_bits
                .load(Ordering::Relaxed)
        }
        #[cfg(not(feature = "nightly"))]
        unsafe {
            self.unwrap_non_null(Private).as_ref().purity_bits
        }
    }

    /// Method this instance is specialized from.
    #[cfg(any(not(feature = "lts"), feature = "all-features-override"))]
    pub fn argescapes(self) -> ValueRef<'scope, 'static> {
        // Safety: the pointer points to valid data
        unsafe { ValueRef::wrap(self.unwrap_non_null(Private).as_ref().argescapes) }
    }

    /// If `specptr` is a specialized function signature for specTypes->rettype
    pub fn is_specsig(self) -> bool {
        // Safety: the pointer points to valid data
        unsafe { self.unwrap_non_null(Private).as_ref().isspecsig != 0 }
    }

    /// If `specptr` is a specialized function signature for specTypes->rettype
    pub fn precompile(self) -> bool {
        cfg_if! {
            if #[cfg(all(feature = "lts", not(feature = "all-features-override")))] {
                // Safety: the pointer points to valid data
                unsafe { self.unwrap_non_null(Private).as_ref().precompile != 0 }
            } else {
                // Safety: the pointer points to valid data
                unsafe {
                    self.unwrap_non_null(Private).as_ref().precompile.load(Ordering::SeqCst) != 0
                }
            }
        }
    }

    /// jlcall entry point
    pub fn invoke(self) -> *mut c_void {
        cfg_if! {
            if #[cfg(all(feature = "lts", not(feature = "all-features-override")))] {
                use std::ptr::null_mut;
                // Safety: the pointer points to valid data
                unsafe { self.unwrap_non_null(Private).as_ref().invoke.map(|x| x as *mut c_void).unwrap_or(null_mut()) }
            } else {
                // Safety: the pointer points to valid data
                unsafe {
                    self.unwrap_non_null(Private).as_ref().invoke.load(Ordering::Relaxed).map(|x| x as *mut c_void).unwrap_or(null_mut())
                }
            }
        }
    }

    /// private data for `jlcall entry point
    pub fn specptr(self) -> *mut c_void {
        cfg_if! {
            if #[cfg(all(feature = "lts", not(feature = "all-features-override")))] {
                // Safety: the pointer points to valid data
                unsafe { self.unwrap_non_null(Private).as_ref().specptr.fptr }
            } else {
                // Safety: the pointer points to valid data
                unsafe {
                    self.unwrap_non_null(Private).as_ref().specptr.fptr.load(Ordering::Relaxed)
                }
            }
        }
    }

    /// nonzero if all roots are built into sysimg or tagged by module key
    #[cfg(any(not(feature = "lts"), feature = "all-features-override"))]
    pub fn relocatability(self) -> u8 {
        // Safety: the pointer points to valid data
        unsafe { self.unwrap_non_null(Private).as_ref().relocatability }
    }

    /// Use the `Output` to extend the lifetime of this data.
    pub fn root<'target>(self, output: Output<'target>) -> CodeInstance<'target> {
        // Safety: the pointer points to valid data
        unsafe {
            let ptr = self.unwrap_non_null(Private);
            output.set_root::<CodeInstance>(ptr);
            CodeInstance::wrap_non_null(ptr, Private)
        }
    }
}

impl_julia_typecheck!(CodeInstance<'scope>, jl_code_instance_type, 'scope);
impl_debug!(CodeInstance<'_>);

impl<'scope> WrapperPriv<'scope, '_> for CodeInstance<'scope> {
    type Wraps = jl_code_instance_t;
    const NAME: &'static str = "CodeInstance";

    // Safety: `inner` must not have been freed yet, the result must never be
    // used after the GC might have freed it.
    unsafe fn wrap_non_null(inner: NonNull<Self::Wraps>, _: Private) -> Self {
        Self(inner, ::std::marker::PhantomData)
    }

    fn unwrap_non_null(self, _: Private) -> NonNull<Self::Wraps> {
        self.0
    }
}

impl_root!(CodeInstance, 1);

/// A reference to a [`CodeInstance`] that has not been explicitly rooted.
pub type CodeInstanceRef<'scope> = Ref<'scope, 'static, CodeInstance<'scope>>;
impl_valid_layout!(CodeInstanceRef, CodeInstance);
impl_ref_root!(CodeInstance, CodeInstanceRef, 1);