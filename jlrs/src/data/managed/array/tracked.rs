//! Track arrays to make directly accessing their content safer.

use std::{
    marker::PhantomData,
    mem::{self, ManuallyDrop},
    ops::{Deref, Range},
};

use jlrs_macros::julia_version;

use super::{
    data::{
        accessor::{
            BitsArrayAccessorI, BitsArrayAccessorMut, IndeterminateArrayAccessorI,
            IndeterminateArrayAccessorMut, InlinePtrArrayAccessorI, InlinePtrArrayAccessorMut,
            PtrArrayAccessorI, PtrArrayAccessorMut, UnionArrayAccessorI, UnionArrayAccessorMut,
        },
        copied::CopiedArray,
    },
    dimensions::{ArrayDimensions, Dims},
    Array, ArrayData, TypedArray, TypedArrayData,
};
#[julia_version(windows_lts = false)]
use super::{ArrayResult, TypedArrayResult};
use crate::{
    convert::unbox::Unbox,
    data::{
        layout::valid_layout::ValidField,
        managed::{value::ValueRef, ManagedRef},
    },
    error::JlrsResult,
    memory::{
        context::ledger::Ledger,
        target::{ExtendedTarget, Target},
    },
};

// TODO: make method, not trait
pub trait TrackArray<'scope, 'data>: Copy {
    /// Track this array.
    ///
    /// While an array is tracked, it can't be mutably tracked.
    fn track<'borrow>(&'borrow self) -> JlrsResult<TrackedArray<'borrow, 'scope, 'data, Self>>;

    /// Mutably track this array.
    ///
    /// While an array is mutably tracked, it can't be tracked otherwise.
    fn track_mut<'borrow>(
        &'borrow mut self,
    ) -> JlrsResult<TrackedArrayMut<'borrow, 'scope, 'data, Self>>;

    #[doc(hidden)]
    fn data_range(&self) -> Range<*const u8>;
}

impl<'scope, 'data> TrackArray<'scope, 'data> for Array<'scope, 'data> {
    fn track<'borrow>(&'borrow self) -> JlrsResult<TrackedArray<'borrow, 'scope, 'data, Self>> {
        Ledger::try_borrow(self.data_range())?;
        unsafe { Ok(TrackedArray::new(self)) }
    }

    fn track_mut<'borrow>(
        &'borrow mut self,
    ) -> JlrsResult<TrackedArrayMut<'borrow, 'scope, 'data, Self>> {
        Ledger::try_borrow_mut(self.data_range())?;
        unsafe { Ok(TrackedArrayMut::new(self)) }
    }

    fn data_range(&self) -> Range<*const u8> {
        let ptr = self.data_ptr().cast();

        unsafe {
            let n_bytes = self.element_size() * self.dimensions().size();
            ptr..ptr.add(n_bytes)
        }
    }
}

impl<'scope, 'data, U: ValidField> TrackArray<'scope, 'data> for TypedArray<'scope, 'data, U> {
    fn track<'borrow>(&'borrow self) -> JlrsResult<TrackedArray<'borrow, 'scope, 'data, Self>> {
        Ledger::try_borrow(self.data_range())?;
        unsafe { Ok(TrackedArray::new(self)) }
    }

    fn track_mut<'borrow>(
        &'borrow mut self,
    ) -> JlrsResult<TrackedArrayMut<'borrow, 'scope, 'data, Self>> {
        Ledger::try_borrow_mut(self.data_range())?;
        unsafe { Ok(TrackedArrayMut::new(self)) }
    }

    fn data_range(&self) -> Range<*const u8> {
        let arr = self.as_array();
        let ptr = arr.data_ptr().cast();

        unsafe {
            let n_bytes = arr.element_size() * arr.dimensions().size();
            ptr..ptr.add(n_bytes)
        }
    }
}

/// An array that has been tracked immutably.
pub struct TrackedArray<'tracked, 'scope, 'data, T>
where
    T: TrackArray<'scope, 'data>,
{
    data: T,
    _scope: PhantomData<&'scope ()>,
    _tracked: PhantomData<&'tracked ()>,
    _data: PhantomData<&'data ()>,
}

impl<'tracked, 'scope, 'data, T> Clone for TrackedArray<'tracked, 'scope, 'data, T>
where
    T: TrackArray<'scope, 'data>,
{
    fn clone(&self) -> Self {
        unsafe {
            Ledger::clone_shared(self.data.data_range());
            Self::new_from_owned(self.data)
        }
    }
}

impl<'tracked, 'scope, 'data, T> TrackedArray<'tracked, 'scope, 'data, T>
where
    T: TrackArray<'scope, 'data>,
{
    pub(crate) unsafe fn new(data: &'tracked T) -> Self {
        TrackedArray {
            data: *data,
            _scope: PhantomData,
            _tracked: PhantomData,
            _data: PhantomData,
        }
    }

    pub(crate) unsafe fn new_from_owned(data: T) -> Self {
        TrackedArray {
            data: data,
            _scope: PhantomData,
            _tracked: PhantomData,
            _data: PhantomData,
        }
    }
}

impl<'tracked, 'scope, 'data> TrackedArray<'tracked, 'scope, 'data, Array<'scope, 'data>> {
    /// Returns the dimensions of the tracked array.
    pub fn dimensions<'borrow>(&'borrow self) -> ArrayDimensions<'borrow> {
        unsafe { self.data.dimensions() }
    }

    /// Try to reborrow the array with the provided element type.
    pub fn try_as_typed<T>(
        self,
    ) -> JlrsResult<TrackedArray<'tracked, 'scope, 'data, TypedArray<'scope, 'data, T>>>
    where
        T: ValidField,
    {
        let data = self.data.try_as_typed::<T>()?;
        let ret = unsafe { Ok(TrackedArray::new_from_owned(data)) };
        mem::forget(self);
        ret
    }

    /// Reborrow the array with the provided element type without checking if this conversion is valid.
    pub unsafe fn as_typed_unchecked<T>(
        self,
    ) -> TrackedArray<'tracked, 'scope, 'data, TypedArray<'scope, 'data, T>>
    where
        T: ValidField,
    {
        let data = self.data.as_typed_unchecked::<T>();
        let ret = TrackedArray::new_from_owned(data);
        mem::forget(self);
        ret
    }

    /// Copy the content of this array.
    pub fn copy_inline_data<T>(&self) -> JlrsResult<CopiedArray<T>>
    where
        T: 'static + ValidField + Unbox,
    {
        unsafe { self.data.copy_inline_data() }
    }

    /// Convert this array to a slice without checking if the layouts are compatible.
    pub unsafe fn as_slice_unchecked<'borrow, T>(&'borrow self) -> &'borrow [T] {
        self.data.as_slice_unchecked()
    }

    /// Create an accessor for the content of the array if the element type is an isbits type.
    pub fn bits_data<'borrow, T>(
        &'borrow self,
    ) -> JlrsResult<BitsArrayAccessorI<'borrow, 'scope, 'data, T>>
    where
        T: ValidField + 'static,
    {
        unsafe { self.data.bits_data() }
    }

    /// Create an accessor for the content of the array if the element type is stored inline, but
    /// can contain references to managed data.
    pub fn inline_data<'borrow, T>(
        &'borrow self,
    ) -> JlrsResult<InlinePtrArrayAccessorI<'borrow, 'scope, 'data, T>>
    where
        T: ValidField,
    {
        unsafe { self.data.inline_data() }
    }

    /// Create an accessor for the content of the array if the element type is a managed type.
    pub fn managed_data<'borrow, T>(
        &'borrow self,
    ) -> JlrsResult<PtrArrayAccessorI<'borrow, 'scope, 'data, T>>
    where
        T: ManagedRef<'scope, 'data>,
        Option<T>: ValidField,
    {
        unsafe { self.data.managed_data() }
    }

    /// Create an accessor for the content of the array if the element type is a non-inlined type
    /// (e.g. any mutable type).
    pub fn value_data<'borrow>(
        &'borrow self,
    ) -> JlrsResult<PtrArrayAccessorI<'borrow, 'scope, 'data, ValueRef<'scope, 'data>>> {
        unsafe { self.data.value_data() }
    }

    /// Create an accessor for the content of the array if the element type is a bits union, i.e.
    /// a union of bits types.
    pub fn union_data<'borrow>(
        &'borrow self,
    ) -> JlrsResult<UnionArrayAccessorI<'borrow, 'scope, 'data>> {
        unsafe { self.data.union_data() }
    }

    /// Create an accessor for the content of the array that makes no assumptions about the
    /// element type.
    pub fn indeterminate_data<'borrow>(
        &'borrow self,
    ) -> IndeterminateArrayAccessorI<'borrow, 'scope, 'data> {
        unsafe { self.data.indeterminate_data() }
    }

    #[julia_version(windows_lts = false)]
    /// Reshape the array.
    ///
    /// Returns a new array with the provided dimensions, the content of the array is shared with
    /// the original array. The old and new dimensions must have an equal number of elements.
    pub fn reshape<'target, 'current, 'borrow, D, S>(
        &self,
        target: ExtendedTarget<'target, '_, '_, S>,
        dims: D,
    ) -> ArrayResult<'target, 'data, S>
    where
        D: Dims,
        S: Target<'target>,
    {
        unsafe { self.data.reshape(target, dims) }
    }

    /// Reshape the array.
    ///
    /// Returns a new array with the provided dimensions, the content of the array is shared with
    /// the original array. The old and new dimensions must have an equal number of elements.
    ///
    /// Safety: if an exception is thrown it isn't caught.
    pub unsafe fn reshape_unchecked<'target, 'current, 'borrow, D, S>(
        &self,
        target: ExtendedTarget<'target, '_, '_, S>,
        dims: D,
    ) -> ArrayData<'target, 'data, S>
    where
        D: Dims,
        S: Target<'target>,
    {
        self.data.reshape_unchecked(target, dims)
    }
}

impl<'tracked, 'scope, 'data, T> TrackedArray<'tracked, 'scope, 'data, TypedArray<'scope, 'data, T>>
where
    T: ValidField,
{
    /// Returns the dimensions of the tracked array.
    pub fn dimensions<'borrow>(&'borrow self) -> ArrayDimensions<'borrow> {
        unsafe { self.data.dimensions() }
    }

    /// Copy the content of this array.
    pub fn copy_inline_data(&self) -> JlrsResult<CopiedArray<T>>
    where
        T: 'static,
    {
        unsafe { self.data.copy_inline_data() }
    }

    /// Convert this array to a slice.
    pub fn as_slice<'borrow>(&'borrow self) -> &'borrow [T] {
        unsafe {
            let arr = std::mem::transmute::<&'borrow Self, &'borrow Array>(self);
            arr.as_slice_unchecked()
        }
    }

    /// Create an accessor for the content of the array if the element type is an isbits type.
    pub fn bits_data<'borrow>(
        &'borrow self,
    ) -> JlrsResult<BitsArrayAccessorI<'borrow, 'scope, 'data, T>> {
        unsafe { self.data.bits_data() }
    }

    /// Create an accessor for the content of the array if the element type is stored inline, but
    /// can contain references to managed data.
    pub fn inline_data<'borrow>(
        &'borrow self,
    ) -> JlrsResult<InlinePtrArrayAccessorI<'borrow, 'scope, 'data, T>> {
        unsafe { self.data.inline_data() }
    }

    /// Create an accessor for the content of the array that makes no assumptions about the
    /// element type.
    pub fn indeterminate_data<'borrow>(
        &'borrow self,
    ) -> IndeterminateArrayAccessorI<'borrow, 'scope, 'data> {
        unsafe { self.data.indeterminate_data() }
    }

    #[julia_version(windows_lts = false)]
    /// Reshape the array.
    ///
    /// Returns a new array with the provided dimensions, the content of the array is shared with
    /// the original array. The old and new dimensions must have an equal number of elements.
    pub fn reshape<'target, 'current, 'borrow, D, S>(
        &self,
        target: ExtendedTarget<'target, '_, '_, S>,
        dims: D,
    ) -> TypedArrayResult<'target, 'data, S, T>
    where
        D: Dims,
        S: Target<'target>,
    {
        unsafe { self.data.reshape(target, dims) }
    }

    /// Reshape the array.
    ///
    /// Returns a new array with the provided dimensions, the content of the array is shared with
    /// the original array. The old and new dimensions must have an equal number of elements.
    ///
    /// Safety: if an exception is thrown it isn't caught.
    pub unsafe fn reshape_unchecked<'target, 'current, 'borrow, D, S>(
        &self,
        target: ExtendedTarget<'target, '_, '_, S>,
        dims: D,
    ) -> TypedArrayData<'target, 'data, S, T>
    where
        D: Dims,
        S: Target<'target>,
    {
        self.data.reshape_unchecked(target, dims)
    }
}

impl<'tracked, 'scope, 'data, T>
    TrackedArray<'tracked, 'scope, 'data, TypedArray<'scope, 'data, Option<T>>>
where
    T: ManagedRef<'scope, 'data>,
    Option<T>: ValidField,
{
    /// Create an accessor for the content of the array if the element type is a managed type.
    pub fn managed_data<'borrow>(
        &'borrow self,
    ) -> JlrsResult<PtrArrayAccessorI<'borrow, 'scope, 'data, T>> {
        unsafe { self.data.managed_data() }
    }

    /// Create an accessor for the content of the array if the element type is a non-inlined type
    /// (e.g. any mutable type).
    pub fn value_data<'borrow>(
        &'borrow self,
    ) -> JlrsResult<PtrArrayAccessorI<'borrow, 'scope, 'data, ValueRef<'scope, 'data>>> {
        unsafe { self.data.value_data() }
    }
}

impl<'scope, 'data, T: TrackArray<'scope, 'data>> Drop for TrackedArray<'_, 'scope, 'data, T> {
    fn drop(&mut self) {
        Ledger::unborrow_shared(self.data.data_range());
    }
}

pub struct TrackedArrayMut<'tracked, 'scope, 'data, T>
where
    T: TrackArray<'scope, 'data>,
{
    tracked: ManuallyDrop<TrackedArray<'tracked, 'scope, 'data, T>>,
}

impl<'tracked, 'scope, 'data, T> TrackedArrayMut<'tracked, 'scope, 'data, T>
where
    T: TrackArray<'scope, 'data>,
{
    pub(crate) unsafe fn new(data: &'tracked mut T) -> Self {
        TrackedArrayMut {
            tracked: ManuallyDrop::new(TrackedArray::new(data)),
        }
    }
}

impl<'tracked, 'scope, 'data> TrackedArrayMut<'tracked, 'scope, 'data, Array<'scope, 'data>> {
    /// Create a mutable accessor for the content of the array if the element type is an isbits
    /// type.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn bits_data_mut<'borrow, T>(
        &'borrow mut self,
    ) -> JlrsResult<BitsArrayAccessorMut<'borrow, 'scope, 'data, T>>
    where
        T: ValidField,
    {
        self.tracked.data.bits_data_mut()
    }

    /// Create a mutable accessor for the content of the array if the element type is stored
    /// inline, but can contain references to managed data.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn inline_data_mut<'borrow, T>(
        &'borrow mut self,
    ) -> JlrsResult<InlinePtrArrayAccessorMut<'borrow, 'scope, 'data, T>>
    where
        T: ValidField,
    {
        self.tracked.data.inline_data_mut()
    }

    /// Create a mutable accessor for the content of the array if the element type is a managed
    /// type.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn managed_data_mut<'borrow, T>(
        &'borrow mut self,
    ) -> JlrsResult<PtrArrayAccessorMut<'borrow, 'scope, 'data, T>>
    where
        T: ManagedRef<'scope, 'data>,
        Option<T>: ValidField,
    {
        self.tracked.data.managed_data_mut()
    }

    /// Create a mutable accessor for the content of the array if the element type is a
    /// non-inlined type (e.g. any mutable type).
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn value_data_mut<'borrow>(
        &'borrow mut self,
    ) -> JlrsResult<PtrArrayAccessorMut<'borrow, 'scope, 'data, ValueRef<'scope, 'data>>> {
        self.tracked.data.value_data_mut()
    }

    /// Create a mutable accessor for the content of the array if the element type is a bits
    /// union, i.e. a union of bits types.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn union_data_mut<'borrow>(
        &'borrow mut self,
    ) -> JlrsResult<UnionArrayAccessorMut<'borrow, 'scope, 'data>> {
        self.tracked.data.union_data_mut()
    }

    /// Create a mutable accessor for the content of the array that makes no assumptions about the
    /// element type.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn indeterminate_data_mut<'borrow>(
        &'borrow mut self,
    ) -> IndeterminateArrayAccessorMut<'borrow, 'scope, 'data> {
        self.tracked.data.indeterminate_data_mut()
    }

    /// Convert this array to a mutable slice without checking if the layouts are compatible.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn as_mut_slice_unchecked<'borrow, T>(&'borrow mut self) -> &'borrow mut [T]
    where
        T: 'static,
    {
        self.tracked.data.as_mut_slice_unchecked()
    }
}

impl<'tracked, 'scope> TrackedArrayMut<'tracked, 'scope, 'static, Array<'scope, 'static>> {
    #[julia_version(windows_lts = false)]
    /// Create a mutable accessor for the content of the array if the element type is a managed
    /// type.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn grow_end<'target, S>(
        &mut self,
        target: S,
        inc: usize,
    ) -> S::Exception<'static, ()>
    where
        S: Target<'target>,
    {
        let current_range = self.tracked.data.data_range();
        let res = self.tracked.data.grow_end(target, inc);
        let new_range = self.tracked.data.data_range();
        Ledger::replace_borrow_mut(current_range, new_range);
        res
    }

    /// Add capacity for `inc` more elements at the end of the array. The array must be
    /// one-dimensional. If the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented. If an
    /// exception is thrown, it isn't caught.
    pub unsafe fn grow_end_unchecked(&mut self, inc: usize) {
        let current_range = self.tracked.data.data_range();
        self.tracked.data.grow_end_unchecked(inc);
        let new_range = self.tracked.data.data_range();
        Ledger::replace_borrow_mut(current_range, new_range);
    }

    #[julia_version(windows_lts = false)]
    /// Remove `dec` elements from the end of the array.  The array must be one-dimensional. If
    /// the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn del_end<'target, S>(&mut self, target: S, dec: usize) -> S::Exception<'static, ()>
    where
        S: Target<'target>,
    {
        let current_range = self.tracked.data.data_range();
        let res = self.tracked.data.del_end(target, dec);
        let new_range = self.tracked.data.data_range();
        Ledger::replace_borrow_mut(current_range, new_range);
        res
    }

    /// Remove `dec` elements from the end of the array.  The array must be one-dimensional. If
    /// the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented. If an
    /// exception is thrown, it isn't caught.
    pub unsafe fn del_end_unchecked(&mut self, dec: usize) {
        let current_range = self.tracked.data.data_range();
        self.tracked.data.del_end_unchecked(dec);
        let new_range = self.tracked.data.data_range();
        Ledger::replace_borrow_mut(current_range, new_range);
    }

    #[julia_version(windows_lts = false)]
    /// Add capacity for `inc` more elements at the start of the array. The array must be
    /// one-dimensional. If the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn grow_begin<'target, S>(
        &mut self,
        target: S,
        inc: usize,
    ) -> S::Exception<'static, ()>
    where
        S: Target<'target>,
    {
        let current_range = self.tracked.data.data_range();
        let res = self.tracked.data.grow_begin(target, inc);
        let new_range = self.tracked.data.data_range();
        Ledger::replace_borrow_mut(current_range, new_range);
        res
    }

    /// Add capacity for `inc` more elements at the start of the array. The array must be
    /// one-dimensional. If the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented. If an
    /// exception is thrown, it isn't caught.
    pub unsafe fn grow_begin_unchecked(&mut self, inc: usize) {
        let current_range = self.tracked.data.data_range();
        self.tracked.data.grow_begin_unchecked(inc);
        let new_range = self.tracked.data.data_range();
        Ledger::replace_borrow_mut(current_range, new_range);
    }

    #[julia_version(windows_lts = false)]
    /// Remove `dec` elements from the start of the array.  The array must be one-dimensional. If
    /// the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn del_begin<'target, S>(
        &mut self,
        target: S,
        dec: usize,
    ) -> S::Exception<'static, ()>
    where
        S: Target<'target>,
    {
        let current_range = self.tracked.data.data_range();
        let res = self.tracked.data.del_begin(target, dec);
        let new_range = self.tracked.data.data_range();
        Ledger::replace_borrow_mut(current_range, new_range);
        res
    }

    /// Remove `dec` elements from the start of the array.  The array must be one-dimensional. If
    /// the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented. If an
    /// exception is thrown, it isn't caught.
    pub unsafe fn del_begin_unchecked(&mut self, dec: usize) {
        let current_range = self.tracked.data.data_range();
        self.tracked.data.del_begin_unchecked(dec);
        let new_range = self.tracked.data.data_range();
        Ledger::replace_borrow_mut(current_range, new_range);
    }
}

impl<'tracked, 'scope, 'data, T>
    TrackedArrayMut<'tracked, 'scope, 'data, TypedArray<'scope, 'data, T>>
where
    T: ValidField,
{
    /// Convert this array to a slice.
    pub fn as_mut_slice<'borrow>(&'borrow mut self) -> &'borrow mut [T] {
        unsafe {
            let arr = std::mem::transmute::<&'borrow mut Self, &'borrow mut Array>(self);
            arr.as_mut_slice_unchecked()
        }
    }

    /// Create a mutable accessor for the content of the array if the element type is an isbits
    /// type.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn bits_data_mut<'borrow>(
        &'borrow mut self,
    ) -> JlrsResult<BitsArrayAccessorMut<'borrow, 'scope, 'data, T>> {
        self.tracked.data.bits_data_mut()
    }

    /// Create a mutable accessor for the content of the array if the element type is stored
    /// inline, but can contain references to managed data.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn inline_data_mut<'borrow>(
        &'borrow mut self,
    ) -> JlrsResult<InlinePtrArrayAccessorMut<'borrow, 'scope, 'data, T>> {
        self.tracked.data.inline_data_mut()
    }

    /// Create a mutable accessor for the content of the array that makes no assumptions about the
    /// element type.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn indeterminate_data_mut<'borrow>(
        &'borrow mut self,
    ) -> IndeterminateArrayAccessorMut<'borrow, 'scope, 'data> {
        self.tracked.data.indeterminate_data_mut()
    }
}

impl<'tracked, 'scope, 'data, T>
    TrackedArrayMut<'tracked, 'scope, 'data, TypedArray<'scope, 'data, Option<T>>>
where
    T: ManagedRef<'scope, 'data>,
    Option<T>: ValidField,
{
    /// Create a mutable accessor for the content of the array if the element type is a managed
    /// type.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn managed_data_mut<'borrow>(
        &'borrow mut self,
    ) -> JlrsResult<PtrArrayAccessorMut<'borrow, 'scope, 'data, T>> {
        self.tracked.data.managed_data_mut()
    }

    /// Create a mutable accessor for the content of the array if the element type is a
    /// non-inlined type (e.g. any mutable type).
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn value_data_mut<'borrow>(
        &'borrow mut self,
    ) -> JlrsResult<PtrArrayAccessorMut<'borrow, 'scope, 'data, ValueRef<'scope, 'data>>> {
        self.tracked.data.value_data_mut()
    }
}

impl<'tracked, 'scope, T> TrackedArrayMut<'tracked, 'scope, 'static, TypedArray<'scope, 'static, T>>
where
    T: ValidField,
{
    #[julia_version(windows_lts = false)]
    /// Add capacity for `inc` more elements at the end of the array. The array must be
    /// one-dimensional. If the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn grow_end<'target, S>(
        &mut self,
        target: S,
        inc: usize,
    ) -> S::Exception<'static, ()>
    where
        S: Target<'target>,
    {
        self.tracked.data.grow_end(target, inc)
    }

    /// Add capacity for `inc` more elements at the end of the array. The array must be
    /// one-dimensional. If the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented. If an
    /// exception is thrown, it isn't caught.
    pub unsafe fn grow_end_unchecked(&mut self, inc: usize) {
        self.tracked.data.grow_end_unchecked(inc)
    }

    #[julia_version(windows_lts = false)]
    /// Remove `dec` elements from the end of the array.  The array must be one-dimensional. If
    /// the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn del_end<'target, S>(&mut self, target: S, dec: usize) -> S::Exception<'static, ()>
    where
        S: Target<'target>,
    {
        self.tracked.data.del_end(target, dec)
    }

    /// Remove `dec` elements from the end of the array.  The array must be one-dimensional. If
    /// the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented. If an
    /// exception is thrown, it isn't caught.
    pub unsafe fn del_end_unchecked(&mut self, dec: usize) {
        self.tracked.data.del_end_unchecked(dec)
    }

    #[julia_version(windows_lts = false)]
    /// Add capacity for `inc` more elements at the start of the array. The array must be
    /// one-dimensional. If the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn grow_begin<'target, S>(
        &mut self,
        target: S,
        inc: usize,
    ) -> S::Exception<'static, ()>
    where
        S: Target<'target>,
    {
        self.tracked.data.grow_begin(target, inc)
    }

    /// Add capacity for `inc` more elements at the start of the array. The array must be
    /// one-dimensional. If the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented. If an
    /// exception is thrown, it isn't caught.
    pub unsafe fn grow_begin_unchecked(&mut self, inc: usize) {
        self.tracked.data.grow_begin_unchecked(inc)
    }

    #[julia_version(windows_lts = false)]
    /// Remove `dec` elements from the start of the array.  The array must be one-dimensional. If
    /// the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented.
    pub unsafe fn del_begin<'target, S>(
        &mut self,
        target: S,
        dec: usize,
    ) -> S::Exception<'static, ()>
    where
        S: Target<'target>,
    {
        self.tracked.data.del_begin(target, dec)
    }

    /// Remove `dec` elements from the start of the array.  The array must be one-dimensional. If
    /// the array isn't one-dimensional an exception is thrown.
    ///
    /// Safety: Mutating things that should absolutely not be mutated is not prevented. If an
    /// exception is thrown, it isn't caught.
    pub unsafe fn del_begin_unchecked(&mut self, dec: usize) {
        self.tracked.data.del_begin_unchecked(dec)
    }
}

impl<'tracked, 'scope, 'data> Deref
    for TrackedArrayMut<'tracked, 'scope, 'data, Array<'scope, 'data>>
{
    type Target = TrackedArray<'tracked, 'scope, 'data, Array<'scope, 'data>>;

    fn deref(&self) -> &Self::Target {
        &self.tracked
    }
}

impl<'tracked, 'scope, 'data, T> Deref
    for TrackedArrayMut<'tracked, 'scope, 'data, TypedArray<'scope, 'data, T>>
where
    T: ValidField,
{
    type Target = TrackedArray<'tracked, 'scope, 'data, TypedArray<'scope, 'data, T>>;

    fn deref(&self) -> &Self::Target {
        &self.tracked
    }
}

impl<'tracked, 'scope, 'data, T> Drop for TrackedArrayMut<'tracked, 'scope, 'data, T>
where
    T: TrackArray<'scope, 'data>,
{
    fn drop(&mut self) {
        Ledger::unborrow_owned(self.tracked.data.data_range());
    }
}
